use anyhow::{Context, Result, bail};
#[cfg(feature = "kv-external")]
use std::time::Duration;
use std::{fs, path::Path};

use fjall::{
    CompressionType as FjallCompression, Database as FjallDb, KeyspaceCreateOptions,
    config::CompressionPolicy as FjallCompressionPolicy,
};
use heed::{
    CommitOrAbort, Database as HeedDb, Env as HeedEnv, EnvFlags, EnvOpenOptions, types::Bytes,
};
use manifold::column_family::ColumnFamilyDatabase as ManifoldDb;
use parity_db::{Db as ParityDb, Options as ParityOptions};
use redb::Database as RedbDb;
use surrealkv::{
    CompressionType as SurrealCompression, Options as SurrealOptions, Tree as SurrealTree,
    TreeBuilder as SurrealTreeBuilder,
};
use turbokv::{Compression as TurboCompression, Db as TurboDb, DbOptions as TurboOptions};

#[cfg(feature = "kv-external")]
use libmdbx::{
    Database as MdbxDb, DatabaseOptions as MdbxOptions, Mode as MdbxMode, NoWriteMap,
    ReadWriteOptions as MdbxReadWriteOptions, SyncMode as MdbxSyncMode,
};
#[cfg(feature = "kv-external")]
use persy::{ByteVec as PersyByteVec, Config as PersyConfig, Persy, ValueMode as PersyValueMode};
#[cfg(feature = "kv-external")]
use rocksdb::{DB as RocksDb, DBCompressionType as RocksCompression, Options as RocksOptions};

#[cfg(feature = "kv-experimental")]
use jammdb::DB as JammDb;
#[cfg(feature = "kv-experimental")]
use lsm_db::Lsm as LsmDb;
#[cfg(feature = "kv-experimental")]
use roughdb::{
    CompressionType as RoughCompression, Db as RoughDb, OpenMode as RoughOpenMode,
    Options as RoughOptions,
};

use super::{
    backend_ops::{
        BackendOps, FjallHandle, HeedHandle, ManifoldHandle, ParityHandle, RetryMode, TurboHandle,
    },
    engine_policy::compression_override_supported,
    types::{Durability, EngineCompression, EngineKind},
};

#[expect(
    unsafe_code,
    reason = "heed requires an unsafe environment open after configuring the map and database limits"
)]
fn open_heed_environment(dir: &Path, durability: Durability) -> Result<HeedEnv> {
    let mut options = EnvOpenOptions::new();
    options.map_size(16usize * 1024 * 1024 * 1024).max_dbs(1);
    if durability == Durability::Relaxed {
        // SAFETY: NO_SYNC only changes LMDB's durability policy. This benchmark owns
        // the environment directory exclusively for the lifetime of the handle.
        unsafe {
            options.flags(EnvFlags::NO_SYNC);
        }
    }
    // SAFETY: each invocation owns `dir`, creates it before opening, and keeps the
    // resulting environment alive until all database handles using it are dropped.
    unsafe { Ok(options.open(dir)?) }
}

pub(super) enum Engine {
    Redb(RedbDb),
    Fjall(FjallHandle),
    Surrealkv(SurrealTree),
    Heed(HeedHandle),
    Sled(Box<sled::Db>),
    Lkv(lkv::Database),
    Manifold(ManifoldHandle<ManifoldDb>),
    Turbokv(Option<TurboHandle<TurboDb>>),
    Paritydb(ParityHandle<ParityDb>),
    #[cfg(feature = "kv-external")]
    Rocksdb(RocksDb),
    #[cfg(feature = "kv-external")]
    Mdbx(MdbxDb<NoWriteMap>),
    #[cfg(feature = "kv-external")]
    Persy(Persy),
    #[cfg(feature = "kv-experimental")]
    Roughdb(RoughDb),
    #[cfg(feature = "kv-experimental")]
    Jammdb(JammDb),
    #[cfg(feature = "kv-experimental")]
    Lsmdb(LsmDb),
}

impl Engine {
    pub(super) async fn open(
        kind: EngineKind,
        durability: Durability,
        path: &Path,
    ) -> Result<Self> {
        Self::open_with_compression(kind, durability, path, EngineCompression::Default).await
    }

