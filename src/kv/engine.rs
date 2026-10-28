use anyhow::{Context, Result, bail};
use std::{fs, path::Path};

use fjall::{
    CompressionType as FjallCompression, Database as FjallDb, Keyspace, KeyspaceCreateOptions,
    PersistMode, config::CompressionPolicy as FjallCompressionPolicy,
};
use heed::{Database as HeedDb, Env as HeedEnv, EnvFlags, EnvOpenOptions, types::Bytes};
use manifold::{
    Durability as ManifoldDurability, TableDefinition as ManifoldTableDefinition,
    column_family::{ColumnFamily as ManifoldCf, ColumnFamilyDatabase as ManifoldDb},
};
use parity_db::{Db as ParityDb, Options as ParityOptions};
use redb::{Database as RedbDb, Durability as RedbDurability, ReadableDatabase, TableDefinition};
use surrealkv::{
    CompressionType as SurrealCompression, Durability as SurrealDurability, LSMIterator,
    Mode as SurrealMode, Options as SurrealOptions, Tree as SurrealTree,
    TreeBuilder as SurrealTreeBuilder,
};
use turbokv::{
    Compression as TurboCompression, Db as TurboDb, DbOptions as TurboOptions,
    WriteBatch as TurboBatch,
};

#[cfg(feature = "kv-external")]
use libmdbx::{
    Database as MdbxDb, DatabaseOptions as MdbxOptions, Mode as MdbxMode, NoWriteMap,
    ReadWriteOptions as MdbxReadWriteOptions, SyncMode as MdbxSyncMode,
    TableFlags as MdbxTableFlags, WriteFlags as MdbxWriteFlags,
};
#[cfg(feature = "kv-external")]
use persy::{
    ByteVec as PersyByteVec, Config as PersyConfig, Persy, TransactionConfig as PersyTxConfig,
    ValueMode as PersyValueMode,
};
#[cfg(feature = "kv-external")]
use rocksdb::{
    DB as RocksDb, DBCompressionType as RocksCompression, Direction as RocksDirection,
    IteratorMode as RocksIteratorMode, Options as RocksOptions, WriteBatch as RocksBatch,
    WriteOptions as RocksWriteOptions,
};

#[cfg(feature = "kv-experimental")]
use jammdb::{DB as JammDb, Data as JammData};
#[cfg(feature = "kv-experimental")]
use lsm_db::{Batch as LsmBatch, Lsm as LsmDb};
#[cfg(feature = "kv-experimental")]
use roughdb::{
    CompressionType as RoughCompression, Db as RoughDb, OpenMode as RoughOpenMode,
    Options as RoughOptions, ReadOptions as RoughReadOptions, WriteBatch as RoughBatch,
    WriteOptions as RoughWriteOptions,
};

use super::types::{Durability, EngineCompression, EngineKind};

pub(crate) const REDB_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("kv");
pub(crate) const MANIFOLD_TABLE: ManifoldTableDefinition<&[u8], &[u8]> =
    ManifoldTableDefinition::new("kv");

#[allow(unsafe_code)]
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

pub(crate) enum Engine {
    Redb(RedbDb),
    Fjall {
        db: FjallDb,
        keyspace: Keyspace,
    },
    Surrealkv(SurrealTree),
    Heed {
        env: HeedEnv,
        db: HeedDb<Bytes, Bytes>,
    },
    Sled(sled::Db),
    Lkv(lkv::Database),
    Manifold {
        _db: ManifoldDb,
        cf: ManifoldCf,
    },
    Turbokv(Option<TurboDb>),
    ParitydbHash(ParityDb),
    ParitydbBtree(ParityDb),
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

impl EngineKind {
    pub(crate) fn version(self) -> &'static str {
        match self {
            Self::Redb => "4.4.0",
            Self::Fjall => "3.1.12",
            Self::Surrealkv => "1.0.0",
            Self::Heed => "0.22.1",
            Self::Sled => "1.0.0-alpha.124",
            Self::Lkv => "0.2.1",
            Self::Manifold | Self::ManifoldWal => "3.1.0",
            Self::Turbokv => "0.6.0",
            Self::ParitydbHash | Self::ParitydbBtree => "0.5.7",
            #[cfg(feature = "kv-external")]
            Self::Rocksdb => "0.25.0",
            #[cfg(feature = "kv-external")]
            Self::Mdbx => "0.9.0",
            #[cfg(feature = "kv-external")]
            Self::Persy => "1.8.1",
            #[cfg(feature = "kv-experimental")]
            Self::Roughdb => "0.11.1",
            #[cfg(feature = "kv-experimental")]
            Self::Jammdb => "0.11.0",
            #[cfg(feature = "kv-experimental")]
            Self::Lsmdb => "1.1.0",
        }
    }

