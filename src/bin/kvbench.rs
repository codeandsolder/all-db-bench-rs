use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use hdrhistogram::Histogram;
use rand::{Rng, SeedableRng, rngs::SmallRng, seq::SliceRandom};
use serde::Serialize;
#[path = "../metrics.rs"]
mod metrics;
use metrics::{ProcDelta, ProcSnapshot, SystemDelta, SystemSnapshot};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use fjall::{Database as FjallDb, Keyspace, KeyspaceCreateOptions, PersistMode};
use heed::{Database as HeedDb, Env as HeedEnv, EnvFlags, EnvOpenOptions, types::Bytes};
use manifold::{
    Durability as ManifoldDurability, TableDefinition as ManifoldTableDefinition,
    column_family::{ColumnFamily as ManifoldCf, ColumnFamilyDatabase as ManifoldDb},
};
use redb::{Database as RedbDb, Durability as RedbDurability, ReadableDatabase, TableDefinition};
use surrealkv::{
    Durability as SurrealDurability, LSMIterator, Mode as SurrealMode, Tree as SurrealTree,
    TreeBuilder as SurrealTreeBuilder,
};
use turbokv::{Db as TurboDb, DbOptions as TurboOptions, WriteBatch as TurboBatch};

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
    CompressionType as RoughCompression, Db as RoughDb, Options as RoughOptions,
    ReadOptions as RoughReadOptions, WriteBatch as RoughBatch, WriteOptions as RoughWriteOptions,
};

const REDB_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("kv");
const MANIFOLD_TABLE: ManifoldTableDefinition<&[u8], &[u8]> = ManifoldTableDefinition::new("kv");
const HIST_MAX_NS: u64 = 60_000_000_000;

#[derive(Clone, Copy, Debug, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
enum EngineKind {
    Redb,
    Fjall,
    Surrealkv,
    Heed,
    Sled,
    Lkv,
    Manifold,
    Turbokv,
    #[cfg(feature = "kv-external")]
    Rocksdb,
    #[cfg(feature = "kv-external")]
    Mdbx,
    #[cfg(feature = "kv-external")]
    Persy,
    #[cfg(feature = "kv-experimental")]
    Roughdb,
    #[cfg(feature = "kv-experimental")]
    Jammdb,
    #[cfg(feature = "kv-experimental")]
    Lsmdb,
}

#[derive(Clone, Copy, Debug, Serialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum Durability {
    Relaxed,
    Sync,
}

#[derive(Clone, Copy, Debug, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
enum Workload {
    PointRead,
    RangeScan,
    ReadHeavy,
    Balanced,
    TinyTxn,
    WriteBurst,
    DeleteBurst,
    Churn,
}

#[derive(Clone, Copy, Debug, Serialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum AccessPattern {
    Auto,
    Uniform,
    Hot80,
    Hot95,
}

#[derive(Clone, Copy, Debug, Serialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum KeyShape {
    Sequential,
    SharedPrefix,
    Hashed,
}

#[derive(Clone, Copy, Debug, Serialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum ValuePattern {
    PseudoRandom,
    Zeros,
    Repeated,
}

#[derive(Clone, Copy, Debug, Serialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum WritePattern {
    Append,
    UpdateUniform,
    UpdateHot,
}

#[derive(Debug, Parser)]
#[command(about = "One fresh database per invocation; output is one JSON record")]
struct Args {
    #[arg(long, value_enum)]
    engine: EngineKind,
    #[arg(long, value_enum)]
    durability: Durability,
    #[arg(long, value_enum)]
    workload: Workload,
    #[arg(long, default_value_t = 100_000)]
    records: u64,
    #[arg(long, default_value_t = 50_000)]
    ops: u64,
    #[arg(long, default_value_t = 256)]
    value_bytes: usize,
    #[arg(long, value_enum, default_value_t = ValuePattern::PseudoRandom)]
    value_pattern: ValuePattern,
    #[arg(long, default_value_t = 8)]
    key_bytes: usize,
    #[arg(long, value_enum, default_value_t = KeyShape::Sequential)]
    key_shape: KeyShape,
    #[arg(long, value_enum, default_value_t = AccessPattern::Auto)]
    access_pattern: AccessPattern,
    #[arg(long, default_value_t = 0)]
    miss_percent: u8,
    #[arg(long, value_enum, default_value_t = WritePattern::Append)]
    write_pattern: WritePattern,
    #[arg(long, default_value_t = 100)]
    txn_size: usize,
    #[arg(long, default_value_t = 100)]
    scan_len: u64,
    #[arg(long, default_value_t = 1)]
    trial: u32,
    #[arg(long, default_value_t = 0x5eed_2026)]
    seed: u64,
    #[arg(long, default_value = "baseline")]
    scenario: String,
    #[arg(long, default_value = "data")]
    root: PathBuf,
    #[arg(long, default_value = "-")]
    output: String,
    #[arg(long, default_value_t = false)]
    keep_db: bool,
    #[arg(long, default_value_t = false)]
    reuse_db: bool,
    #[arg(long, default_value_t = false)]
    skip_prefill: bool,
    #[arg(long, default_value_t = 5_000)]
    warmup_reads: u64,
    #[arg(long)]
    db_name: Option<String>,
    #[arg(long)]
    progress_file: Option<PathBuf>,
    #[arg(long)]
    verify_prefix_records: Option<u64>,
    #[arg(long, default_value_t = 0)]
    verify_tail_records: u64,
    #[arg(long, default_value_t = 0)]
    settle_ms: u64,
    #[arg(long, default_value_t = 250)]
    settle_sample_ms: u64,
}

