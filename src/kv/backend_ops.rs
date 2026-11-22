use anyhow::{Result, bail};
use std::{
    borrow::Borrow,
    future::{Future, ready},
    sync::Arc,
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
use persy::{
    ByteVec as PersyByteVec, Persy, PrepareError as PersyPrepareError,
    TransactionConfig as PersyTxConfig,
};
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

use super::types::Durability;

const REDB_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("kv");
const MANIFOLD_TABLE: ManifoldTableDefinition<&[u8], &[u8]> = ManifoldTableDefinition::new("kv");

/// Only concurrent workloads may retry typed transaction conflicts. Retry time
/// remains inside the measured transaction and all successful retries are counted.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum RetryMode {
    SingleAttempt,
    Concurrent,
}

/// Native backend operations through an exclusively borrowed client handle.
/// The native database may still be shared by other independently owned clients.
/// Reads and range scans materialize the same data in both benchmark lanes.
pub(super) trait BackendOps {
    fn read(&mut self, key: &[u8]) -> impl Future<Output = Result<Option<Vec<u8>>>>;
    fn commit(
        &mut self,
        durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
        mode: RetryMode,
    ) -> impl Future<Output = Result<u64>>;
    fn count_range(&mut self, start: &[u8], end: &[u8]) -> impl Future<Output = Result<usize>>;
}

/// Synchronous storage engines execute their native transaction directly.
/// A ready future exposes the result to the shared asynchronous harness without
/// an executor hop, blocking adapter, or unnecessary asynchronous state machine.
trait SyncBackend {
    fn read_sync(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;
    fn commit_sync(
        &self,
        durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
    ) -> Result<()>;
    fn count_range_sync(&self, start: &[u8], end: &[u8]) -> Result<usize>;
}

impl<B: SyncBackend> BackendOps for B {
    fn read(&mut self, key: &[u8]) -> impl Future<Output = Result<Option<Vec<u8>>>> {
        ready(self.read_sync(key))
    }

    fn commit(
        &mut self,
        durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
        _mode: RetryMode,
    ) -> impl Future<Output = Result<u64>> {
        ready(self.commit_sync(durability, puts, deletes).map(|()| 0))
    }

    fn count_range(&mut self, start: &[u8], end: &[u8]) -> impl Future<Output = Result<usize>> {
        ready(self.count_range_sync(start, end))
    }
}

impl<B: SyncBackend> SyncBackend for Arc<B> {
    fn read_sync(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.as_ref().read_sync(key)
    }

    fn commit_sync(
        &self,
        durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
    ) -> Result<()> {
        self.as_ref().commit_sync(durability, puts, deletes)
    }

    fn count_range_sync(&self, start: &[u8], end: &[u8]) -> Result<usize> {
        self.as_ref().count_range_sync(start, end)
    }
}

#[derive(Clone)]
pub(super) struct FjallHandle {
    pub(super) db: FjallDb,
    pub(super) keyspace: Keyspace,
}

#[derive(Clone)]
pub(super) struct HeedHandle {
    pub(super) env: HeedEnv,
    pub(super) db: HeedDb<'static, Bytes, Bytes>,
}

// The column family must retain its database owner. Only concurrent runs
// share that owner via Arc; single-client runs keep direct ownership.
#[derive(Clone)]
pub(super) struct ManifoldHandle<D: Borrow<ManifoldDb>> {
    pub(super) owner: D,
    pub(super) cf: ManifoldCf,
}

#[derive(Clone)]
pub(super) struct ParityHandle<D> {
    pub(super) db: D,
    pub(super) ordered: bool,
}

impl SyncBackend for RedbDb {
    fn read_sync(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let txn = self.begin_read()?;
        let table = txn.open_table(REDB_TABLE)?;
        Ok(table.get(key)?.map(|v| v.value().to_vec()))
    }

    fn commit_sync(
        &self,
        durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
    ) -> Result<()> {
        let mut txn = self.begin_write()?;
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
        Ok(())
    }