    pub(crate) fn durability_mapping(self, d: Durability) -> Result<&'static str> {
        Ok(match (self, d) {
            (Self::Redb, Durability::Relaxed) => "redb Durability::None",
            (Self::Redb, Durability::Sync) => "redb Durability::Immediate",
            (Self::Fjall, Durability::Relaxed) => "fjall commit + PersistMode::Buffer",
            (Self::Fjall, Durability::Sync) => "fjall commit + PersistMode::SyncAll",
            (Self::Surrealkv, Durability::Relaxed) => "surrealkv Durability::Eventual",
            (Self::Surrealkv, Durability::Sync) => "surrealkv Durability::Immediate",
            (Self::Heed, Durability::Relaxed) => "LMDB MDB_NOSYNC via heed EnvFlags::NO_SYNC",
            (Self::Heed, Durability::Sync) => "LMDB default synchronous commit",
            (Self::Sled, Durability::Relaxed) => "sled apply_batch without explicit flush",
            (Self::Sled, Durability::Sync) => "sled apply_batch + Db::flush before acknowledgement",
            (Self::Lkv, Durability::Sync) => "lkv commit; sync_data before publication",
            (Self::Lkv, Durability::Relaxed) => bail!("lkv 0.2.1 has no relaxed commit mode"),
            (Self::Manifold, Durability::Sync) => {
                "Manifold column-family without WAL + Durability::Immediate; direct durable commit"
            }
            (Self::Manifold, Durability::Relaxed) => {
                bail!("Manifold no-WAL comparison is sync-only")
            }
            (Self::ManifoldWal, Durability::Sync) => {
                "Manifold default column-family WAL + Durability::Immediate; diagnostic configuration (3.1.0 fails abrupt-process recovery)"
            }
            (Self::ManifoldWal, Durability::Relaxed) => bail!(
                "Manifold WAL path fsyncs even non-durable transaction records; relaxed lane intentionally omitted"
            ),
            (Self::Turbokv, Durability::Relaxed) => {
                "TurboKV DbOptions::durable; WAL without per-write fsync"
            }
            (Self::Turbokv, Durability::Sync) => {
                "TurboKV DbOptions::paranoid; sync WAL before acknowledgement"
            }
            (Self::ParitydbHash | Self::ParitydbBtree, Durability::Relaxed) => {
                "ParityDB default background durability pipeline; commit publishes to the in-memory overlay before WAL/data fsync completes"
            }
            (Self::ParitydbHash | Self::ParitydbBtree, Durability::Sync) => {
                bail!("ParityDB 0.5.7 has no public durable-before-return commit API")
            }
            #[cfg(feature = "kv-external")]
            (Self::Rocksdb, Durability::Relaxed) => {
                "RocksDB WAL enabled; WriteOptions::set_sync(false)"
            }
            #[cfg(feature = "kv-external")]
            (Self::Rocksdb, Durability::Sync) => {
                "RocksDB WAL enabled; WriteOptions::set_sync(true)"
            }
            #[cfg(feature = "kv-external")]
            (Self::Mdbx, Durability::Relaxed) => {
                "libmdbx NoWriteMap + SyncMode::SafeNoSync; crash rolls back to last steady commit"
            }
            #[cfg(feature = "kv-external")]
            (Self::Mdbx, Durability::Sync) => "libmdbx NoWriteMap + SyncMode::Durable",
            #[cfg(feature = "kv-external")]
            (Self::Persy, Durability::Relaxed) => {
                "Persy TransactionConfig background_sync=true; fsync queued after acknowledgement"
            }
            #[cfg(feature = "kv-external")]
            (Self::Persy, Durability::Sync) => {
                "Persy foreground transaction sync; file sync_all before commit completes"
            }
            #[cfg(feature = "kv-experimental")]
            (Self::Roughdb, Durability::Relaxed) => "RoughDB WAL + WriteOptions { sync: false }",
            #[cfg(feature = "kv-experimental")]
            (Self::Roughdb, Durability::Sync) => "RoughDB WAL + WriteOptions { sync: true }",
            #[cfg(feature = "kv-experimental")]
            (Self::Jammdb, Durability::Sync) => {
                "jammdb writable transaction commit; file sync_all before publication"
            }
            #[cfg(feature = "kv-experimental")]
            (Self::Jammdb, Durability::Relaxed) => {
                bail!("jammdb 0.11.0 exposes no relaxed commit mode")
            }
            #[cfg(feature = "kv-experimental")]
            (Self::Lsmdb, Durability::Sync) => {
                "lsm-db 1.1.0 durability feature; wal-db durable log before acknowledgement"
            }
            #[cfg(feature = "kv-experimental")]
            (Self::Lsmdb, Durability::Relaxed) => {
                bail!("lsm-db durability is compile-time; this adapter is the durable build")
            }
        })
    }
}

