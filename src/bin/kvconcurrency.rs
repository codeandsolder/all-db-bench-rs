#![allow(clippy::too_many_lines)]

#[allow(dead_code)]
#[path = "kvbench.rs"]
mod base;

use anyhow::{Context, Result, bail};
use base::{
    AccessPattern, Durability, Engine, EngineKind, KeyShape, ValuePattern, Workload, WritePattern,
};
use clap::Parser;
use hdrhistogram::Histogram;
use rand::{Rng, SeedableRng, rngs::SmallRng, seq::SliceRandom};
use serde::Serialize;
use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Barrier, Condvar, Mutex, mpsc},
    thread,
    time::Instant,
};

use fjall::{Database as FjallDb, Keyspace, PersistMode};
use heed::{Database as HeedDb, Env as HeedEnv, types::Bytes};
use manifold::{
    Durability as ManifoldDurability, TableDefinition as ManifoldTableDefinition,
    column_family::{ColumnFamily as ManifoldCf, ColumnFamilyDatabase as ManifoldDb},
};
use parity_db::Db as ParityDb;
use redb::{Database as RedbDb, Durability as RedbDurability, ReadableDatabase, TableDefinition};
use surrealkv::{
    Durability as SurrealDurability, Error as SurrealError, LSMIterator, Mode as SurrealMode,
    Tree as SurrealTree,
};
use turbokv::{Db as TurboDb, WriteBatch as TurboBatch};

#[cfg(feature = "kv-external")]
use libmdbx::{
    Database as MdbxDb, NoWriteMap, TableFlags as MdbxTableFlags, WriteFlags as MdbxWriteFlags,
};
#[cfg(feature = "kv-external")]
use persy::{ByteVec as PersyByteVec, Persy, TransactionConfig as PersyTxConfig};
#[cfg(feature = "kv-external")]
use rocksdb::{
    DB as RocksDb, Direction as RocksDirection, IteratorMode as RocksIteratorMode,
    WriteBatch as RocksBatch, WriteOptions as RocksWriteOptions,
};

#[cfg(feature = "kv-experimental")]
use jammdb::{DB as JammDb, Data as JammData};
#[cfg(feature = "kv-experimental")]
use lsm_db::{Batch as LsmBatch, Lsm as LsmDb};
#[cfg(feature = "kv-experimental")]
use roughdb::{
    Db as RoughDb, ReadOptions as RoughReadOptions, WriteBatch as RoughBatch,
    WriteOptions as RoughWriteOptions,
};

const REDB_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("kv");
const MANIFOLD_TABLE: ManifoldTableDefinition<&[u8], &[u8]> = ManifoldTableDefinition::new("kv");

#[derive(Clone, Debug, Parser)]
#[command(about = "Shared-database multi-client scaling benchmark")]
struct Args {
    #[arg(long, value_enum)]
    engine: EngineKind,
    #[arg(long, value_enum)]
    durability: Durability,
    #[arg(long, value_enum)]
    workload: Workload,
    #[arg(long, default_value_t = 100_000)]
    records: u64,
    /// Total logical operations across all clients, not operations per client.
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
    clients: usize,
    #[arg(long, default_value_t = 1)]
    trial: u32,
    #[arg(long, default_value_t = 0x5eed_2026)]
    seed: u64,
    #[arg(long, default_value = "concurrency")]
    scenario: String,
    #[arg(long, default_value = "data")]
    root: PathBuf,
    #[arg(long, default_value = "-")]
    output: String,
    #[arg(long, default_value_t = false)]
    keep_db: bool,
    #[arg(long, default_value_t = 5_000)]
    warmup_reads: u64,
}

#[derive(Debug, Serialize)]
struct ClientMeasurement {
    client: usize,
    ops_requested: u64,
    ops_completed: u64,
    elapsed_s: f64,
    ops_per_s: f64,
    reads: u64,
    writes: u64,
    deletes: u64,
    write_conflict_retries: u64,
    read_latency: base::Quantiles,
    write_txn_latency: base::Quantiles,
}

#[derive(Debug)]
struct ClientRun {
    client: usize,
    ops_requested: u64,
    ops_completed: u64,
    elapsed_s: f64,
    reads: u64,
    writes: u64,
    deletes: u64,
    write_conflict_retries: u64,
    read_hist: Histogram<u64>,
    write_hist: Histogram<u64>,
}

#[derive(Debug, Serialize)]
struct Measurement {
    format_version: u32,
    lane: &'static str,
    engine: EngineKind,
    engine_version: &'static str,
    durability: Durability,
    durability_mapping: &'static str,
    workload: Workload,
    records: u64,
    ops_requested: u64,
    ops_completed: u64,
    clients: usize,
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
    write_conflict_retries: u64,
    read_latency: base::Quantiles,
    write_txn_latency: base::Quantiles,
    client_elapsed_s_min: f64,
    client_elapsed_s_median: f64,
    client_elapsed_s_max: f64,
    client_ops_per_s_min: f64,
    client_ops_per_s_median: f64,
    client_ops_per_s_max: f64,
    client_throughput_max_min_ratio: f64,
    client_measurements: Vec<ClientMeasurement>,
    db_bytes: u64,
    peak_rss_kib: u64,
    measured_process: base::metrics::ProcDelta,
    system_before: base::metrics::SystemSnapshot,
    system_after: base::metrics::SystemSnapshot,
    measured_system_delta: base::metrics::SystemDelta,
    path: String,
    db_kept: bool,
    warmup_reads: u64,
    concurrency_handle: &'static str,
}