    fn count_range_sync(&self, start: &[u8], end: &[u8]) -> Result<usize> {
        let txn = self.begin_read()?;
        let table = txn.open_table(REDB_TABLE)?;
        let mut n = 0usize;
        for item in table.range(start..end)? {
            let _ = item?;
            n += 1;
        }
        Ok(n)
    }
}

impl SyncBackend for FjallHandle {
    fn read_sync(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.keyspace.get(key)?.map(|v| v.to_vec()))
    }

    fn commit_sync(
        &self,
        durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
    ) -> Result<()> {
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
        Ok(())
    }

    fn count_range_sync(&self, start: &[u8], end: &[u8]) -> Result<usize> {
        let mut n = 0usize;
        for item in self.keyspace.range(start..end) {
            let _ = item.into_inner()?;
            n += 1;
        }
        Ok(n)
    }
}

impl BackendOps for SurrealTree {
    fn read(&mut self, key: &[u8]) -> impl Future<Output = Result<Option<Vec<u8>>>> {
        ready((|| {
            let txn = self.begin_with_mode(SurrealMode::ReadOnly)?;
            Ok(txn.get(key)?)
        })())
    }

    async fn commit(
        &mut self,
        durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
        mode: RetryMode,
    ) -> Result<u64> {
        const MAX_CONFLICT_RETRIES: u64 = 10_000;
        let mut retries = 0u64;
        loop {
            let mut txn = self.begin()?;
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
                    if mode == RetryMode::Concurrent && retries < MAX_CONFLICT_RETRIES =>
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

    fn count_range(&mut self, start: &[u8], end: &[u8]) -> impl Future<Output = Result<usize>> {
        ready((|| {
            let txn = self.begin_with_mode(SurrealMode::ReadOnly)?;
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
            drop(iter);
            drop(txn);
            Ok(n)
        })())
    }
}

impl SyncBackend for HeedHandle {
    fn read_sync(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let txn = self.env.read_txn()?;
        Ok(self.db.get(&txn, key)?.map(ToOwned::to_owned))
    }

    fn commit_sync(
        &self,
        _durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
    ) -> Result<()> {
        let mut txn = self.env.write_txn()?;
        for (k, v) in puts {
            self.db.put(&mut txn, k.as_slice(), v.as_slice())?;
        }
        for k in deletes {
            self.db.delete(&mut txn, k.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    fn count_range_sync(&self, start: &[u8], end: &[u8]) -> Result<usize> {
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

impl SyncBackend for sled::Db {
    fn read_sync(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.get(key)?.map(|v| v.to_vec()))
    }

    fn commit_sync(
        &self,
        durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
    ) -> Result<()> {
        let mut batch = sled::Batch::default();
        for (k, v) in puts {
            batch.insert(k.as_slice(), v.as_slice());
        }
        for k in deletes {
            batch.remove(k.as_slice());
        }
        self.apply_batch(batch)?;
        if durability == Durability::Sync {
            self.flush()?;
        }
        Ok(())
    }

    fn count_range_sync(&self, start: &[u8], end: &[u8]) -> Result<usize> {
        let mut n = 0usize;
        for item in self.range(start..end) {
            let _ = item?;
            n += 1;
        }
        Ok(n)
    }
}

impl<D: Borrow<ManifoldDb>> SyncBackend for ManifoldHandle<D> {
    fn read_sync(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let txn = self.cf.begin_read()?;
        let table = txn.open_table(MANIFOLD_TABLE)?;
        Ok(table.get(key)?.map(|v| v.value().to_vec()))
    }

    fn commit_sync(
        &self,
        _durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
    ) -> Result<()> {
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
        Ok(())
    }

    fn count_range_sync(&self, start: &[u8], end: &[u8]) -> Result<usize> {
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

// TurboKV close consumes the owned database. Concurrent clients retain
// an Arc instead, which is unwrapped after the worker snapshot.
#[derive(Clone)]
pub(super) struct TurboHandle<D: Borrow<TurboDb>>(pub(super) D);

impl<D: Borrow<TurboDb>> BackendOps for TurboHandle<D> {
    async fn read(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.0.borrow().get(key).await?)
    }

    async fn commit(
        &mut self,
        _durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
        _mode: RetryMode,
    ) -> Result<u64> {
        let mut batch = TurboBatch::new();
        for (k, v) in puts {
            batch.put(k.as_slice(), v.as_slice());
        }
        for k in deletes {
            batch.delete(k.as_slice());
        }
        self.0.borrow().write_batch(&batch).await?;
        Ok(0)
    }

    async fn count_range(&mut self, start: &[u8], end: &[u8]) -> Result<usize> {
        Ok(self.0.borrow().range(start, end).await?.len())
    }
}

impl<D: Borrow<ParityDb>> SyncBackend for ParityHandle<D> {
    fn read_sync(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.db.borrow().get(0, key)?)
    }

    fn commit_sync(
        &self,
        _durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
    ) -> Result<()> {
        let mut changes = Vec::with_capacity(puts.len() + deletes.len());
        for (k, v) in puts {
            changes.push((0u8, k.clone(), Some(v.clone())));
        }
        for k in deletes {
            changes.push((0u8, k.clone(), None));
        }
        self.db.borrow().commit(changes)?;
        Ok(())
    }

    fn count_range_sync(&self, start: &[u8], end: &[u8]) -> Result<usize> {
        if !self.ordered {
            bail!("ParityDB hash-column mode has no ordered range iterator");
        }
        let mut iter = self.db.borrow().iter(0)?;
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
impl SyncBackend for RocksDb {
    fn read_sync(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.get(key)?)
    }

    fn commit_sync(
        &self,
        durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
    ) -> Result<()> {
        let mut batch = RocksBatch::default();
        for (k, v) in puts {
            batch.put(k, v);
        }
        for k in deletes {
            batch.delete(k);
        }
        let mut opts = RocksWriteOptions::default();
        opts.set_sync(durability == Durability::Sync);
        self.write_opt(batch, &opts)?;
        Ok(())
    }

    fn count_range_sync(&self, start: &[u8], end: &[u8]) -> Result<usize> {
        let mut n = 0usize;
        for item in self.iterator(RocksIteratorMode::From(start, RocksDirection::Forward)) {
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
impl SyncBackend for MdbxDb<NoWriteMap> {
    fn read_sync(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let txn = self.begin_ro_txn()?;
        let table = txn.open_table(None)?;
        Ok(txn.get::<Vec<u8>>(&table, key)?)
    }

    fn commit_sync(
        &self,
        _durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
    ) -> Result<()> {
        let txn = self.begin_rw_txn()?;
        let table = txn.create_table(None, MdbxTableFlags::default())?;
        for (k, v) in puts {
            txn.put(&table, k, v, MdbxWriteFlags::UPSERT)?;
        }
        for k in deletes {
            let _ = txn.del(&table, k, None)?;
        }
        txn.commit()?;
        Ok(())
    }

    fn count_range_sync(&self, start: &[u8], end: &[u8]) -> Result<usize> {
        let txn = self.begin_ro_txn()?;
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
impl BackendOps for Persy {
    fn read(&mut self, key: &[u8]) -> impl Future<Output = Result<Option<Vec<u8>>>> {
        ready((|| {
            let key = PersyByteVec::from(key.to_vec());
            Ok(self
                .one::<PersyByteVec, PersyByteVec>("kv", &key)?
                .map(Vec::<u8>::from))
        })())
    }

    async fn commit(
        &mut self,
        durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
        mode: RetryMode,
    ) -> Result<u64> {
        const MAX_LOCK_TIMEOUT_RETRIES: u64 = 100;
        let jitter_seed = u64::from(
            puts.first()
                .map(|(key, _)| key.as_slice())
                .or_else(|| deletes.first().map(Vec::as_slice))
                .and_then(|key| key.last().copied())
                .unwrap_or(0),
        );
        let mut retries = 0u64;
        loop {
            // The native transaction and its prepare error are thread-confined.
            // Finish or release them before the asynchronous backoff.
            {
                let mut tx = match durability {
                    Durability::Relaxed => {
                        self.begin_with(PersyTxConfig::new().set_background_sync(true))?
                    }
                    Durability::Sync => self.begin()?,
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
                match tx.prepare() {
                    Ok(prepared) => {
                        prepared.commit()?;
                        return Ok(retries);
                    }
                    Err(error) if mode == RetryMode::SingleAttempt => return Err(error.into()),
                    Err(error) => match error.error() {
                        PersyPrepareError::TransactionTimeout => {}
                        other => return Err(anyhow::Error::new(other)),
                    },
                }
            }
            retries += 1;
            if retries > MAX_LOCK_TIMEOUT_RETRIES {
                bail!("Persy transaction lock timeout exceeded {MAX_LOCK_TIMEOUT_RETRIES} retries");
            }
            // Prevent lock-step reacquisition; all retry time is measured.
            let backoff_us = 100 + ((jitter_seed + retries * 17) % 40) * 50;
            tokio::time::sleep(std::time::Duration::from_micros(backoff_us)).await;
        }
    }

    fn count_range(&mut self, start: &[u8], end: &[u8]) -> impl Future<Output = Result<usize>> {
        ready((|| {
            let range = PersyByteVec::from(start.to_vec())..PersyByteVec::from(end.to_vec());
            let mut n = 0usize;
            for (_k, values) in self.range::<PersyByteVec, PersyByteVec, _>("kv", range)? {
                if values.into_iter().next().is_some() {
                    n += 1;
                }
            }
            Ok(n)
        })())
    }
}

#[cfg(feature = "kv-experimental")]
impl SyncBackend for RoughDb {
    fn read_sync(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.get(key)?)
    }

    fn commit_sync(
        &self,
        durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
    ) -> Result<()> {
        let mut batch = RoughBatch::new();
        for (k, v) in puts {
            batch.put(k, v);
        }
        for k in deletes {
            batch.delete(k);
        }
        self.write(
            &RoughWriteOptions {
                sync: durability == Durability::Sync,
            },
            batch,
        )?;
        Ok(())
    }

    fn count_range_sync(&self, start: &[u8], end: &[u8]) -> Result<usize> {
        let mut iter = self.new_iterator(&RoughReadOptions::default())?;
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
impl SyncBackend for JammDb {
    fn read_sync(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let tx = self.tx(false)?;
        let bucket = tx.get_bucket("kv")?;
        let value = match bucket.get(key) {
            Some(JammData::KeyValue(kv)) => Some(kv.value().to_vec()),
            Some(JammData::Bucket(_)) => bail!("jammdb KV bucket contains nested bucket"),
            None => None,
        };
        drop(tx);
        Ok(value)
    }

    fn commit_sync(
        &self,
        durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
    ) -> Result<()> {
        if durability == Durability::Relaxed {
            bail!("jammdb relaxed mode unsupported");
        }
        let tx = self.tx(true)?;
        let bucket = tx.get_bucket("kv")?;
        for (k, v) in puts {
            bucket.put(k.as_slice(), v.as_slice())?;
        }
        for k in deletes {
            if bucket.get(k.as_slice()).is_some() {
                let _ = bucket.delete(k.as_slice())?;
            }
        }
        Ok(tx.commit()?)
    }

    fn count_range_sync(&self, start: &[u8], end: &[u8]) -> Result<usize> {
        let tx = self.tx(false)?;
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
        drop(tx);
        Ok(n)
    }
}

#[cfg(feature = "kv-experimental")]
impl SyncBackend for LsmDb {
    fn read_sync(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.get(key)?)
    }

    fn commit_sync(
        &self,
        durability: Durability,
        puts: &[(Vec<u8>, Vec<u8>)],
        deletes: &[Vec<u8>],
    ) -> Result<()> {
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
        self.write(batch)?;
        Ok(())
    }

    fn count_range_sync(&self, start: &[u8], end: &[u8]) -> Result<usize> {
        Ok(self.scan(start.to_vec()..end.to_vec())?.count())
    }
}