pub(crate) fn compression_override_supported(kind: EngineKind) -> bool {
    match kind {
        EngineKind::Fjall | EngineKind::Surrealkv | EngineKind::Turbokv => true,
        #[cfg(feature = "kv-external")]
        EngineKind::Rocksdb => true,
        _ => false,
    }
}

pub(crate) fn compression_mapping(
    kind: EngineKind,
    compression: EngineCompression,
) -> Result<&'static str> {
    Ok(match (kind, compression) {
        (EngineKind::Fjall, EngineCompression::Default) => {
            "Fjall default: L0/L1 data blocks uncompressed, L2+ LZ4; journal default unchanged"
        }
        (EngineKind::Fjall, EngineCompression::None) => {
            "Fjall data blocks uncompressed; journal compression disabled"
        }
        (EngineKind::Fjall, EngineCompression::Lz4) => {
            "Fjall data blocks LZ4 at every level; journal LZ4"
        }
        (EngineKind::Fjall, EngineCompression::Snappy | EngineCompression::Zstd) => {
            bail!("Fjall 3.1.12 exposes LZ4 but not the requested codec")
        }
        (EngineKind::Surrealkv, EngineCompression::Default | EngineCompression::None) => {
            "SurrealKV SSTable compression disabled; WAL compression unchanged"
        }
        (EngineKind::Surrealkv, EngineCompression::Snappy) => {
            "SurrealKV SSTable blocks Snappy at every level; WAL compression unchanged"
        }
        (EngineKind::Surrealkv, EngineCompression::Zstd) => {
            "SurrealKV SSTable blocks Zstd at every level; WAL compression unchanged"
        }
        (EngineKind::Surrealkv, EngineCompression::Lz4) => {
            bail!("SurrealKV 1.0.0 SSTables do not expose LZ4 compression")
        }
        (EngineKind::Turbokv, EngineCompression::Default | EngineCompression::Lz4) => {
            "TurboKV SSTable compression LZ4; WAL compression unchanged"
        }
        (EngineKind::Turbokv, EngineCompression::None) => {
            "TurboKV SSTable compression disabled; WAL compression unchanged"
        }
        (EngineKind::Turbokv, EngineCompression::Snappy) => {
            "TurboKV SSTable compression Snappy; WAL compression unchanged"
        }
        (EngineKind::Turbokv, EngineCompression::Zstd) => {
            "TurboKV SSTable compression Zstd; WAL compression unchanged"
        }
        #[cfg(feature = "kv-external")]
        (EngineKind::Rocksdb, EngineCompression::Default | EngineCompression::None) => {
            "RocksDB benchmark SSTable compression disabled; WAL compression unchanged"
        }
        #[cfg(feature = "kv-external")]
        (EngineKind::Rocksdb, EngineCompression::Lz4) => {
            "RocksDB SSTable compression LZ4; WAL compression unchanged"
        }
        #[cfg(feature = "kv-external")]
        (EngineKind::Rocksdb, EngineCompression::Snappy) => {
            "RocksDB SSTable compression Snappy; WAL compression unchanged"
        }
        #[cfg(feature = "kv-external")]
        (EngineKind::Rocksdb, EngineCompression::Zstd) => {
            "RocksDB SSTable compression Zstd; WAL compression unchanged"
        }
        (_, EngineCompression::Default) => {
            "engine native/default compression configuration; no benchmark override"
        }
        _ => bail!("engine {kind:?} does not expose the requested benchmark compression override"),
    })
}