#[allow(async_fn_in_trait)]
trait ClientOps: Clone + Send + 'static {
    async fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>>;
    async fn write_batch(
        &mut self,
        durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
    ) -> Result<u64>;
    async fn scan_count(&mut self, start: &[u8], end: &[u8]) -> Result<usize>;
}

#[derive(Clone)]
struct RedbClient(Arc<RedbDb>);

impl ClientOps for RedbClient {
    async fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let txn = self.0.begin_read()?;
        let table = txn.open_table(REDB_TABLE)?;
        Ok(table.get(key)?.map(|v| v.value().to_vec()))
    }

    async fn write_batch(
        &mut self,
        durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
    ) -> Result<u64> {
        let mut txn = self.0.begin_write()?;
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
        Ok(0)
    }

    async fn scan_count(&mut self, start: &[u8], end: &[u8]) -> Result<usize> {
        let txn = self.0.begin_read()?;
        let table = txn.open_table(REDB_TABLE)?;
        let mut n = 0usize;
        for item in table.range(start..end)? {
            let _ = item?;
            n += 1;
        }
        Ok(n)
    }
}

#[derive(Clone)]
struct FjallClient {
    db: FjallDb,
    keyspace: Keyspace,
}

impl ClientOps for FjallClient {
    async fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.keyspace.get(key)?.map(|v| v.to_vec()))
    }

    async fn write_batch(
        &mut self,
        durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
    ) -> Result<u64> {
        let mut batch = self.db.batch();
        for (k, v) in puts {
            batch.insert(&self.keyspace, k.as_slice(), v.as_slice());
        }
        for k in deletes {
            batch.remove(&self.keyspace, k.as_slice());
        }
        batch.commit()?;
        self.db.persist(match durability {
            Durability::Relaxed => PersistMode::Buffer,
            Durability::Sync => PersistMode::SyncAll,
        })?;
        Ok(0)
    }

    async fn scan_count(&mut self, start: &[u8], end: &[u8]) -> Result<usize> {
        let mut n = 0usize;
        for item in self.keyspace.range(start..end) {
            let _ = item.into_inner()?;
            n += 1;
        }
        Ok(n)
    }
}

#[derive(Clone)]
struct SurrealClient(SurrealTree);

impl ClientOps for SurrealClient {
    async fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let txn = self.0.begin_with_mode(SurrealMode::ReadOnly)?;
        Ok(txn.get(key)?.map(|v| v.to_vec()))
    }

    async fn write_batch(
        &mut self,
        durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
    ) -> Result<u64> {
        const MAX_CONFLICT_RETRIES: u64 = 10_000;
        let mut retries = 0u64;
        loop {
            let mut txn = self.0.begin()?;
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
            match txn.commit().await {
                Ok(()) => return Ok(retries),
                Err(SurrealError::TransactionWriteConflict | SurrealError::TransactionRetry)
                    if retries < MAX_CONFLICT_RETRIES =>
                {
                    retries += 1;
                    if retries <= 8 {
                        std::hint::spin_loop();
                    } else {
                        tokio::task::yield_now().await;
                    }
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    async fn scan_count(&mut self, start: &[u8], end: &[u8]) -> Result<usize> {
        let txn = self.0.begin_with_mode(SurrealMode::ReadOnly)?;
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
        Ok(n)
    }
}

#[derive(Clone)]
struct HeedClient {
    env: HeedEnv,
    db: HeedDb<Bytes, Bytes>,
}

impl ClientOps for HeedClient {
    async fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let txn = self.env.read_txn()?;
        Ok(self.db.get(&txn, key)?.map(ToOwned::to_owned))
    }

    async fn write_batch(
        &mut self,
        _durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
    ) -> Result<u64> {
        let mut txn = self.env.write_txn()?;
        for (k, v) in puts {
            self.db.put(&mut txn, k.as_slice(), v.as_slice())?;
        }
        for k in deletes {
            self.db.delete(&mut txn, k.as_slice())?;
        }
        txn.commit()?;
        Ok(0)
    }

    async fn scan_count(&mut self, start: &[u8], end: &[u8]) -> Result<usize> {
        let txn = self.env.read_txn()?;
        let range = (
            std::ops::Bound::Included(start),
            std::ops::Bound::Excluded(end),
        );
        let mut n = 0usize;
        for item in self.db.range(&txn, &range)? {
            let _ = item?;
            n += 1;
        }
        Ok(n)
    }
}

#[derive(Clone)]
struct SledClient(sled::Db);

impl ClientOps for SledClient {
    async fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.0.get(key)?.map(|v| v.to_vec()))
    }

    async fn write_batch(
        &mut self,
        durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
    ) -> Result<u64> {
        let mut batch = sled::Batch::default();
        for (k, v) in puts {
            batch.insert(k.as_slice(), v.as_slice());
        }
        for k in deletes {
            batch.remove(k.as_slice());
        }
        self.0.apply_batch(batch)?;
        if durability == Durability::Sync {
            self.0.flush()?;
        }
        Ok(0)
    }

    async fn scan_count(&mut self, start: &[u8], end: &[u8]) -> Result<usize> {
        let mut n = 0usize;
        for item in self.0.range(start..end) {
            let _ = item?;
            n += 1;
        }
        Ok(n)
    }
}