    #[expect(
        clippy::too_many_lines,
        reason = "engine construction is one exhaustive backend dispatch table"
    )]
    pub(super) async fn open_with_compression(
        kind: EngineKind,
        durability: Durability,
        path: &Path,
        compression: EngineCompression,
    ) -> Result<Self> {
        if compression != EngineCompression::Default && !compression_override_supported(kind) {
            bail!(
                "engine {kind:?} does not expose a benchmarked compression override; use --engine-compression default"
            );
        }
        fs::create_dir_all(path)?;
        match kind {
            EngineKind::Redb => {
                let db_path = path.join("bench.redb");
                let db = if db_path.exists() {
                    RedbDb::open(&db_path)?
                } else {
                    RedbDb::create(&db_path)?
                };
                Ok(Self::Redb(db))
            }
            EngineKind::Fjall => {
                let mut builder = FjallDb::builder(path.join("fjall"));
                let keyspace_options = match compression {
                    EngineCompression::Default => KeyspaceCreateOptions::default(),
                    EngineCompression::None => {
                        builder = builder.journal_compression(FjallCompression::None);
                        KeyspaceCreateOptions::default()
                            .data_block_compression_policy(FjallCompressionPolicy::disabled())
                    }
                    EngineCompression::Lz4 => {
                        builder = builder.journal_compression(FjallCompression::Lz4);
                        KeyspaceCreateOptions::default().data_block_compression_policy(
                            FjallCompressionPolicy::all(FjallCompression::Lz4),
                        )
                    }
                    EngineCompression::Snappy | EngineCompression::Zstd => {
                        bail!("Fjall 3.1.12 exposes LZ4 but not the requested codec")
                    }
                };
                let db = builder.open()?;
                let keyspace = db.keyspace("kv", move || keyspace_options)?;
                Ok(Self::Fjall(FjallHandle { db, keyspace }))
            }
            EngineKind::Surrealkv => {
                let opts = match compression {
                    EngineCompression::Default => SurrealOptions::default(),
                    EngineCompression::None => SurrealOptions::default()
                        .with_compression_per_level(vec![SurrealCompression::None]),
                    EngineCompression::Snappy => SurrealOptions::default()
                        .with_compression_per_level(vec![SurrealCompression::SnappyCompression]),
                    EngineCompression::Zstd => SurrealOptions::default()
                        .with_compression_per_level(vec![SurrealCompression::ZstdCompression]),
                    EngineCompression::Lz4 => {
                        bail!("SurrealKV 1.0.0 SSTables do not expose LZ4 compression")
                    }
                };
                let tree = SurrealTreeBuilder::with_options(opts)
                    .with_path(path.join("surrealkv"))
                    .build()?;
                Ok(Self::Surrealkv(tree))
            }
            EngineKind::Heed => {
                let dir = path.join("heed");
                fs::create_dir_all(&dir)?;
                let env = open_heed_environment(&dir, durability)?;
                let wtxn = env.write_txn()?;
                let db: HeedDb<'static, Bytes, Bytes> = env
                    .create_databases_and_commit(wtxn, |opener, wtxn| {
                        opener
                            .create_database::<Bytes, Bytes>(wtxn, None)
                            .map(CommitOrAbort::<_, ()>::Commit)
                    })?
                    .unwrap_commit();
                Ok(Self::Heed(HeedHandle { env, db }))
            }
            EngineKind::Sled => {
                let db = sled::Config::new()
                    .path(path.join("sled"))
                    .flush_every_ms(None)
                    .open()?;
                Ok(Self::Sled(Box::new(db)))
            }
            EngineKind::Lkv => {
                if durability == Durability::Relaxed {
                    bail!("lkv relaxed mode unsupported");
                }
                let db_path = path.join("bench.lkv");
                let db = if db_path.exists() {
                    lkv::Database::open(&db_path)?
                } else {
                    lkv::Database::create(&db_path)?
                };
                Ok(Self::Lkv(db))
            }
            EngineKind::Manifold | EngineKind::ManifoldWal => {
                if durability == Durability::Relaxed {
                    bail!("Manifold configurations are sync-only in this benchmark");
                }
                let db = match kind {
                    EngineKind::Manifold => ManifoldDb::builder()
                        .without_wal()
                        .open(path.join("bench.manifold"))?,
                    EngineKind::ManifoldWal => {
                        ManifoldDb::builder().open(path.join("bench.manifold"))?
                    }
                    _ => unreachable!(),
                };
                let cf = db.column_family_or_create("kv")?;
                Ok(Self::Manifold(ManifoldHandle { owner: db, cf }))
            }
            EngineKind::Turbokv => {
                let opts = match durability {
                    Durability::Relaxed => TurboOptions::durable(),
                    Durability::Sync => TurboOptions::paranoid(),
                };
                let opts = match compression {
                    EngineCompression::Default => opts,
                    EngineCompression::None => opts.with_compression(TurboCompression::None),
                    EngineCompression::Lz4 => opts.with_compression(TurboCompression::Lz4),
                    EngineCompression::Snappy => opts.with_compression(TurboCompression::Snappy),
                    EngineCompression::Zstd => opts.with_compression(TurboCompression::Zstd),
                };
                Ok(Self::Turbokv(Some(TurboHandle(
                    TurboDb::open_with_options(path.join("turbokv"), opts).await?,
                ))))
            }
            EngineKind::ParitydbHash | EngineKind::ParitydbBtree => {
                if durability == Durability::Sync {
                    bail!("ParityDB 0.5.7 has no public durable-before-return commit API");
                }
                let dir = match kind {
                    EngineKind::ParitydbHash => path.join("paritydb-hash"),
                    EngineKind::ParitydbBtree => path.join("paritydb-btree"),
                    _ => unreachable!(),
                };
                let mut opts = ParityOptions::with_columns(&dir, 1);
                if matches!(kind, EngineKind::ParitydbBtree) {
                    opts.columns[0].btree_index = true;
                }
                let db = ParityDb::open_or_create(&opts)?;
                Ok(Self::Paritydb(ParityHandle {
                    db,
                    ordered: matches!(kind, EngineKind::ParitydbBtree),
                }))
            }
            #[cfg(feature = "kv-external")]
            EngineKind::Rocksdb => {
                let mut opts = RocksOptions::default();
                opts.create_if_missing(true);
                let codec = match compression {
                    EngineCompression::Default | EngineCompression::None => RocksCompression::None,
                    EngineCompression::Lz4 => RocksCompression::Lz4,
                    EngineCompression::Snappy => RocksCompression::Snappy,
                    EngineCompression::Zstd => RocksCompression::Zstd,
                };
                opts.set_compression_type(codec);
                Ok(Self::Rocksdb(RocksDb::open(&opts, path.join("rocksdb"))?))
            }
            #[cfg(feature = "kv-external")]
            EngineKind::Mdbx => {
                let sync_mode = match durability {
                    Durability::Relaxed => MdbxSyncMode::SafeNoSync,
                    Durability::Sync => MdbxSyncMode::Durable,
                };
                let opts = MdbxOptions {
                    mode: MdbxMode::ReadWrite(MdbxReadWriteOptions {
                        sync_mode,
                        ..Default::default()
                    }),
                    ..Default::default()
                };
                Ok(Self::Mdbx(MdbxDb::<NoWriteMap>::open_with_options(
                    path.join("mdbx"),
                    opts,
                )?))
            }
            #[cfg(feature = "kv-external")]
            EngineKind::Persy => {
                let mut config = PersyConfig::new();
                if let Some(raw) = std::env::var_os("DBBENCH_PERSY_LOCK_TIMEOUT_MS") {
                    let raw = raw.to_string_lossy();
                    let timeout_ms: u64 = raw.parse().with_context(|| {
                        format!("invalid DBBENCH_PERSY_LOCK_TIMEOUT_MS={raw:?}")
                    })?;
                    if timeout_ms == 0 {
                        bail!("DBBENCH_PERSY_LOCK_TIMEOUT_MS must be greater than zero");
                    }
                    config.change_transaction_lock_timeout(Duration::from_millis(timeout_ms));
                }
                let db = Persy::open_or_create_with(path.join("bench.persy"), config, |db| {
                    let mut tx = db.begin()?;
                    tx.create_index::<PersyByteVec, PersyByteVec>("kv", PersyValueMode::Replace)?;
                    tx.prepare()?.commit()?;
                    Ok(())
                })?;
                Ok(Self::Persy(db))
            }
            #[cfg(feature = "kv-experimental")]
            EngineKind::Roughdb => {
                let opts = RoughOptions {
                    open_mode: RoughOpenMode::CreateIfMissing,
                    compression: RoughCompression::NoCompression,
                    ..RoughOptions::default()
                };
                Ok(Self::Roughdb(RoughDb::open(path.join("roughdb"), opts)?))
            }
            #[cfg(feature = "kv-experimental")]
            EngineKind::Jammdb => {
                if durability == Durability::Relaxed {
                    bail!("jammdb relaxed mode unsupported");
                }
                let db = JammDb::open(path.join("bench.jammdb"))?;
                let tx = db.tx(true)?;
                let _ = tx.get_or_create_bucket("kv")?;
                tx.commit()?;
                Ok(Self::Jammdb(db))
            }
            #[cfg(feature = "kv-experimental")]
            EngineKind::Lsmdb => {
                if durability == Durability::Relaxed {
                    bail!("lsm-db relaxed mode unsupported in durable build");
                }
                Ok(Self::Lsmdb(LsmDb::open(path.join("lsmdb"))?))
            }
        }
    }

    pub(super) async fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        match self {
            Self::Redb(db) => db.read(key).await,
            Self::Fjall(db) => db.read(key).await,
            Self::Surrealkv(db) => db.read(key).await,
            Self::Heed(db) => db.read(key).await,
            Self::Sled(db) => db.read(key).await,
            Self::Manifold(db) => db.read(key).await,
            Self::Turbokv(db) => {
                db.as_mut()
                    .context("TurboKV already closed")?
                    .read(key)
                    .await
            }
            Self::Paritydb(db) => db.read(key).await,
            #[cfg(feature = "kv-external")]
            Self::Rocksdb(db) => db.read(key).await,
            #[cfg(feature = "kv-external")]
            Self::Mdbx(db) => db.read(key).await,
            #[cfg(feature = "kv-external")]
            Self::Persy(db) => db.read(key).await,
            #[cfg(feature = "kv-experimental")]
            Self::Roughdb(db) => db.read(key).await,
            #[cfg(feature = "kv-experimental")]
            Self::Jammdb(db) => db.read(key).await,
            #[cfg(feature = "kv-experimental")]
            Self::Lsmdb(db) => db.read(key).await,
            Self::Lkv(db) => {
                let txn = db.begin_read()?;
                Ok(txn.get(key)?.map(ToOwned::to_owned))
            }
        }
    }

    pub(super) async fn write_batch(
        &mut self,
        durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
    ) -> Result<()> {
        let mode = RetryMode::SingleAttempt;
        match self {
            Self::Redb(db) => db.commit(durability, puts, deletes, mode).await,
            Self::Fjall(db) => db.commit(durability, puts, deletes, mode).await,
            Self::Surrealkv(db) => db.commit(durability, puts, deletes, mode).await,
            Self::Heed(db) => db.commit(durability, puts, deletes, mode).await,
            Self::Sled(db) => db.commit(durability, puts, deletes, mode).await,
            Self::Manifold(db) => db.commit(durability, puts, deletes, mode).await,
            Self::Turbokv(db) => {
                db.as_mut()
                    .context("TurboKV already closed")?
                    .commit(durability, puts, deletes, mode)
                    .await
            }
            Self::Paritydb(db) => db.commit(durability, puts, deletes, mode).await,
            #[cfg(feature = "kv-external")]
            Self::Rocksdb(db) => db.commit(durability, puts, deletes, mode).await,
            #[cfg(feature = "kv-external")]
            Self::Mdbx(db) => db.commit(durability, puts, deletes, mode).await,
            #[cfg(feature = "kv-external")]
            Self::Persy(db) => db.commit(durability, puts, deletes, mode).await,
            #[cfg(feature = "kv-experimental")]
            Self::Roughdb(db) => db.commit(durability, puts, deletes, mode).await,
            #[cfg(feature = "kv-experimental")]
            Self::Jammdb(db) => db.commit(durability, puts, deletes, mode).await,
            #[cfg(feature = "kv-experimental")]
            Self::Lsmdb(db) => db.commit(durability, puts, deletes, mode).await,
            Self::Lkv(db) => {
                let mut txn = db.begin_write()?;
                for (k, v) in puts {
                    txn.put(k.as_slice(), v.as_slice())?;
                }
                for k in deletes {
                    txn.delete(k.as_slice())?;
                }
                txn.commit()?;
                Ok(0)
            }
        }
        .map(|_| ())
    }

    pub(super) async fn scan_count(&mut self, start: &[u8], end: &[u8]) -> Result<usize> {
        match self {
            Self::Redb(db) => db.count_range(start, end).await,
            Self::Fjall(db) => db.count_range(start, end).await,
            Self::Surrealkv(db) => db.count_range(start, end).await,
            Self::Heed(db) => db.count_range(start, end).await,
            Self::Sled(db) => db.count_range(start, end).await,
            Self::Manifold(db) => db.count_range(start, end).await,
            Self::Turbokv(db) => {
                db.as_mut()
                    .context("TurboKV already closed")?
                    .count_range(start, end)
                    .await
            }
            Self::Paritydb(db) => db.count_range(start, end).await,
            #[cfg(feature = "kv-external")]
            Self::Rocksdb(db) => db.count_range(start, end).await,
            #[cfg(feature = "kv-external")]
            Self::Mdbx(db) => db.count_range(start, end).await,
            #[cfg(feature = "kv-external")]
            Self::Persy(db) => db.count_range(start, end).await,
            #[cfg(feature = "kv-experimental")]
            Self::Roughdb(db) => db.count_range(start, end).await,
            #[cfg(feature = "kv-experimental")]
            Self::Jammdb(db) => db.count_range(start, end).await,
            #[cfg(feature = "kv-experimental")]
            Self::Lsmdb(db) => db.count_range(start, end).await,
            Self::Lkv(_) => bail!("lkv 0.2.1 exposes full iteration but no keyed range seek"),
        }
    }

    pub(super) async fn close(&mut self) -> Result<()> {
        match self {
            Self::Surrealkv(tree) => tree.close().await?,
            Self::Turbokv(slot) => {
                if let Some(db) = slot.take() {
                    db.0.close().await?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod shutdown_tests {
    use super::{Durability, Engine, EngineKind};
    use anyhow::{Context, Result, ensure};
    use clap::ValueEnum;
    use std::{fs, path::PathBuf, time::SystemTime};

    struct RemoveOnDrop(PathBuf);

    impl Drop for RemoveOnDrop {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    async fn check_native_operations(
        kind: EngineKind,
        durability: Durability,
        root: &std::path::Path,
    ) -> Result<()> {
        let path = root.join(format!("{kind:?}-{durability:?}"));
        let mut engine = Engine::open(kind, durability, &path).await?;
        let key = |id: u64| id.to_be_bytes().to_vec();
        let original = (1..=4)
            .map(|id| (key(id), vec![u8::try_from(id).unwrap_or_default(); 16]))
            .collect::<Vec<_>>();
        engine.write_batch(durability, &original, &[]).await?;
        engine
            .write_batch(
                durability,
                &[(key(2), vec![42; 16]), (key(5), vec![5; 16])],
                &[key(1), key(3)],
            )
            .await?;
        let actual = [
            None,
            Some(vec![42; 16]),
            None,
            Some(vec![4; 16]),
            Some(vec![5; 16]),
        ];
        for (id, expected) in (1..=5).zip(actual) {
            ensure!(
                engine.get(&key(id)).await? == expected,
                "unexpected value at {id}"
            );
        }
        ensure!(
            engine.get(&key(6)).await?.is_none(),
            "missing key returned a value"
        );
        if kind.supports_range_scan() {
            ensure!(
                engine.scan_count(&key(1), &key(5)).await? == 2,
                "range must count only existing keys and exclude endpoint"
            );
            ensure!(
                engine.scan_count(&key(2), &key(3)).await? == 1,
                "inclusive lower bound"
            );
        } else {
            ensure!(
                engine.scan_count(&key(1), &key(5)).await.is_err(),
                "unordered backend exposed range"
            );
        }
        engine.close().await?;
        drop(engine);
        fs::remove_dir_all(&path)?;
        Ok(())
    }

    #[tokio::test]
    async fn native_operations_match_fixed_key_value_contract() -> Result<()> {
        let id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("dbbench-native-ops-{}-{id}", std::process::id()));
        fs::create_dir(&root)?;
        let _cleanup = RemoveOnDrop(root.clone());
        for &kind in EngineKind::value_variants() {
            for durability in [Durability::Sync, Durability::Relaxed] {
                if kind.durability_mapping(durability).is_ok() {
                    check_native_operations(kind, durability, &root)
                        .await
                        .with_context(|| format!("{kind:?}/{durability:?}"))?;
                }
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn surrealkv_explicit_close_releases_lock_before_handle_drop() -> anyhow::Result<()> {
        let id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "dbbench-surrealkv-close-{}-{id}",
            std::process::id()
        ));
        let _cleanup = RemoveOnDrop(path.clone());

        let mut first = Engine::open(EngineKind::Surrealkv, Durability::Sync, &path).await?;
        first.close().await?;
        // A purely async Drop cannot promise that the file lock has been
        // released before the next operation; explicit close must.
        let mut reopened = Engine::open(EngineKind::Surrealkv, Durability::Sync, &path).await?;
        reopened.close().await?;
        drop(reopened);
        drop(first);
        fs::remove_dir_all(&path)?;
        Ok(())
    }
}