impl Engine {
    pub(crate) async fn open(
        kind: EngineKind,
        durability: Durability,
        path: &Path,
    ) -> Result<Self> {
        Self::open_with_compression(kind, durability, path, EngineCompression::Default).await
    }

    pub(crate) async fn open_with_compression(
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
                Ok(Self::Fjall { db, keyspace })
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
                let mut wtxn = env.write_txn()?;
                let db: HeedDb<Bytes, Bytes> = env.create_database(&mut wtxn, None)?;
                wtxn.commit()?;
                Ok(Self::Heed { env, db })
            }
            EngineKind::Sled => {
                let db = sled::Config::new()
                    .path(path.join("sled"))
                    .flush_every_ms(None)
                    .open()?;
                Ok(Self::Sled(db))
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
                Ok(Self::Manifold { _db: db, cf })
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
                Ok(Self::Turbokv(Some(
                    TurboDb::open_with_options(path.join("turbokv"), opts).await?,
                )))
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
                Ok(match kind {
                    EngineKind::ParitydbHash => Self::ParitydbHash(db),
                    EngineKind::ParitydbBtree => Self::ParitydbBtree(db),
                    _ => unreachable!(),
                })
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

    pub(crate) async fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(match self {
            Self::Redb(db) => {
                let txn = db.begin_read()?;
                let table = txn.open_table(REDB_TABLE)?;
                table.get(key)?.map(|v| v.value().to_vec())
            }
            Self::Fjall { keyspace, .. } => keyspace.get(key)?.map(|v| v.to_vec()),
            Self::Surrealkv(tree) => {
                let txn = tree.begin_with_mode(SurrealMode::ReadOnly)?;
                txn.get(key)?.map(|v| v.to_vec())
            }
            Self::Heed { env, db } => {
                let txn = env.read_txn()?;
                db.get(&txn, key)?.map(ToOwned::to_owned)
            }
            Self::Sled(db) => db.get(key)?.map(|v| v.to_vec()),
            Self::Lkv(db) => {
                let txn = db.begin_read()?;
                txn.get(key)?.map(ToOwned::to_owned)
            }
            Self::Manifold { cf, .. } => {
                let txn = cf.begin_read()?;
                let table = txn.open_table(MANIFOLD_TABLE)?;
                table.get(key)?.map(|v| v.value().to_vec())
            }
            Self::Turbokv(db) => {
                db.as_ref()
                    .context("TurboKV already closed")?
                    .get(key)
                    .await?
            }
            Self::ParitydbHash(db) | Self::ParitydbBtree(db) => db.get(0, key)?,
            #[cfg(feature = "kv-external")]
            Self::Rocksdb(db) => db.get(key)?.map(|v| v.to_vec()),
            #[cfg(feature = "kv-external")]
            Self::Mdbx(db) => {
                let txn = db.begin_ro_txn()?;
                let table = txn.open_table(None)?;
                txn.get::<Vec<u8>>(&table, key)?
            }
            #[cfg(feature = "kv-external")]
            Self::Persy(db) => {
                let key = PersyByteVec::from(key.to_vec());
                db.one::<PersyByteVec, PersyByteVec>("kv", &key)?
                    .map(Vec::<u8>::from)
            }
            #[cfg(feature = "kv-experimental")]
            Self::Roughdb(db) => db.get(key)?,
            #[cfg(feature = "kv-experimental")]
            Self::Jammdb(db) => {
                let tx = db.tx(false)?;
                let bucket = tx.get_bucket("kv")?;
                match bucket.get(key) {
                    Some(JammData::KeyValue(kv)) => Some(kv.value().to_vec()),
                    Some(JammData::Bucket(_)) => bail!("jammdb KV bucket contains nested bucket"),
                    None => None,
                }
            }
            #[cfg(feature = "kv-experimental")]
            Self::Lsmdb(db) => db.get(key)?,
        })
    }