#[derive(Clone)]
struct ManifoldClient {
    _db: Arc<ManifoldDb>,
    cf: ManifoldCf,
}

impl ClientOps for ManifoldClient {
    async fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let txn = self.cf.begin_read()?;
        let table = txn.open_table(MANIFOLD_TABLE)?;
        Ok(table.get(key)?.map(|v| v.value().to_vec()))
    }

    async fn write_batch(
        &mut self,
        _durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
    ) -> Result<u64> {
        let mut txn = self.cf.begin_write()?;
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
        Ok(0)
    }

    async fn scan_count(&mut self, start: &[u8], end: &[u8]) -> Result<usize> {
        let txn = self.cf.begin_read()?;
        let table = txn.open_table(MANIFOLD_TABLE)?;
        let mut n = 0usize;
        for item in table.range(start..end)? {
            let _ = item?;
            n += 1;
        }
        Ok(n)
    }
}

#[derive(Clone)]
struct TurboClient(Arc<TurboDb>);

impl ClientOps for TurboClient {
    async fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.0.get(key).await?)
    }

    async fn write_batch(
        &mut self,
        _durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
    ) -> Result<u64> {
        let mut batch = TurboBatch::new();
        for (k, v) in puts {
            batch.put(k.as_slice(), v.as_slice());
        }
        for k in deletes {
            batch.delete(k.as_slice());
        }
        self.0.write_batch(&batch).await?;
        Ok(0)
    }

    async fn scan_count(&mut self, start: &[u8], end: &[u8]) -> Result<usize> {
        Ok(self.0.range(start, end).await?.len())
    }
}

#[derive(Clone)]
struct ParityClient {
    db: Arc<ParityDb>,
    ordered: bool,
}

impl ClientOps for ParityClient {
    async fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.db.get(0, key)?)
    }

    async fn write_batch(
        &mut self,
        _durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
    ) -> Result<u64> {
        let mut changes = Vec::with_capacity(puts.len() + deletes.len());
        for (k, v) in puts {
            changes.push((0u8, k.clone(), Some(v.clone())));
        }
        for k in deletes {
            changes.push((0u8, k.clone(), None));
        }
        self.db.commit(changes)?;
        Ok(0)
    }

    async fn scan_count(&mut self, start: &[u8], end: &[u8]) -> Result<usize> {
        if !self.ordered {
            bail!("ParityDB hash-column mode has no ordered range iterator");
        }
        let mut iter = self.db.iter(0)?;
        iter.seek(start)?;
        let mut n = 0usize;
        while let Some((k, _v)) = iter.next()? {
            if k.as_slice() >= end {
                break;
            }
            n += 1;
        }
        Ok(n)
    }
}

#[cfg(feature = "kv-external")]
#[derive(Clone)]
struct RocksClient(Arc<RocksDb>);

#[cfg(feature = "kv-external")]
impl ClientOps for RocksClient {
    async fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.0.get(key)?.map(|v| v.to_vec()))
    }

    async fn write_batch(
        &mut self,
        durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
    ) -> Result<u64> {
        let mut batch = RocksBatch::default();
        for (k, v) in puts {
            batch.put(k, v);
        }
        for k in deletes {
            batch.delete(k);
        }
        let mut opts = RocksWriteOptions::default();
        opts.set_sync(durability == Durability::Sync);
        self.0.write_opt(batch, &opts)?;
        Ok(0)
    }

    async fn scan_count(&mut self, start: &[u8], end: &[u8]) -> Result<usize> {
        let mut n = 0usize;
        for item in self
            .0
            .iterator(RocksIteratorMode::From(start, RocksDirection::Forward))
        {
            let (k, _) = item?;
            if k.as_ref() >= end {
                break;
            }
            n += 1;
        }
        Ok(n)
    }
}

#[cfg(feature = "kv-external")]
#[derive(Clone)]
struct MdbxClient(Arc<MdbxDb<NoWriteMap>>);