#[derive(Debug, Serialize)]
struct Quantiles {
    count: u64,
    p50_us: f64,
    p95_us: f64,
    p99_us: f64,
    p999_us: f64,
    max_us: f64,
}

#[derive(Debug, Serialize)]
struct Verification {
    expected_prefix_records: u64,
    checked_prefix_records: u64,
    missing_prefix_records: u64,
    tail_checked_records: u64,
    tail_prefix_present: u64,
    tail_total_present: u64,
    tail_present_after_gap: u64,
    transaction_atomic_tail: bool,
    verification_ok: bool,
    verify_s: f64,
}

#[derive(Debug, Serialize)]
struct SettleSample {
    elapsed_ms: u64,
    interval_ms: u64,
    db_bytes: u64,
    process: ProcDelta,
    system: SystemDelta,
}

#[derive(Debug, Serialize)]
struct SettleMeasurement {
    requested_ms: u64,
    sample_ms: u64,
    elapsed_s: f64,
    db_bytes_before: u64,
    db_bytes_after: u64,
    process: ProcDelta,
    system: SystemDelta,
    samples: Vec<SettleSample>,
}

#[derive(Debug, Serialize)]
struct Measurement {
    format_version: u32,
    engine: EngineKind,
    engine_version: &'static str,
    durability: Durability,
    durability_mapping: &'static str,
    workload: Workload,
    records: u64,
    ops_requested: u64,
    ops_completed: u64,
    value_bytes: usize,
    value_pattern: ValuePattern,
    key_bytes: usize,
    key_shape: KeyShape,
    access_pattern: AccessPattern,
    miss_percent: u8,
    write_pattern: WritePattern,
    txn_size: usize,
    scan_len: u64,
    trial: u32,
    seed: u64,
    scenario: String,
    open_s: f64,
    prefill_s: f64,
    warmup_s: f64,
    elapsed_s: f64,
    ops_per_s: f64,
    reads: u64,
    writes: u64,
    deletes: u64,
    read_latency: Quantiles,
    write_txn_latency: Quantiles,
    db_bytes: u64,
    post_workload_settle: Option<SettleMeasurement>,
    peak_rss_kib: u64,
    measured_process: ProcDelta,
    system_before: SystemSnapshot,
    system_after: SystemSnapshot,
    measured_system_delta: SystemDelta,
    path: String,
    db_kept: bool,
    reused_db: bool,
    prefill_skipped: bool,
    warmup_reads: u64,
    verification: Option<Verification>,
}

enum Engine {
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

impl Args {
    fn engine_matches_lkv(&self) -> bool {
        matches!(self.engine, EngineKind::Lkv)
    }
}

impl EngineKind {
    fn version(self) -> &'static str {
        match self {
            Self::Redb => "4.3.0",
            Self::Fjall => "3.1.12",
            Self::Surrealkv => "0.21.4",
            Self::Heed => "0.22.1",
            Self::Sled => "1.0.0-alpha.124",
            Self::Lkv => "0.2.1",
            Self::Manifold => "3.1.0",
            Self::Turbokv => "0.6.0",
            #[cfg(feature = "kv-external")]
            Self::Rocksdb => "0.25.0",
            #[cfg(feature = "kv-external")]
            Self::Mdbx => "0.9.0",
            #[cfg(feature = "kv-external")]
            Self::Persy => "1.8.1",
            #[cfg(feature = "kv-experimental")]
            Self::Roughdb => "0.10.1",
            #[cfg(feature = "kv-experimental")]
            Self::Jammdb => "0.11.0",
            #[cfg(feature = "kv-experimental")]
            Self::Lsmdb => "1.0.0",
        }
    }