    pub(crate) async fn write_batch(
        &mut self,
        durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
    ) -> Result<()> {
        match self {
            Self::Redb(db) => {
                let mut txn = db.begin_write()?;
                txn.set_durability(match durability {
                    Durability::Relaxed => RedbDurability::None,
                    Durability::Sync => RedbDurability::Immediate,
                })?;
                {
                    let mut table = txn.open_table(REDB_TABLE)?;
                    for (k, v) in puts {
                        table.insert(k.as_slice(), v.as_slice())?;
                    }
                    for k in deletes {
                        table.remove(k.as_slice())?;
                    }
                }
                txn.commit()?;
            }
            Self::Fjall { db, keyspace } => {
                let mut batch = db.batch();
                for (k, v) in puts {
                    batch.insert(keyspace, k.as_slice(), v.as_slice());
                }
                for k in deletes {
                    batch.remove(keyspace, k.as_slice());
                }
                batch.commit()?;
                db.persist(match durability {
                    Durability::Relaxed => PersistMode::Buffer,
                    Durability::Sync => PersistMode::SyncAll,
                })?;
            }
            Self::Surrealkv(tree) => {
                let mut txn = tree.begin()?;
                txn.set_durability(match durability {
                    Durability::Relaxed => SurrealDurability::Eventual,
                    Durability::Sync => SurrealDurability::Immediate,
                });
                for (k, v) in puts {
                    txn.set(k.as_slice(), v.as_slice())?;
                }
                for k in deletes {
                    txn.delete(k.as_slice())?;
                }
                txn.commit().await?;
            }
            Self::Heed { env, db } => {
                let mut txn = env.write_txn()?;
                for (k, v) in puts {
                    db.put(&mut txn, k.as_slice(), v.as_slice())?;
                }
                for k in deletes {
                    db.delete(&mut txn, k.as_slice())?;
                }
                txn.commit()?;
            }
            Self::Sled(db) => {
                let mut batch = sled::Batch::default();
                for (k, v) in puts {
                    batch.insert(k.as_slice(), v.as_slice());
                }
                for k in deletes {
                    batch.remove(k.as_slice());
                }
                db.apply_batch(batch)?;
                if durability == Durability::Sync {
                    db.flush()?;
                }
            }
            Self::Lkv(db) => {
                let mut txn = db.begin_write()?;
                for (k, v) in puts {
                    txn.put(k.as_slice(), v.as_slice())?;
                }
                for k in deletes {
                    txn.delete(k.as_slice())?;
                }
                txn.commit()?;
            }
            Self::Manifold { cf, .. } => {
                let mut txn = cf.begin_write()?;
                txn.set_durability(ManifoldDurability::Immediate)?;
                {
                    let mut table = txn.open_table(MANIFOLD_TABLE)?;
                    for (k, v) in puts {
                        table.insert(k.as_slice(), v.as_slice())?;
                    }
                    for k in deletes {
                        table.remove(k.as_slice())?;
                    }
                }
                txn.commit()?;
            }
            Self::Turbokv(db) => {
                let mut batch = TurboBatch::new();
                for (k, v) in puts {
                    batch.put(k.as_slice(), v.as_slice());
                }
                for k in deletes {
                    batch.delete(k.as_slice());
                }
                db.as_ref()
                    .context("TurboKV already closed")?
                    .write_batch(&batch)
                    .await?;
            }
            Self::ParitydbHash(db) | Self::ParitydbBtree(db) => {
                let mut changes = Vec::with_capacity(puts.len() + deletes.len());
                for (k, v) in puts {
                    changes.push((0u8, k.clone(), Some(v.clone())));
                }
                for k in deletes {
                    changes.push((0u8, k.clone(), None));
                }
                db.commit(changes)?;
            }
            #[cfg(feature = "kv-external")]
            Self::Rocksdb(db) => {
                let mut batch = RocksBatch::default();
                for (k, v) in puts {
                    batch.put(k, v);
                }
                for k in deletes {
                    batch.delete(k);
                }
                let mut opts = RocksWriteOptions::default();
                opts.set_sync(durability == Durability::Sync);
                db.write_opt(batch, &opts)?;
            }
            #[cfg(feature = "kv-external")]
            Self::Mdbx(db) => {
                let txn = db.begin_rw_txn()?;
                let table = txn.create_table(None, MdbxTableFlags::default())?;
                for (k, v) in puts {
                    txn.put(&table, k, v, MdbxWriteFlags::UPSERT)?;
                }
                for k in deletes {
                    let _ = txn.del(&table, k, None)?;
                }
                txn.commit()?;
            }
            #[cfg(feature = "kv-external")]
            Self::Persy(db) => {
                let mut tx = match durability {
                    Durability::Relaxed => {
                        db.begin_with(PersyTxConfig::new().set_background_sync(true))?
                    }
                    Durability::Sync => db.begin()?,
                };
                for (k, v) in puts {
                    tx.put::<PersyByteVec, PersyByteVec>(
                        "kv",
                        PersyByteVec::from(k.clone()),
                        PersyByteVec::from(v.clone()),
                    )?;
                }
                for k in deletes {
                    tx.remove::<PersyByteVec, PersyByteVec>(
                        "kv",
                        PersyByteVec::from(k.clone()),
                        None,
                    )?;
                }
                tx.prepare()?.commit()?;
            }
            #[cfg(feature = "kv-experimental")]
            Self::Roughdb(db) => {
                let mut batch = RoughBatch::new();
                for (k, v) in puts {
                    batch.put(k, v);
                }
                for k in deletes {
                    batch.delete(k);
                }
                db.write(
                    &RoughWriteOptions {
                        sync: durability == Durability::Sync,
                    },
                    batch,
                )?;
            }
            #[cfg(feature = "kv-experimental")]
            Self::Jammdb(db) => {
                if durability == Durability::Relaxed {
                    bail!("jammdb relaxed mode unsupported");
                }
                let tx = db.tx(true)?;
                let bucket = tx.get_bucket("kv")?;
                for (k, v) in puts {
                    bucket.put(k.as_slice(), v.as_slice())?;
                }
                for k in deletes {
                    if bucket.get(k.as_slice()).is_some() {
                        let _ = bucket.delete(k.as_slice())?;
                    }
                }
                tx.commit()?;
            }
            #[cfg(feature = "kv-experimental")]
            Self::Lsmdb(db) => {
                if durability == Durability::Relaxed {
                    bail!("lsm-db relaxed mode unsupported in durable build");
                }
                let mut batch = LsmBatch::new();
                for (k, v) in puts {
                    batch.put(k, v);
                }
                for k in deletes {
                    batch.delete(k);
                }
                db.write(batch)?;
            }
        }
        Ok(())
    }