#[cfg(feature = "kv-external")]
impl ClientOps for MdbxClient {
    async fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let txn = self.0.begin_ro_txn()?;
        let table = txn.open_table(None)?;
        Ok(txn.get::<Vec<u8>>(&table, key)?)
    }

    async fn write_batch(
        &mut self,
        _durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
    ) -> Result<u64> {
        let txn = self.0.begin_rw_txn()?;
        let table = txn.create_table(None, MdbxTableFlags::default())?;
        for (k, v) in puts {
            txn.put(&table, k, v, MdbxWriteFlags::UPSERT)?;
        }
        for k in deletes {
            let _ = txn.del(&table, k, None)?;
        }
        txn.commit()?;
        Ok(0)
    }

    async fn scan_count(&mut self, start: &[u8], end: &[u8]) -> Result<usize> {
        let txn = self.0.begin_ro_txn()?;
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
        Ok(n)
    }
}

#[cfg(feature = "kv-external")]
#[derive(Clone)]
struct PersyClient(Persy);

#[cfg(feature = "kv-external")]
impl ClientOps for PersyClient {
    async fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let key = PersyByteVec::from(key.to_vec());
        Ok(self
            .0
            .one::<PersyByteVec, PersyByteVec>("kv", &key)?
            .map(Vec::<u8>::from))
    }

    async fn write_batch(
        &mut self,
        durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
    ) -> Result<u64> {
        let mut tx = match durability {
            Durability::Relaxed => self
                .0
                .begin_with(PersyTxConfig::new().set_background_sync(true))?,
            Durability::Sync => self.0.begin()?,
        };
        for (k, v) in puts {
            tx.put::<PersyByteVec, PersyByteVec>(
                "kv",
                PersyByteVec::from(k.clone()),
                PersyByteVec::from(v.clone()),
            )?;
        }
        for k in deletes {
            tx.remove::<PersyByteVec, PersyByteVec>("kv", PersyByteVec::from(k.clone()), None)?;
        }
        tx.prepare()?.commit()?;
        Ok(0)
    }

    async fn scan_count(&mut self, start: &[u8], end: &[u8]) -> Result<usize> {
        let range = PersyByteVec::from(start.to_vec())..PersyByteVec::from(end.to_vec());
        let mut n = 0usize;
        for (_k, values) in self.0.range::<PersyByteVec, PersyByteVec, _>("kv", range)? {
            if values.into_iter().next().is_some() {
                n += 1;
            }
        }
        Ok(n)
    }
}

#[cfg(feature = "kv-experimental")]
#[derive(Clone)]
struct RoughClient(Arc<RoughDb>);

#[cfg(feature = "kv-experimental")]
impl ClientOps for RoughClient {
    async fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        match self.0.get(key) {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn write_batch(
        &mut self,
        durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
    ) -> Result<u64> {
        let mut batch = RoughBatch::new();
        for (k, v) in puts {
            batch.put(k, v);
        }
        for k in deletes {
            batch.delete(k);
        }
        self.0.write(
            &RoughWriteOptions {
                sync: durability == Durability::Sync,
            },
            batch,
        )?;
        Ok(0)
    }

    async fn scan_count(&mut self, start: &[u8], end: &[u8]) -> Result<usize> {
        let mut iter = self.0.new_iterator(&RoughReadOptions::default())?;
        iter.seek(start);
        let mut n = 0usize;
        for item in iter.forward() {
            let (k, _) = item?;
            if k.as_slice() >= end {
                break;
            }
            n += 1;
        }
        Ok(n)
    }
}

#[cfg(feature = "kv-experimental")]
#[derive(Clone)]
struct JammClient(JammDb);

#[cfg(feature = "kv-experimental")]
impl ClientOps for JammClient {
    async fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let tx = self.0.tx(false)?;
        let bucket = tx.get_bucket("kv")?;
        Ok(match bucket.get(key) {
            Some(JammData::KeyValue(kv)) => Some(kv.value().to_vec()),
            Some(JammData::Bucket(_)) => bail!("jammdb KV bucket contains nested bucket"),
            None => None,
        })
    }

    async fn write_batch(
        &mut self,
        durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
    ) -> Result<u64> {
        if durability == Durability::Relaxed {
            bail!("jammdb relaxed mode unsupported");
        }
        let tx = self.0.tx(true)?;
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
        Ok(0)
    }

    async fn scan_count(&mut self, start: &[u8], end: &[u8]) -> Result<usize> {
        let tx = self.0.tx(false)?;
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
        Ok(n)
    }
}

#[cfg(feature = "kv-experimental")]
#[derive(Clone)]
struct LsmClient(Arc<LsmDb>);

#[cfg(feature = "kv-experimental")]
impl ClientOps for LsmClient {
    async fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.0.get(key)?)
    }

    async fn write_batch(
        &mut self,
        durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
    ) -> Result<u64> {
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
        self.0.write(batch)?;
        Ok(0)
    }

    async fn scan_count(&mut self, start: &[u8], end: &[u8]) -> Result<usize> {
        Ok(self.0.scan(start.to_vec()..end.to_vec())?.count())
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
            base::sample_existing_id(rng, args.records, args.access_pattern, args.workload),
            true,
        )
    }
}