    fn durability_mapping(self, d: Durability) -> Result<&'static str> {
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
                "Manifold column-family WAL + Durability::Immediate; WAL fsync before visibility"
            }
            (Self::Manifold, Durability::Relaxed) => bail!(
                "Manifold WAL path fsyncs even non-durable transaction records; relaxed lane intentionally omitted"
            ),
            (Self::Turbokv, Durability::Relaxed) => {
                "TurboKV DbOptions::durable; WAL without per-write fsync"
            }
            (Self::Turbokv, Durability::Sync) => {
                "TurboKV DbOptions::paranoid; sync WAL before acknowledgement"
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
                "lsm-db 1.0.0 durability feature; wal-db durable log before acknowledgement"
            }
            #[cfg(feature = "kv-experimental")]
            (Self::Lsmdb, Durability::Relaxed) => {
                bail!("lsm-db durability is compile-time; this adapter is the durable build")
            }
        })
    }
}

impl Engine {
    async fn open(kind: EngineKind, durability: Durability, path: &Path) -> Result<Self> {
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
                let db = FjallDb::builder(path.join("fjall")).open()?;
                let keyspace = db.keyspace("kv", KeyspaceCreateOptions::default)?;
                Ok(Self::Fjall { db, keyspace })
            }
            EngineKind::Surrealkv => {
                let tree = SurrealTreeBuilder::new()
                    .with_path(path.join("surrealkv"))
                    .build()?;
                Ok(Self::Surrealkv(tree))
            }
            EngineKind::Heed => {
                let dir = path.join("heed");
                fs::create_dir_all(&dir)?;
                let mut opts = EnvOpenOptions::new();
                opts.map_size(16usize * 1024 * 1024 * 1024).max_dbs(1);
                if durability == Durability::Relaxed {
                    unsafe {
                        opts.flags(EnvFlags::NO_SYNC);
                    }
                }
                let env = unsafe { opts.open(&dir)? };
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
            EngineKind::Manifold => {
                if durability == Durability::Relaxed {
                    bail!("Manifold WAL path intentionally omitted from relaxed lane");
                }
                let db = ManifoldDb::builder().open(path.join("bench.manifold"))?;
                let cf = db.column_family_or_create("kv")?;
                Ok(Self::Manifold { _db: db, cf })
            }
            EngineKind::Turbokv => {
                let opts = match durability {
                    Durability::Relaxed => TurboOptions::durable(),
                    Durability::Sync => TurboOptions::paranoid(),
                };
                Ok(Self::Turbokv(Some(
                    TurboDb::open_with_options(path.join("turbokv"), opts).await?,
                )))
            }
            #[cfg(feature = "kv-external")]
            EngineKind::Rocksdb => {
                let mut opts = RocksOptions::default();
                opts.create_if_missing(true);
                opts.set_compression_type(RocksCompression::None);
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
                let db = Persy::open_or_create_with(
                    path.join("bench.persy"),
                    PersyConfig::new(),
                    |db| {
                        let mut tx = db.begin()?;
                        tx.create_index::<PersyByteVec, PersyByteVec>(
                            "kv",
                            PersyValueMode::Replace,
                        )?;
                        tx.prepare()?.commit()?;
                        Ok(())
                    },
                )?;
                Ok(Self::Persy(db))
            }
            #[cfg(feature = "kv-experimental")]
            EngineKind::Roughdb => {
                let opts = RoughOptions {
                    create_if_missing: true,
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

    async fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
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
            Self::Roughdb(db) => match db.get(key) {
                Ok(v) => Some(v),
                Err(e) if e.is_not_found() => None,
                Err(e) => return Err(e.into()),
            },
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

    async fn write_batch(
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

    async fn scan_count(&mut self, start: &[u8], end: &[u8]) -> Result<usize> {
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

    async fn close(&mut self) -> Result<()> {
        if let Self::Turbokv(slot) = self {
            if let Some(db) = slot.take() {
                db.close().await?;
            }
        }
        Ok(())
    }
}

fn mix_u64(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

fn key(id: u64, len: usize, shape: KeyShape, seed: u64) -> Vec<u8> {
    debug_assert!(len >= 8);
    let mut out = vec![0u8; len];
    match shape {
        KeyShape::Sequential => {
            out[..8].copy_from_slice(&id.to_be_bytes());
            let mut x = mix_u64(id ^ seed ^ 0x6b65_792d_7365_7100);
            for chunk in out[8..].chunks_mut(8) {
                x = mix_u64(x.wrapping_add(0x9e37_79b9_7f4a_7c15));
                let b = x.to_le_bytes();
                chunk.copy_from_slice(&b[..chunk.len()]);
            }
        }
        KeyShape::SharedPrefix => {
            out[..len - 8].fill(0x5a);
            out[len - 8..].copy_from_slice(&id.to_be_bytes());
        }
        KeyShape::Hashed => {
            let mut x = mix_u64(id ^ seed ^ 0x6861_7368_6564_6b79);
            out[..8].copy_from_slice(&x.to_be_bytes());
            for chunk in out[8..].chunks_mut(8) {
                x = mix_u64(x.wrapping_add(0x9e37_79b9_7f4a_7c15));
                let b = x.to_le_bytes();
                chunk.copy_from_slice(&b[..chunk.len()]);
            }
        }
    }
    out
}

fn sample_existing_id(
    rng: &mut SmallRng,
    records: u64,
    pattern: AccessPattern,
    workload: Workload,
) -> u64 {
    let effective = match pattern {
        AccessPattern::Auto => match workload {
            Workload::ReadHeavy | Workload::Balanced => AccessPattern::Hot80,
            _ => AccessPattern::Uniform,
        },
        other => other,
    };
    let (hot_percent, hot_fraction) = match effective {
        AccessPattern::Auto | AccessPattern::Uniform => return rng.random_range(0..records),
        AccessPattern::Hot80 => (80u32, 5u64),
        AccessPattern::Hot95 => (95u32, 20u64),
    };
    if records < hot_fraction {
        return rng.random_range(0..records);
    }
    let hot = (records / hot_fraction).max(1);
    if rng.random_range(0..100u32) < hot_percent {
        rng.random_range(0..hot)
    } else if hot < records {
        rng.random_range(hot..records)
    } else {
        rng.random_range(0..records)
    }
}

fn sample_read_id(rng: &mut SmallRng, args: &Args) -> (u64, bool) {
    let miss = args.miss_percent > 0 && rng.random_range(0..100u32) < u32::from(args.miss_percent);
    if miss {
        let miss_base = args
            .records
            .saturating_add(args.ops)
            .saturating_add(1_000_000);
        let span = args.records.max(1);
        (miss_base.saturating_add(rng.random_range(0..span)), false)
    } else {
        (
            sample_existing_id(rng, args.records, args.access_pattern, args.workload),
            true,
        )
    }
}

fn value(id: u64, len: usize, salt: u64, pattern: ValuePattern) -> Vec<u8> {
    match pattern {
        ValuePattern::Zeros => vec![0u8; len],
        ValuePattern::Repeated => {
            let block = mix_u64(id ^ salt ^ 0x7265_7065_6174_6564).to_le_bytes();
            let mut out = vec![0u8; len];
            for chunk in out.chunks_mut(block.len()) {
                chunk.copy_from_slice(&block[..chunk.len()]);
            }
            out
        }
        ValuePattern::PseudoRandom => {
            let mut x = id ^ salt ^ 0x9e37_79b9_7f4a_7c15;
            let mut out = vec![0u8; len];
            for chunk in out.chunks_mut(8) {
                x = mix_u64(x);
                let b = x.to_le_bytes();
                let n = chunk.len();
                chunk.copy_from_slice(&b[..n]);
                x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
            }
            out
        }
    }
}

fn hist() -> Histogram<u64> {
    Histogram::new_with_bounds(1, HIST_MAX_NS, 3).unwrap()
}

fn record(h: &mut Histogram<u64>, d: Duration) {
    let ns = d.as_nanos().min(HIST_MAX_NS as u128) as u64;
    let _ = h.record(ns.max(1));
}

fn quantiles(h: &Histogram<u64>) -> Quantiles {
    let us = |v: u64| v as f64 / 1000.0;
    if h.is_empty() {
        return Quantiles {
            count: 0,
            p50_us: 0.0,
            p95_us: 0.0,
            p99_us: 0.0,
            p999_us: 0.0,
            max_us: 0.0,
        };
    }
    Quantiles {
        count: h.len(),
        p50_us: us(h.value_at_quantile(0.50)),
        p95_us: us(h.value_at_quantile(0.95)),
        p99_us: us(h.value_at_quantile(0.99)),
        p999_us: us(h.value_at_quantile(0.999)),
        max_us: us(h.max()),
    }
}

fn peak_rss_kib() -> u64 {
    let Ok(status) = fs::read_to_string("/proc/self/status") else {
        return 0;
    };
    status
        .lines()
        .find_map(|line| {
            let rest = line.strip_prefix("VmHWM:")?;
            rest.split_whitespace().next()?.parse().ok()
        })
        .unwrap_or(0)
}

fn dir_size(path: &Path) -> u64 {
    fn walk(p: &Path, sum: &mut u64) {
        let Ok(md) = fs::symlink_metadata(p) else {
            return;
        };
        if md.is_file() {
            *sum = sum.saturating_add(md.len());
            return;
        }
        let Ok(rd) = fs::read_dir(p) else { return };
        for e in rd.flatten() {
            walk(&e.path(), sum);
        }
    }
    let mut n = 0;
    walk(path, &mut n);
    n
}

async fn prefill(engine: &mut Engine, args: &Args) -> Result<()> {
    const BATCH: usize = 10_000;
    let mut puts = Vec::with_capacity(BATCH);
    for id in 0..args.records {
        puts.push((
            key(id, args.key_bytes, args.key_shape, args.seed),
            value(id, args.value_bytes, args.seed, args.value_pattern),
        ));
        if puts.len() == BATCH {
            engine.write_batch(args.durability, &puts, &[]).await?;
            puts.clear();
        }
    }
    if !puts.is_empty() {
        engine.write_batch(args.durability, &puts, &[]).await?;
    }
    Ok(())
}

async fn warm_reads(engine: &mut Engine, args: &Args) -> Result<()> {
    let mut rng = SmallRng::seed_from_u64(args.seed ^ 0x7777);
    for _ in 0..args.records.min(args.warmup_reads) {
        let id = sample_existing_id(&mut rng, args.records, args.access_pattern, args.workload);
        let _ = engine
            .get(&key(id, args.key_bytes, args.key_shape, args.seed))
            .await?;
    }
    Ok(())
}

async fn run_workload(
    engine: &mut Engine,
    args: &Args,
    read_hist: &mut Histogram<u64>,
    write_hist: &mut Histogram<u64>,
) -> Result<(u64, u64, u64, u64)> {
    let mut rng = SmallRng::seed_from_u64(args.seed ^ args.trial as u64);
    let mut reads = 0u64;
    let mut writes = 0u64;
    let mut deletes = 0u64;
    let mut done = 0u64;
    let txn_size = args.txn_size.max(1);
    let mut next_id = args.records;

    let make_key = |id| key(id, args.key_bytes, args.key_shape, args.seed);
    let make_value = |id, salt| value(id, args.value_bytes, salt, args.value_pattern);

    match args.workload {
        Workload::PointRead => {
            for _ in 0..args.ops {
                let (id, expect_hit) = sample_read_id(&mut rng, args);
                let t = Instant::now();
                let got = engine.get(&make_key(id)).await?;
                record(read_hist, t.elapsed());
                if got.is_some() != expect_hit {
                    bail!("point-read hit/miss mismatch for id {id}: expected_hit={expect_hit}");
                }
                reads += 1;
                done += 1;
            }
        }
        Workload::RangeScan => {
            if args.engine_matches_lkv() {
                bail!("lkv range-scan unsupported: no keyed seek/range API");
            }
            if args.key_shape == KeyShape::Hashed {
                bail!("hashed keys intentionally have no ID-ordered range semantics");
            }
            if args.scan_len == 0 || args.scan_len > args.records {
                bail!("scan_len must be in 1..=records");
            }
            let last_start = args.records - args.scan_len;
            for _ in 0..args.ops {
                let start_id = if last_start == 0 {
                    0
                } else {
                    rng.random_range(0..=last_start)
                };
                let end_id = start_id + args.scan_len;
                let start_key = make_key(start_id);
                let end_key = make_key(end_id);
                let t = Instant::now();
                let n = engine.scan_count(&start_key, &end_key).await?;
                record(read_hist, t.elapsed());
                if n != args.scan_len as usize {
                    bail!("range scan returned {n} rows, expected {}", args.scan_len);
                }
                reads += args.scan_len;
                done += 1;
            }
        }
        Workload::TinyTxn | Workload::WriteBurst => {
            let batch = if matches!(args.workload, Workload::TinyTxn) {
                1
            } else {
                txn_size
            };
            while done < args.ops {
                let n = (args.ops - done).min(batch as u64) as usize;
                let mut puts = Vec::with_capacity(n);
                for _ in 0..n {
                    let id = match args.write_pattern {
                        WritePattern::Append => {
                            let id = next_id;
                            next_id += 1;
                            id
                        }
                        WritePattern::UpdateUniform => rng.random_range(0..args.records),
                        WritePattern::UpdateHot => sample_existing_id(
                            &mut rng,
                            args.records,
                            match args.access_pattern {
                                AccessPattern::Auto | AccessPattern::Uniform => {
                                    AccessPattern::Hot80
                                }
                                other => other,
                            },
                            args.workload,
                        ),
                    };
                    puts.push((make_key(id), make_value(id, args.seed ^ 0x1111 ^ done)));
                }
                let t = Instant::now();
                engine.write_batch(args.durability, &puts, &[]).await?;
                record(write_hist, t.elapsed());
                writes += n as u64;
                done += n as u64;
                if let Some(progress_file) = &args.progress_file {
                    fs::write(progress_file, format!("{done}\n"))
                        .context("write recovery progress marker")?;
                }
            }
        }
        Workload::DeleteBurst => {
            if args.ops > args.records {
                bail!("delete-burst requires ops <= records to avoid repeated tombstones");
            }
            while done < args.ops {
                let n = (args.ops - done).min(txn_size as u64) as usize;
                let mut dels = Vec::with_capacity(n);
                for offset in 0..n {
                    let id = done + offset as u64;
                    dels.push(make_key(id));
                }
                let t = Instant::now();
                engine.write_batch(args.durability, &[], &dels).await?;
                record(write_hist, t.elapsed());
                deletes += n as u64;
                done += n as u64;
            }
        }
        Workload::ReadHeavy | Workload::Balanced | Workload::Churn => {
            enum WriteSpec {
                Put(Vec<u8>, Vec<u8>),
                Delete(Vec<u8>),
            }
            enum Unit {
                Read(Vec<u8>),
                Write {
                    puts: Vec<(Vec<u8>, Vec<u8>)>,
                    deletes: Vec<Vec<u8>>,
                },
            }

            while done < args.ops {
                let epoch = (args.ops - done).min(1_000);
                let (r_target, u_target, i_target, d_target) = match args.workload {
                    Workload::ReadHeavy => {
                        let u = epoch * 5 / 100;
                        (epoch - u, u, 0, 0)
                    }
                    Workload::Balanced => {
                        let u = epoch * 30 / 100;
                        let i = epoch * 10 / 100;
                        let d = epoch * 10 / 100;
                        (epoch - u - i - d, u, i, d)
                    }
                    Workload::Churn => {
                        let u = epoch * 40 / 100;
                        let i = epoch * 30 / 100;
                        (0, u, i, epoch - u - i)
                    }
                    _ => unreachable!(),
                };

                let mut units = Vec::with_capacity((r_target as usize) + 16);
                for _ in 0..r_target {
                    let (id, _expect_hit) = sample_read_id(&mut rng, args);
                    units.push(Unit::Read(make_key(id)));
                }

                let mut write_specs = Vec::with_capacity((u_target + i_target + d_target) as usize);
                for _ in 0..u_target {
                    let id = sample_existing_id(
                        &mut rng,
                        args.records,
                        args.access_pattern,
                        args.workload,
                    );
                    write_specs.push(WriteSpec::Put(
                        make_key(id),
                        make_value(id, args.seed ^ done),
                    ));
                }
                for _ in 0..i_target {
                    let id = next_id;
                    next_id += 1;
                    write_specs.push(WriteSpec::Put(
                        make_key(id),
                        make_value(id, args.seed ^ done),
                    ));
                }
                for _ in 0..d_target {
                    let id = sample_existing_id(
                        &mut rng,
                        args.records,
                        args.access_pattern,
                        args.workload,
                    );
                    write_specs.push(WriteSpec::Delete(make_key(id)));
                }
                write_specs.shuffle(&mut rng);

                let mut puts = Vec::with_capacity(txn_size);
                let mut dels = Vec::with_capacity(txn_size);
                let mut batch_ops = 0usize;
                for spec in write_specs {
                    match spec {
                        WriteSpec::Put(k, v) => puts.push((k, v)),
                        WriteSpec::Delete(k) => dels.push(k),
                    }
                    batch_ops += 1;
                    if batch_ops == txn_size {
                        units.push(Unit::Write {
                            puts: std::mem::take(&mut puts),
                            deletes: std::mem::take(&mut dels),
                        });
                        batch_ops = 0;
                    }
                }
                if batch_ops != 0 {
                    units.push(Unit::Write {
                        puts,
                        deletes: dels,
                    });
                }
                units.shuffle(&mut rng);

                for unit in units {
                    match unit {
                        Unit::Read(k) => {
                            let t = Instant::now();
                            let _ = engine.get(&k).await?;
                            record(read_hist, t.elapsed());
                            reads += 1;
                            done += 1;
                        }
                        Unit::Write {
                            puts,
                            deletes: dels,
                        } => {
                            let n = puts.len() + dels.len();
                            let t = Instant::now();
                            engine.write_batch(args.durability, &puts, &dels).await?;
                            record(write_hist, t.elapsed());
                            writes += puts.len() as u64;
                            deletes += dels.len() as u64;
                            done += n as u64;
                        }
                    }
                }
            }
        }
    }
    Ok((done, reads, writes, deletes))
}

async fn measure_settle(path: &Path, args: &Args) -> Option<SettleMeasurement> {
    if args.settle_ms == 0 {
        return None;
    }
    let sample_ms = args.settle_sample_ms.max(1);
    let total_started = Instant::now();
    let total_proc_before = ProcSnapshot::capture();
    let total_sys_before = SystemSnapshot::capture();
    let db_bytes_before = dir_size(path);
    let mut samples = Vec::new();
    let mut prev_proc = total_proc_before.clone();
    let mut prev_sys = total_sys_before.clone();
    let mut prev_elapsed = Duration::ZERO;

    loop {
        let elapsed = total_started.elapsed();
        if elapsed.as_millis() as u64 >= args.settle_ms {
            break;
        }
        let remaining_ms = args
            .settle_ms
            .saturating_sub(elapsed.as_millis() as u64)
            .max(1);
        let sleep_ms = remaining_ms.min(sample_ms);
        tokio::time::sleep(Duration::from_millis(sleep_ms)).await;

        let now_elapsed = total_started.elapsed();
        let now_proc = ProcSnapshot::capture();
        let now_sys = SystemSnapshot::capture();
        let interval = now_elapsed.saturating_sub(prev_elapsed);
        samples.push(SettleSample {
            elapsed_ms: now_elapsed.as_millis() as u64,
            interval_ms: interval.as_millis() as u64,
            db_bytes: dir_size(path),
            process: prev_proc.delta(&now_proc, interval),
            system: prev_sys.delta(&now_sys),
        });
        prev_proc = now_proc;
        prev_sys = now_sys;
        prev_elapsed = now_elapsed;
    }

    let total_elapsed = total_started.elapsed();
    let total_proc_after = ProcSnapshot::capture();
    let total_sys_after = SystemSnapshot::capture();
    Some(SettleMeasurement {
        requested_ms: args.settle_ms,
        sample_ms,
        elapsed_s: total_elapsed.as_secs_f64(),
        db_bytes_before,
        db_bytes_after: dir_size(path),
        process: total_proc_before.delta(&total_proc_after, total_elapsed),
        system: total_sys_before.delta(&total_sys_after),
        samples,
    })
}

async fn verify_recovery(
    engine: &mut Engine,
    args: &Args,
    expected_prefix_records: u64,
    tail_records: u64,
) -> Result<Verification> {
    let started = Instant::now();
    let mut missing_prefix_records = 0u64;
    for id in 0..expected_prefix_records {
        if engine
            .get(&key(id, args.key_bytes, args.key_shape, args.seed))
            .await?
            .is_none()
        {
            missing_prefix_records += 1;
        }
    }

    let mut tail_prefix_present = 0u64;
    let mut tail_total_present = 0u64;
    let mut tail_present_after_gap = 0u64;
    let mut gap_seen = false;
    for id in expected_prefix_records..expected_prefix_records.saturating_add(tail_records) {
        let present = engine
            .get(&key(id, args.key_bytes, args.key_shape, args.seed))
            .await?
            .is_some();
        if present {
            tail_total_present += 1;
            if gap_seen {
                tail_present_after_gap += 1;
            } else {
                tail_prefix_present += 1;
            }
        } else {
            gap_seen = true;
        }
    }

    let txn = args.txn_size.max(1) as u64;
    let transaction_atomic_tail = tail_prefix_present == 0 || tail_prefix_present % txn == 0;
    let verification_ok =
        missing_prefix_records == 0 && tail_present_after_gap == 0 && transaction_atomic_tail;
    Ok(Verification {
        expected_prefix_records,
        checked_prefix_records: expected_prefix_records,
        missing_prefix_records,
        tail_checked_records: tail_records,
        tail_prefix_present,
        tail_total_present,
        tail_present_after_gap,
        transaction_atomic_tail,
        verification_ok,
        verify_s: started.elapsed().as_secs_f64(),
    })
}

#[tokio::main(flavor = "multi_thread", worker_threads = 1)]
async fn main() -> Result<()> {
    let args = Args::parse();
    if args.records == 0 {
        bail!("--records must be greater than zero");
    }
    if args.key_bytes < 8 {
        bail!("--key-bytes must be at least 8");
    }
    if args.miss_percent > 100 {
        bail!("--miss-percent must be in 0..=100");
    }
    if args.key_shape == KeyShape::Hashed && matches!(args.workload, Workload::RangeScan) {
        bail!("--key-shape hashed is incompatible with ID-ordered range-scan");
    }
    if (args.progress_file.is_some() || args.verify_prefix_records.is_some())
        && args.write_pattern != WritePattern::Append
    {
        bail!("recovery progress/verification requires --write-pattern append");
    }

    let mapping = args.engine.durability_mapping(args.durability)?;
    let run_name = format!(
        "{:?}-{:?}-{:?}-n{}-k{}-{:?}-v{}-{:?}-tx{}-scan{}-trial{}",
        args.engine,
        args.durability,
        args.workload,
        args.records,
        args.key_bytes,
        args.key_shape,
        args.value_bytes,
        args.value_pattern,
        args.txn_size,
        args.scan_len,
        args.trial
    )
    .to_lowercase()
    .replace('_', "-");
    let path = if let Some(name) = &args.db_name {
        let candidate = Path::new(name);
        if candidate.components().count() != 1 || name == "." || name == ".." {
            bail!("--db-name must be one plain path component");
        }
        args.root.join(candidate)
    } else {
        args.root.join(run_name)
    };
    if path.exists() && !args.reuse_db {
        fs::remove_dir_all(&path).context("remove stale run directory")?;
    }
    if args.reuse_db && !path.exists() {
        bail!(
            "--reuse-db requested but database path does not exist: {}",
            path.display()
        );
    }
    fs::create_dir_all(&path)?;

    let open_started = Instant::now();
    let mut engine = Engine::open(args.engine, args.durability, &path).await?;
    let open_s = open_started.elapsed().as_secs_f64();
    let prefill_s = if args.skip_prefill {
        0.0
    } else {
        let prefill_started = Instant::now();
        prefill(&mut engine, &args).await?;
        prefill_started.elapsed().as_secs_f64()
    };
    let verification = if let Some(expected_prefix_records) = args.verify_prefix_records {
        Some(
            verify_recovery(
                &mut engine,
                &args,
                expected_prefix_records,
                args.verify_tail_records,
            )
            .await?,
        )
    } else {
        None
    };

    let warmup_started = Instant::now();
    warm_reads(&mut engine, &args).await?;
    let warmup_s = warmup_started.elapsed().as_secs_f64();

    let mut read_hist = hist();
    let mut write_hist = hist();
    let system_before = SystemSnapshot::capture();
    let process_before = ProcSnapshot::capture();
    let started = Instant::now();
    let (completed, reads, writes, deletes) =
        run_workload(&mut engine, &args, &mut read_hist, &mut write_hist).await?;
    let elapsed = started.elapsed();
    let process_after = ProcSnapshot::capture();
    let system_after = SystemSnapshot::capture();
    let measured_process = process_before.delta(&process_after, elapsed);
    let measured_system_delta = system_before.delta(&system_after);
    let post_workload_settle = measure_settle(&path, &args).await;
    engine.close().await?;
    drop(engine);
    let db_bytes = dir_size(&path);

    let result = Measurement {
        format_version: 3,
        engine: args.engine,
        engine_version: args.engine.version(),
        durability: args.durability,
        durability_mapping: mapping,
        workload: args.workload,
        records: args.records,
        ops_requested: args.ops,
        ops_completed: completed,
        value_bytes: args.value_bytes,
        value_pattern: args.value_pattern,
        key_bytes: args.key_bytes,
        key_shape: args.key_shape,
        access_pattern: args.access_pattern,
        miss_percent: args.miss_percent,
        write_pattern: args.write_pattern,
        txn_size: args.txn_size,
        scan_len: args.scan_len,
        trial: args.trial,
        seed: args.seed,
        scenario: args.scenario.clone(),
        open_s,
        prefill_s,
        warmup_s,
        elapsed_s: elapsed.as_secs_f64(),
        ops_per_s: completed as f64 / elapsed.as_secs_f64(),
        reads,
        writes,
        deletes,
        read_latency: quantiles(&read_hist),
        write_txn_latency: quantiles(&write_hist),
        db_bytes,
        post_workload_settle,
        peak_rss_kib: peak_rss_kib(),
        measured_process,
        system_before,
        system_after,
        measured_system_delta,
        path: path.display().to_string(),
        db_kept: args.keep_db,
        reused_db: args.reuse_db,
        prefill_skipped: args.skip_prefill,
        warmup_reads: args.warmup_reads,
        verification,
    };

    let line = serde_json::to_string(&result)?;
    if args.output == "-" {
        println!("{line}");
    } else {
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&args.output)?;
        writeln!(f, "{line}")?;
    }
    if !args.keep_db {
        fs::remove_dir_all(&path).context("remove completed run database")?;
    }
    Ok(())
}
