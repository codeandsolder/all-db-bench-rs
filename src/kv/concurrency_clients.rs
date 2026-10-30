use anyhow::{Result, bail};
use std::sync::Arc;

use fjall::{Database as FjallDb, Keyspace, PersistMode};
use heed::{Database as HeedDb, Env as HeedEnv, types::Bytes};
use manifold::{
    Durability as ManifoldDurability,
    column_family::{ColumnFamily as ManifoldCf, ColumnFamilyDatabase as ManifoldDb},
};
use parity_db::Db as ParityDb;
use redb::{Database as RedbDb, Durability as RedbDurability, ReadableDatabase};
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

use super::{
    engine::{MANIFOLD_TABLE, REDB_TABLE},
    types::Durability,
};

#[expect(async_fn_in_trait, reason = "async trait methods are intentional for the benchmark client abstraction")]
pub(super) trait ClientOps: Clone + Send + 'static {
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
pub(super) struct RedbClient(pub(super) Arc<RedbDb>);

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
pub(super) struct FjallClient {
    pub(super) db: FjallDb,
    pub(super) keyspace: Keyspace,
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
pub(super) struct SurrealClient(pub(super) SurrealTree);

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
pub(super) struct HeedClient {
    pub(super) env: HeedEnv,
    pub(super) db: HeedDb<Bytes, Bytes>,
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
pub(super) struct SledClient(pub(super) sled::Db);

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
pub(super) struct ManifoldClient {
    pub(super) _db: Arc<ManifoldDb>,
    pub(super) cf: ManifoldCf,
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
pub(super) struct TurboClient(pub(super) Arc<TurboDb>);

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
pub(super) struct ParityClient {
    pub(super) db: Arc<ParityDb>,
    pub(super) ordered: bool,
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
pub(super) struct RocksClient(pub(super) Arc<RocksDb>);

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
pub(super) struct MdbxClient(pub(super) Arc<MdbxDb<NoWriteMap>>);

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
pub(super) struct PersyClient(pub(super) Persy);

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
        const MAX_LOCK_TIMEOUT_RETRIES: u64 = 100;
        let jitter_seed = puts
            .first()
            .map(|(key, _)| key.as_slice())
            .or_else(|| deletes.first().map(Vec::as_slice))
            .and_then(|key| key.last().copied())
            .unwrap_or(0) as u64;
        let mut retries = 0u64;
        loop {
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
            match tx.prepare() {
                Ok(prepared) => {
                    prepared.commit()?;
                    return Ok(retries);
                }
                Err(error) => match error.error() {
                    PersyPrepareError::TransactionTimeout => {
                        retries = retries.saturating_add(1);
                        if retries > MAX_LOCK_TIMEOUT_RETRIES {
                            bail!(
                                "Persy transaction lock timeout exceeded {MAX_LOCK_TIMEOUT_RETRIES} retries"
                            );
                        }
                        // Transactions that collided tend to time out together. A tiny,
                        // deterministic per-key/retry skew prevents immediate lock-step
                        // reacquisition while keeping all retry time inside measured latency.
                        let backoff_us = 100 + ((jitter_seed + retries * 17) % 40) * 50;
                        tokio::time::sleep(std::time::Duration::from_micros(backoff_us)).await;
                    }
                    other => return Err(anyhow::Error::new(other)),
                },
            }
        }
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
pub(super) struct RoughClient(pub(super) Arc<RoughDb>);

#[cfg(feature = "kv-experimental")]
impl ClientOps for RoughClient {
    async fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.0.get(key)?)
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
pub(super) struct JammClient(pub(super) JammDb);

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
pub(super) struct LsmClient(pub(super) Arc<LsmDb>);

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