async fn prefill(engine: &mut Engine, args: &Args) -> Result<()> {
    const BATCH: usize = 10_000;
    let mut puts = Vec::with_capacity(BATCH);
    for id in 0..args.records {
        puts.push((
            base::key(id, args.key_bytes, args.key_shape, args.seed),
            base::value(id, args.value_bytes, args.seed, args.value_pattern),
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
        let id =
            base::sample_existing_id(&mut rng, args.records, args.access_pattern, args.workload);
        let _ = engine
            .get(&base::key(id, args.key_bytes, args.key_shape, args.seed))
            .await?;
    }
    Ok(())
}

async fn run_client<C: ClientOps>(
    engine: &mut C,
    args: &Args,
    client: usize,
    ops: u64,
    offset: u64,
) -> Result<ClientRun> {
    let salt = base::mix_u64((client as u64).wrapping_add(0x636c_6965_6e74_0001));
    let mut rng = SmallRng::seed_from_u64(args.seed ^ u64::from(args.trial) ^ salt);
    let mut read_hist = base::hist();
    let mut write_hist = base::hist();
    let mut reads = 0u64;
    let mut writes = 0u64;
    let mut deletes = 0u64;
    let mut write_conflict_retries = 0u64;
    let mut done = 0u64;
    let txn_size = args.txn_size.max(1);
    let mut next_id = args.records.saturating_add(offset);
    let make_key = |id| base::key(id, args.key_bytes, args.key_shape, args.seed);
    let make_value = |id, salt| base::value(id, args.value_bytes, salt, args.value_pattern);

    let started = Instant::now();
    match args.workload {
        Workload::PointRead => {
            for _ in 0..ops {
                let (id, expect_hit) = sample_read_id(&mut rng, args);
                let t = Instant::now();
                let got = engine.get(&make_key(id)).await?;
                base::record(&mut read_hist, t.elapsed());
                if got.is_some() != expect_hit {
                    bail!("client {client} point-read mismatch id={id} expected_hit={expect_hit}");
                }
                reads += 1;
                done += 1;
            }
        }
        Workload::RangeScan => {
            if matches!(args.engine, EngineKind::Lkv | EngineKind::ParitydbHash) {
                bail!("selected engine has no ordered keyed range-scan API");
            }
            if args.key_shape == KeyShape::Hashed {
                bail!("hashed keys have no ID-ordered range semantics");
            }
            if args.scan_len == 0 || args.scan_len > args.records {
                bail!("scan_len must be in 1..=records");
            }
            let last_start = args.records - args.scan_len;
            for _ in 0..ops {
                let start_id = if last_start == 0 {
                    0
                } else {
                    rng.random_range(0..=last_start)
                };
                let end_id = start_id + args.scan_len;
                let t = Instant::now();
                let n = engine
                    .scan_count(&make_key(start_id), &make_key(end_id))
                    .await?;
                base::record(&mut read_hist, t.elapsed());
                if n != args.scan_len as usize {
                    bail!(
                        "client {client} range scan returned {n}, expected {}",
                        args.scan_len
                    );
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
            while done < ops {
                let n = (ops - done).min(batch as u64) as usize;
                let mut puts = Vec::with_capacity(n);
                for local in 0..n {
                    let id = match args.write_pattern {
                        WritePattern::Append => {
                            let id = next_id;
                            next_id += 1;
                            id
                        }
                        WritePattern::UpdateUniform => rng.random_range(0..args.records),
                        WritePattern::UpdateHot => base::sample_existing_id(
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
                    puts.push((
                        make_key(id),
                        make_value(id, args.seed ^ 0x1111 ^ offset ^ done ^ local as u64),
                    ));
                }
                let t = Instant::now();
                write_conflict_retries = write_conflict_retries
                    .saturating_add(engine.write_batch(args.durability, &puts, &[]).await?);
                base::record(&mut write_hist, t.elapsed());
                writes += n as u64;
                done += n as u64;
            }
        }
        Workload::DeleteBurst => {
            if args.ops > args.records {
                bail!("delete-burst requires total ops <= records");
            }
            while done < ops {
                let n = (ops - done).min(txn_size as u64) as usize;
                let mut dels = Vec::with_capacity(n);
                for local in 0..n {
                    dels.push(make_key(offset + done + local as u64));
                }
                let t = Instant::now();
                write_conflict_retries = write_conflict_retries
                    .saturating_add(engine.write_batch(args.durability, &[], &dels).await?);
                base::record(&mut write_hist, t.elapsed());
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

            while done < ops {
                let epoch = (ops - done).min(1_000);
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

                let mut specs = Vec::with_capacity((u_target + i_target + d_target) as usize);
                for _ in 0..u_target {
                    let id = base::sample_existing_id(
                        &mut rng,
                        args.records,
                        args.access_pattern,
                        args.workload,
                    );
                    specs.push(WriteSpec::Put(
                        make_key(id),
                        make_value(id, args.seed ^ offset ^ done),
                    ));
                }
                for _ in 0..i_target {
                    let id = next_id;
                    next_id += 1;
                    specs.push(WriteSpec::Put(
                        make_key(id),
                        make_value(id, args.seed ^ offset ^ done),
                    ));
                }
                for _ in 0..d_target {
                    let id = base::sample_existing_id(
                        &mut rng,
                        args.records,
                        args.access_pattern,
                        args.workload,
                    );
                    specs.push(WriteSpec::Delete(make_key(id)));
                }
                specs.shuffle(&mut rng);

                let mut puts = Vec::with_capacity(txn_size);
                let mut dels = Vec::with_capacity(txn_size);
                let mut batch_ops = 0usize;
                for spec in specs {
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
                            base::record(&mut read_hist, t.elapsed());
                            reads += 1;
                            done += 1;
                        }
                        Unit::Write {
                            puts,
                            deletes: dels,
                        } => {
                            let n = puts.len() + dels.len();
                            let t = Instant::now();
                            write_conflict_retries = write_conflict_retries.saturating_add(
                                engine.write_batch(args.durability, &puts, &dels).await?,
                            );
                            base::record(&mut write_hist, t.elapsed());
                            writes += puts.len() as u64;
                            deletes += dels.len() as u64;
                            done += n as u64;
                        }
                    }
                }
            }
        }
    }

    Ok(ClientRun {
        client,
        ops_requested: ops,
        ops_completed: done,
        elapsed_s: started.elapsed().as_secs_f64(),
        reads,
        writes,
        deletes,
        write_conflict_retries,
        read_hist,
        write_hist,
    })
}

struct AggregateRun {
    completed: u64,
    reads: u64,
    writes: u64,
    deletes: u64,
    write_conflict_retries: u64,
    read_hist: Histogram<u64>,
    write_hist: Histogram<u64>,
    clients: Vec<ClientRun>,
    elapsed_s: f64,
    process: base::metrics::ProcDelta,
    system_before: base::metrics::SystemSnapshot,
    system_after: base::metrics::SystemSnapshot,
    system_delta: base::metrics::SystemDelta,
}

fn run_clients<C: ClientOps>(
    prototype: C,
    args: &Args,
    runtime: &tokio::runtime::Handle,
) -> Result<AggregateRun> {
    let start_barrier = Arc::new(Barrier::new(args.clients + 1));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let (tx, rx) = mpsc::channel::<(usize, Result<ClientRun>)>();
    let base_ops = args.ops / args.clients as u64;
    let extra = args.ops % args.clients as u64;
    let mut offset = 0u64;
    let mut joins = Vec::with_capacity(args.clients);

    for client in 0..args.clients {
        let ops = base_ops + u64::from((client as u64) < extra);
        let client_offset = offset;
        offset = offset.saturating_add(ops);

        let mut engine = prototype.clone();
        let thread_args = args.clone();
        let start_barrier = Arc::clone(&start_barrier);
        let release = Arc::clone(&release);
        let tx = tx.clone();
        let runtime = runtime.clone();
        joins.push(
            thread::Builder::new()
                .name(format!("dbbench-c{client}"))
                .spawn(move || {
                    let outcome = (|| -> Result<ClientRun> {
                        start_barrier.wait();
                        runtime.block_on(run_client(
                            &mut engine,
                            &thread_args,
                            client,
                            ops,
                            client_offset,
                        ))
                    })();
                    let _ = tx.send((client, outcome));

                    // Keep the task/TID alive until the coordinator has captured its
                    // process-after snapshot. This avoids losing schedstat deltas.
                    let (lock, cv) = &*release;
                    let mut released = lock.lock().unwrap_or_else(|e| e.into_inner());
                    while !*released {
                        released = cv.wait(released).unwrap_or_else(|e| e.into_inner());
                    }
                })?,
        );
    }
    drop(tx);

    let system_before = base::metrics::SystemSnapshot::capture();
    let process_before = base::metrics::ProcSnapshot::capture();
    let started = Instant::now();
    start_barrier.wait();

    let mut client_runs = Vec::with_capacity(args.clients);
    let receive_result: Result<()> = (|| {
        for _ in 0..args.clients {
            let (client, result) = rx
                .recv()
                .context("client result channel closed before all clients completed")?;
            client_runs.push(result.with_context(|| format!("client {client} failed"))?);
        }
        Ok(())
    })();
    let elapsed = started.elapsed();

    let process_after = base::metrics::ProcSnapshot::capture();
    let system_after = base::metrics::SystemSnapshot::capture();

    {
        let (lock, cv) = &*release;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cv.notify_all();
    }

    for join in joins {
        join.join()
            .map_err(|_| anyhow::anyhow!("concurrency client thread panicked"))?;
    }
    receive_result?;

    client_runs.sort_by_key(|r| r.client);
    let mut read_hist = base::hist();
    let mut write_hist = base::hist();
    let mut completed = 0u64;
    let mut reads = 0u64;
    let mut writes = 0u64;
    let mut deletes = 0u64;
    let mut write_conflict_retries = 0u64;
    for client in &client_runs {
        read_hist += &client.read_hist;
        write_hist += &client.write_hist;
        completed = completed.saturating_add(client.ops_completed);
        reads = reads.saturating_add(client.reads);
        writes = writes.saturating_add(client.writes);
        deletes = deletes.saturating_add(client.deletes);
        write_conflict_retries =
            write_conflict_retries.saturating_add(client.write_conflict_retries);
    }

    Ok(AggregateRun {
        completed,
        reads,
        writes,
        deletes,
        write_conflict_retries,
        read_hist,
        write_hist,
        clients: client_runs,
        elapsed_s: elapsed.as_secs_f64(),
        process: process_before.delta(&process_after, elapsed),
        system_delta: system_before.delta(&system_after),
        system_before,
        system_after,
    })
}

fn run_clients_without_starving_runtime<C: ClientOps>(
    prototype: C,
    args: &Args,
) -> Result<AggregateRun> {
    let runtime = tokio::runtime::Handle::current();
    tokio::task::block_in_place(|| run_clients(prototype, args, &runtime))
}

async fn dispatch(engine: Engine, args: &Args) -> Result<(&'static str, AggregateRun)> {
    match engine {
        Engine::Redb(db) => {
            let shared = Arc::new(db);
            Ok((
                "Arc<redb::Database>",
                run_clients_without_starving_runtime(RedbClient(shared), args)?,
            ))
        }
        Engine::Fjall { db, keyspace } => Ok((
            "native Clone handles",
            run_clients_without_starving_runtime(FjallClient { db, keyspace }, args)?,
        )),
        Engine::Surrealkv(tree) => Ok((
            "native Clone handle",
            run_clients_without_starving_runtime(SurrealClient(tree), args)?,
        )),
        Engine::Heed { env, db } => Ok((
            "native Clone Env/Database handles",
            run_clients_without_starving_runtime(HeedClient { env, db }, args)?,
        )),
        Engine::Sled(db) => Ok((
            "one native sled::Db clone per client; handle is Send+Clone but !Sync",
            run_clients_without_starving_runtime(SledClient(db), args)?,
        )),
        Engine::Lkv(_) => bail!(
            "lkv concurrency unsupported: write transactions require &mut Database and 0.2.1 exposes no cloneable/shared writer handle; benchmark-side mutex would fake concurrency"
        ),
        Engine::Manifold { _db, cf } => {
            let shared = Arc::new(_db);
            Ok((
                "shared Arc<ColumnFamilyDatabase> owner + native ColumnFamily clones",
                run_clients_without_starving_runtime(ManifoldClient { _db: shared, cf }, args)?,
            ))
        }
        Engine::Turbokv(mut slot) => {
            let db = slot.take().context("TurboKV missing database handle")?;
            let shared = Arc::new(db);
            let run = run_clients_without_starving_runtime(TurboClient(Arc::clone(&shared)), args)?;
            let db = Arc::try_unwrap(shared)
                .map_err(|_| anyhow::anyhow!("TurboKV client retained shared handle"))?;
            db.close().await?;
            Ok(("Arc<turbokv::Db>", run))
        }
        Engine::ParitydbHash(db) => Ok((
            "Arc<parity_db::Db> hash column",
            run_clients_without_starving_runtime(
                ParityClient {
                    db: Arc::new(db),
                    ordered: false,
                },
                args,
            )?,
        )),
        Engine::ParitydbBtree(db) => Ok((
            "Arc<parity_db::Db> B-tree column",
            run_clients_without_starving_runtime(
                ParityClient {
                    db: Arc::new(db),
                    ordered: true,
                },
                args,
            )?,
        )),
        #[cfg(feature = "kv-external")]
        Engine::Rocksdb(db) => Ok((
            "Arc<rocksdb::DB>",
            run_clients_without_starving_runtime(RocksClient(Arc::new(db)), args)?,
        )),
        #[cfg(feature = "kv-external")]
        Engine::Mdbx(db) => Ok((
            "Arc<libmdbx::Database>",
            run_clients_without_starving_runtime(MdbxClient(Arc::new(db)), args)?,
        )),
        #[cfg(feature = "kv-external")]
        Engine::Persy(db) => Ok((
            "native Persy clone per client",
            run_clients_without_starving_runtime(PersyClient(db), args)?,
        )),
        #[cfg(feature = "kv-experimental")]
        Engine::Roughdb(db) => Ok((
            "Arc<roughdb::Db>",
            run_clients_without_starving_runtime(RoughClient(Arc::new(db)), args)?,
        )),
        #[cfg(feature = "kv-experimental")]
        Engine::Jammdb(db) => Ok((
            "native jammdb::DB clone per client",
            run_clients_without_starving_runtime(JammClient(db), args)?,
        )),
        #[cfg(feature = "kv-experimental")]
        Engine::Lsmdb(db) => Ok((
            "Arc<lsm_db::Lsm>",
            run_clients_without_starving_runtime(LsmClient(Arc::new(db)), args)?,
        )),
    }
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    let n = values.len();
    if n % 2 == 0 {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    } else {
        values[n / 2]
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    if args.records == 0 {
        bail!("--records must be greater than zero");
    }
    if args.ops == 0 {
        bail!("--ops must be greater than zero");
    }
    if args.clients == 0 || args.clients as u64 > args.ops {
        bail!("--clients must be in 1..=ops");
    }
    if args.key_bytes < 8 {
        bail!("--key-bytes must be at least 8");
    }
    if args.miss_percent > 100 {
        bail!("--miss-percent must be in 0..=100");
    }
    if args.key_shape == KeyShape::Hashed && matches!(args.workload, Workload::RangeScan) {
        bail!("hashed keys are incompatible with ID-ordered range-scan");
    }
    if matches!(args.workload, Workload::DeleteBurst) && args.ops > args.records {
        bail!("delete-burst requires total ops <= records");
    }
    if matches!(args.engine, EngineKind::Lkv) {
        bail!(
            "lkv concurrency unsupported: no native shared/cloneable writer handle; external serialization is intentionally not benchmarked"
        );
    }

    let durability_mapping = args.engine.durability_mapping(args.durability)?;
    let run_name = format!(
        "{:?}-{:?}-{:?}-c{}-n{}-k{}-{:?}-v{}-{:?}-tx{}-scan{}-trial{}",
        args.engine,
        args.durability,
        args.workload,
        args.clients,
        args.records,
        args.key_bytes,
        args.key_shape,
        args.value_bytes,
        args.value_pattern,
        args.txn_size,
        args.scan_len,
        args.trial,
    )
    .to_lowercase()
    .replace('_', "-");
    let path = args.root.join(run_name);
    if path.exists() {
        fs::remove_dir_all(&path).context("remove stale concurrency run directory")?;
    }
    fs::create_dir_all(&path)?;

    let open_started = Instant::now();
    let mut engine = Engine::open(args.engine, args.durability, &path).await?;
    let open_s = open_started.elapsed().as_secs_f64();

    let prefill_started = Instant::now();
    prefill(&mut engine, &args).await?;
    let prefill_s = prefill_started.elapsed().as_secs_f64();

    let warmup_started = Instant::now();
    warm_reads(&mut engine, &args).await?;
    let warmup_s = warmup_started.elapsed().as_secs_f64();

    let (concurrency_handle, run) = dispatch(engine, &args).await?;
    if run.completed != args.ops {
        bail!(
            "concurrency run completed {} operations, expected {}",
            run.completed,
            args.ops
        );
    }

    let db_bytes = base::dir_size(&path);
    let mut client_elapsed = Vec::with_capacity(run.clients.len());
    let mut client_rates = Vec::with_capacity(run.clients.len());
    let mut client_measurements = Vec::with_capacity(run.clients.len());
    for client in run.clients {
        let rate = client.ops_completed as f64 / client.elapsed_s.max(f64::MIN_POSITIVE);
        client_elapsed.push(client.elapsed_s);
        client_rates.push(rate);
        client_measurements.push(ClientMeasurement {
            client: client.client,
            ops_requested: client.ops_requested,
            ops_completed: client.ops_completed,
            elapsed_s: client.elapsed_s,
            ops_per_s: rate,
            reads: client.reads,
            writes: client.writes,
            deletes: client.deletes,
            write_conflict_retries: client.write_conflict_retries,
            read_latency: base::quantiles(&client.read_hist),
            write_txn_latency: base::quantiles(&client.write_hist),
        });
    }
    let elapsed_min = client_elapsed.iter().copied().fold(f64::INFINITY, f64::min);
    let elapsed_max = client_elapsed.iter().copied().fold(0.0, f64::max);
    let rate_min = client_rates.iter().copied().fold(f64::INFINITY, f64::min);
    let rate_max = client_rates.iter().copied().fold(0.0, f64::max);

    let result = Measurement {
        format_version: 5,
        lane: "kv-concurrency",
        engine: args.engine,
        engine_version: args.engine.version(),
        durability: args.durability,
        durability_mapping,
        workload: args.workload,
        records: args.records,
        ops_requested: args.ops,
        ops_completed: run.completed,
        clients: args.clients,
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
        elapsed_s: run.elapsed_s,
        ops_per_s: run.completed as f64 / run.elapsed_s.max(f64::MIN_POSITIVE),
        reads: run.reads,
        writes: run.writes,
        deletes: run.deletes,
        write_conflict_retries: run.write_conflict_retries,
        read_latency: base::quantiles(&run.read_hist),
        write_txn_latency: base::quantiles(&run.write_hist),
        client_elapsed_s_min: elapsed_min,
        client_elapsed_s_median: median(client_elapsed),
        client_elapsed_s_max: elapsed_max,
        client_ops_per_s_min: rate_min,
        client_ops_per_s_median: median(client_rates),
        client_ops_per_s_max: rate_max,
        client_throughput_max_min_ratio: rate_max / rate_min.max(f64::MIN_POSITIVE),
        client_measurements,
        db_bytes,
        peak_rss_kib: base::peak_rss_kib(),
        measured_process: run.process,
        system_before: run.system_before,
        system_after: run.system_after,
        measured_system_delta: run.system_delta,
        path: path.display().to_string(),
        db_kept: args.keep_db,
        warmup_reads: args.warmup_reads,
        concurrency_handle,
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
        fs::remove_dir_all(&path).context("remove completed concurrency database")?;
    }
    Ok(())
}