    pub(crate) async fn scan_count(&mut self, start: &[u8], end: &[u8]) -> Result<usize> {
        Ok(match self {
            Self::Redb(db) => {
                let txn = db.begin_read()?;
                let table = txn.open_table(REDB_TABLE)?;
                let mut n = 0usize;
                for item in table.range(start..end)? {
                    let _ = item?;
                    n += 1;
                }
                n
            }
            Self::Fjall { keyspace, .. } => {
                let mut n = 0usize;
                for item in keyspace.range(start..end) {
                    let _ = item.into_inner()?;
                    n += 1;
                }
                n
            }
            Self::Surrealkv(tree) => {
                let txn = tree.begin_with_mode(SurrealMode::ReadOnly)?;
                let mut iter = txn.range(start, end)?;
                let mut n = 0usize;
                iter.seek_first()?;
                while iter.valid() {
                    let _ = iter.value()?;
                    n += 1;
                    if !iter.next()? {
                        break;
                    }
                }
                n
            }
            Self::Heed { env, db } => {
                let txn = env.read_txn()?;
                let range = (
                    std::ops::Bound::Included(start),
                    std::ops::Bound::Excluded(end),
                );
                let mut n = 0usize;
                for item in db.range(&txn, &range)? {
                    let _ = item?;
                    n += 1;
                }
                n
            }
            Self::Sled(db) => {
                let mut n = 0usize;
                for item in db.range(start..end) {
                    let _ = item?;
                    n += 1;
                }
                n
            }
            Self::Lkv(_) => bail!("lkv 0.2.1 exposes full iteration but no keyed range seek"),
            Self::Manifold { cf, .. } => {
                let txn = cf.begin_read()?;
                let table = txn.open_table(MANIFOLD_TABLE)?;
                let mut n = 0usize;
                for item in table.range(start..end)? {
                    let _ = item?;
                    n += 1;
                }
                n
            }
            Self::Turbokv(db) => db
                .as_ref()
                .context("TurboKV already closed")?
                .range(start, end)
                .await?
                .len(),
            Self::ParitydbHash(_) => {
                bail!("ParityDB hash-column mode has no ordered range iterator")
            }
            Self::ParitydbBtree(db) => {
                let mut iter = db.iter(0)?;
                iter.seek(start)?;
                let mut n = 0usize;
                while let Some((k, _v)) = iter.next()? {
                    if k.as_slice() >= end {
                        break;
                    }
                    n += 1;
                }
                n
            }
            #[cfg(feature = "kv-external")]
            Self::Rocksdb(db) => {
                let mut n = 0usize;
                for item in db.iterator(RocksIteratorMode::From(start, RocksDirection::Forward)) {
                    let (k, _) = item?;
                    if k.as_ref() >= end {
                        break;
                    }
                    n += 1;
                }
                n
            }
            #[cfg(feature = "kv-external")]
            Self::Mdbx(db) => {
                let txn = db.begin_ro_txn()?;
                let table = txn.open_table(None)?;
                let mut cursor = txn.cursor(&table)?;
                let mut n = 0usize;
                for item in cursor.iter_from::<Vec<u8>, Vec<u8>>(start) {
                    let (k, _) = item?;
                    if k.as_slice() >= end {
                        break;
                    }
                    n += 1;
                }
                n
            }
            #[cfg(feature = "kv-external")]
            Self::Persy(db) => {
                let range = PersyByteVec::from(start.to_vec())..PersyByteVec::from(end.to_vec());
                let mut n = 0usize;
                for (_k, values) in db.range::<PersyByteVec, PersyByteVec, _>("kv", range)? {
                    if values.into_iter().next().is_some() {
                        n += 1;
                    }
                }
                n
            }
            #[cfg(feature = "kv-experimental")]
            Self::Roughdb(db) => {
                let mut iter = db.new_iterator(&RoughReadOptions::default())?;
                iter.seek(start);
                let mut n = 0usize;
                for item in iter.forward() {
                    let (k, _) = item?;
                    if k.as_slice() >= end {
                        break;
                    }
                    n += 1;
                }
                n
            }
            #[cfg(feature = "kv-experimental")]
            Self::Jammdb(db) => {
                let tx = db.tx(false)?;
                let bucket = tx.get_bucket("kv")?;
                let mut cursor = bucket.cursor();
                cursor.seek(start);
                let mut n = 0usize;
                for item in cursor {
                    let k = item.key();
                    if k >= end {
                        break;
                    }
                    if matches!(item, JammData::KeyValue(_)) {
                        n += 1;
                    }
                }
                n
            }
            #[cfg(feature = "kv-experimental")]
            Self::Lsmdb(db) => db.scan(start.to_vec()..end.to_vec())?.count(),
        })
    }

    pub(crate) async fn close(&mut self) -> Result<()> {
        if let Self::Turbokv(slot) = self {
            if let Some(db) = slot.take() {
                db.close().await?;
            }
        }
        Ok(())
    }
}
