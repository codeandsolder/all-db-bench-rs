use super::{
    backend_ops::{BackendOps, RetryMode},
    data::{key, sample_existing_id, sample_read_id, value},
    lifecycle::DatasetSpec,
    metrics::{hist, record},
    types::{
        AccessPattern, Durability, EngineKind, KeyShape, ValuePattern, Workload, WritePattern,
    },
    workload::{bounded_churn_swap, mixed_epoch_targets},
};
use all_db_bench_common::{concurrency::WorkPartition, single::SingleOptions};
use anyhow::{Context, Result, bail};
use hdrhistogram::Histogram;
use rand::{RngExt, SeedableRng, rngs::SmallRng, seq::SliceRandom};
use std::time::Instant;

#[derive(Clone, Debug, clap::Args)]
pub(super) struct FixedWorkArgs {
    #[arg(long, value_enum)]
    pub(super) engine: EngineKind,
    #[arg(long, value_enum)]
    pub(super) durability: Durability,
    #[arg(long, value_enum)]
    pub(super) workload: Workload,
    #[arg(long, default_value_t = 100_000)]
    pub(super) records: u64,
    #[arg(long, default_value_t = 50_000)]
    pub(super) ops: u64,
    #[arg(long, default_value_t = 256)]
    pub(super) value_bytes: usize,
    #[arg(long, value_enum, default_value_t = ValuePattern::PseudoRandom)]
    pub(super) value_pattern: ValuePattern,
    #[arg(long, default_value_t = 8)]
    pub(super) key_bytes: usize,
    #[arg(long, value_enum, default_value_t = KeyShape::Sequential)]
    pub(super) key_shape: KeyShape,
    #[arg(long, value_enum, default_value_t = AccessPattern::Auto)]
    pub(super) access_pattern: AccessPattern,
    #[arg(long, default_value_t = 0)]
    pub(super) miss_percent: u8,
    #[arg(long, value_enum, default_value_t = WritePattern::Append)]
    pub(super) write_pattern: WritePattern,
    #[arg(long, default_value_t = 100)]
    pub(super) txn_size: usize,
    #[arg(long, default_value_t = 100)]
    pub(super) scan_len: u64,
    #[arg(long, default_value_t = 1)]
    pub(super) trial: u32,
    #[arg(long, default_value_t = 0x5eed_2026)]
    pub(super) seed: u64,
}

impl FixedWorkArgs {
    pub(super) const fn spec(&self) -> WorkSpec {
        WorkSpec {
            dataset: DatasetSpec {
                records: self.records,
                key_bytes: self.key_bytes,
                key_shape: self.key_shape,
                seed: self.seed,
                value_bytes: self.value_bytes,
                value_pattern: self.value_pattern,
                durability: self.durability,
            },
            workload: self.workload,
            total_ops: self.ops,
            access_pattern: self.access_pattern,
            miss_percent: self.miss_percent,
            write_pattern: self.write_pattern,
            txn_size: self.txn_size,
            scan_len: self.scan_len,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct WorkSpec {
    pub(super) dataset: DatasetSpec,
    pub(super) workload: Workload,
    pub(super) total_ops: u64,
    pub(super) access_pattern: AccessPattern,
    pub(super) miss_percent: u8,
    pub(super) write_pattern: WritePattern,
    pub(super) txn_size: usize,
    pub(super) scan_len: u64,
}

impl WorkSpec {
    pub(super) fn validate(self, engine: EngineKind, pool_slots: u64) -> Result<()> {
        let records = self.dataset.records;
        if records == 0 || self.total_ops == 0 || self.txn_size == 0 {
            bail!("--records, --ops and --txn-size must be greater than zero");
        }
        if self.dataset.key_bytes < 8 {
            bail!("--key-bytes must be at least 8");
        }
        if self.miss_percent > 100 {
            bail!("--miss-percent must be in 0..=100");
        }
        if matches!(self.workload, Workload::RangeScan) {
            if !engine.supports_range_scan() || self.dataset.key_shape == KeyShape::Hashed {
                bail!("range-scan requires ordered keys and a native keyed range API");
            }
            if self.scan_len == 0 || self.scan_len > records {
                bail!("scan_len must be in 1..=records");
            }
            self.total_ops
                .checked_mul(self.scan_len)
                .context("range-read count overflow")?;
        }
        if matches!(self.workload, Workload::DeleteBurst) && self.total_ops > records {
            bail!("delete-burst requires total ops <= records");
        }
        let appends = matches!(self.workload, Workload::TinyTxn | Workload::WriteBurst)
            && self.write_pattern == WritePattern::Append
            || matches!(self.workload, Workload::Balanced | Workload::Churn);
        if appends && pool_slots == 0 {
            records
                .checked_add(self.total_ops)
                .context("append key-ID range overflow")?;
        }
        self.miss_base(pool_slots)?;
        Ok(())
    }

    fn miss_base(self, pool_slots: u64) -> Result<u64> {
        let records = self.dataset.records;
        let pool_end = pool_slots
            .checked_mul(2)
            .and_then(|n| records.checked_add(n))
            .context("bounded churn key-ID range overflow")?;
        if self.miss_percent == 0
            || !matches!(
                self.workload,
                Workload::PointRead | Workload::ReadHeavy | Workload::Balanced
            )
        {
            return Ok(0);
        }
        let historical_base = records
            .checked_add(self.total_ops)
            .and_then(|n| n.checked_add(1_000_000))
            .context("missing-key range overflow")?;
        // Keep the historical miss stream when possible. Large bounded pools
        // require moving it above the entire occupied and inactive key domain.
        let base = historical_base.max(pool_end);
        base.checked_add(records - 1)
            .context("missing-key range overflow")?;
        Ok(base)
    }

    fn key(self, id: u64) -> Vec<u8> {
        key(
            id,
            self.dataset.key_bytes,
            self.dataset.key_shape,
            self.dataset.seed,
        )
    }

    fn put(self, id: u64, salt: u64) -> (Vec<u8>, Vec<u8>) {
        (
            self.key(id),
            value(
                id,
                self.dataset.value_bytes,
                salt,
                self.dataset.value_pattern,
            ),
        )
    }

    fn sample_read(self, rng: &mut SmallRng, miss_base: u64) -> (u64, bool) {
        sample_read_id(
            rng,
            self.dataset.records,
            miss_base,
            self.miss_percent,
            self.access_pattern,
            self.workload,
        )
    }

    fn sample_existing(self, rng: &mut SmallRng) -> u64 {
        sample_existing_id(
            rng,
            self.dataset.records,
            self.access_pattern,
            self.workload,
        )
    }
}

#[derive(Clone, Copy)]
pub(super) enum WorkMode<'a> {
    Single(&'a SingleOptions),
    Concurrent,
}

impl WorkMode<'_> {
    const fn retry(self) -> RetryMode {
        match self {
            Self::Single(_) => RetryMode::SingleAttempt,
            Self::Concurrent => RetryMode::Concurrent,
        }
    }

    fn checkpoint(self, done: u64) -> Result<()> {
        if let Self::Single(options) = self {
            options.checkpoint(done)?;
        }
        Ok(())
    }

    const fn local_salt(self, local: u64) -> u64 {
        match self {
            Self::Single(_) => 0,
            Self::Concurrent => local,
        }
    }
}

/// Each worker has exclusive ownership of a disjoint set of slot pairs.
/// No slot may be toggled twice in one shuffled epoch.
pub(super) struct ChurnPool {
    total: u64,
    offset: u64,
    active_a: Vec<bool>,
    cursor: u64,
}

impl ChurnPool {
    pub(super) fn new(total: u64, partition: WorkPartition) -> Result<Self> {
        if partition.count == 0
            || partition
                .offset
                .checked_add(partition.count)
                .is_none_or(|end| end > total)
        {
            bail!("invalid bounded churn partition");
        }
        Ok(Self {
            total,
            offset: partition.offset,
            active_a: vec![true; usize::try_from(partition.count)?],
            cursor: 0,
        })
    }

    fn toggle(&mut self, spec: WorkSpec, salt: u64) -> Result<WriteSpec> {
        let local = usize::try_from(self.cursor % u64::try_from(self.active_a.len())?)?;
        self.cursor += 1;
        let slot = self.offset + u64::try_from(local)?;
        let swap = bounded_churn_swap(spec.dataset.records, self.total, slot, self.active_a[local]);
        self.active_a[local] = !self.active_a[local];
        Ok(WriteSpec::Toggle {
            put: spec.put(swap.put_id, salt ^ self.cursor),
            delete: spec.key(swap.delete_id),
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
enum WriteSpec {
    Put((Vec<u8>, Vec<u8>)),
    Delete(Vec<u8>),
    Toggle {
        put: (Vec<u8>, Vec<u8>),
        delete: Vec<u8>,
    },
}

#[derive(Debug, PartialEq, Eq)]
struct Batch {
    puts: Vec<(Vec<u8>, Vec<u8>)>,
    deletes: Vec<Vec<u8>>,
}

impl Batch {
    fn new(capacity: usize) -> Self {
        Self {
            puts: Vec::with_capacity(capacity),
            deletes: Vec::with_capacity(capacity),
        }
    }

    const fn len(&self) -> usize {
        self.puts.len() + self.deletes.len()
    }

    const fn is_empty(&self) -> bool {
        self.puts.is_empty() && self.deletes.is_empty()
    }

    fn push(&mut self, spec: WriteSpec) {
        match spec {
            WriteSpec::Put(put) => self.puts.push(put),
            WriteSpec::Delete(key) => self.deletes.push(key),
            WriteSpec::Toggle { put, delete } => {
                self.puts.push(put);
                self.deletes.push(delete);
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Unit {
    Read(Vec<u8>),
    Write(Batch),
}

fn pack_writes(specs: Vec<WriteSpec>, txn_size: usize, units: &mut Vec<Unit>) {
    let mut batch = Batch::new(txn_size.min(1_000));
    for spec in specs {
        let ops = if matches!(spec, WriteSpec::Toggle { .. }) {
            2
        } else {
            1
        };
        if !batch.is_empty() && batch.len() + ops > txn_size {
            units.push(Unit::Write(std::mem::replace(&mut batch, Batch::new(0))));
        }
        batch.push(spec);
        if batch.len() == txn_size {
            units.push(Unit::Write(std::mem::replace(&mut batch, Batch::new(0))));
        }
    }
    if !batch.is_empty() {
        units.push(Unit::Write(batch));
    }
}

pub(super) struct WorkStats {
    pub(super) completed: u64,
    pub(super) reads: u64,
    pub(super) writes: u64,
    pub(super) deletes: u64,
    pub(super) write_conflict_retries: u64,
    pub(super) read_hist: Histogram<u64>,
    pub(super) write_hist: Histogram<u64>,
}

impl WorkStats {
    pub(super) fn new() -> Result<Self> {
        Ok(Self {
            completed: 0,
            reads: 0,
            writes: 0,
            deletes: 0,
            write_conflict_retries: 0,
            read_hist: hist()?,
            write_hist: hist()?,
        })
    }
}

pub(super) struct WorkExecutor<'a> {
    spec: WorkSpec,
    partition: WorkPartition,
    mode: WorkMode<'a>,
    rng: SmallRng,
    next_id: u64,
    miss_base: u64,
    pool: Option<ChurnPool>,
    pub(super) stats: WorkStats,
}

impl<'a> WorkExecutor<'a> {
    pub(super) fn new(
        spec: WorkSpec,
        partition: WorkPartition,
        mode: WorkMode<'a>,
        rng_seed: u64,
        pool: Option<ChurnPool>,
        stats: WorkStats,
    ) -> Result<Self> {
        if partition.count == 0
            || partition
                .offset
                .checked_add(partition.count)
                .is_none_or(|end| end > spec.total_ops)
        {
            bail!("invalid workload partition");
        }
        if pool
            .as_ref()
            .is_some_and(|pool| pool.active_a.len() < 300 || spec.txn_size < 2)
        {
            bail!(
                "bounded churn requires at least 300 slots per client and transactions of at least two operations"
            );
        }
        // The append cursor is only used in growth workloads.
        let next_id = spec.dataset.records.saturating_add(partition.offset);
        let miss_base = spec.miss_base(pool.as_ref().map_or(0, |pool| pool.total))?;
        Ok(Self {
            spec,
            partition,
            mode,
            rng: SmallRng::seed_from_u64(rng_seed),
            next_id,
            miss_base,
            pool,
            stats,
        })
    }

    const fn append_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    const fn remaining(&self) -> u64 {
        self.partition.count - self.stats.completed
    }

    pub(super) async fn run<C: BackendOps + Send>(&mut self, engine: &mut C) -> Result<()> {
        match self.spec.workload {
            Workload::PointRead => self.point_reads(engine).await,
            Workload::RangeScan => self.range_scans(engine).await,
            Workload::TinyTxn | Workload::WriteBurst | Workload::DeleteBurst => {
                self.write_only(engine).await
            }
            Workload::ReadHeavy | Workload::Balanced | Workload::Churn => self.mixed(engine).await,
        }
    }

    async fn point_reads<C: BackendOps + Send>(&mut self, engine: &mut C) -> Result<()> {
        while self.remaining() != 0 {
            let (id, expect_hit) = self.spec.sample_read(&mut self.rng, self.miss_base);
            let started = Instant::now();
            let got = engine.read(&self.spec.key(id)).await?;
            record(&mut self.stats.read_hist, started.elapsed());
            if got.is_some() != expect_hit {
                bail!("point-read mismatch id={id} expected_hit={expect_hit}");
            }
            self.stats.reads += 1;
            self.stats.completed += 1;
        }
        Ok(())
    }

    async fn range_scans<C: BackendOps + Send>(&mut self, engine: &mut C) -> Result<()> {
        let last_start = self.spec.dataset.records - self.spec.scan_len;
        let expected = usize::try_from(self.spec.scan_len)?;
        while self.remaining() != 0 {
            let start = if last_start == 0 {
                0
            } else {
                self.rng.random_range(0..=last_start)
            };
            let end = start + self.spec.scan_len;
            // Key generation is outside the native range timing in both lanes.
            let start = self.spec.key(start);
            let end = self.spec.key(end);
            let started = Instant::now();
            let n = engine.count_range(&start, &end).await?;
            record(&mut self.stats.read_hist, started.elapsed());
            if n != expected {
                bail!("range scan returned {n}, expected {expected}");
            }
            self.stats.reads += self.spec.scan_len;
            self.stats.completed += 1;
        }
        Ok(())
    }

    async fn commit<C: BackendOps + Send>(&mut self, engine: &mut C, batch: Batch) -> Result<()> {
        let started = Instant::now();
        let retries = engine
            .commit(
                self.spec.dataset.durability,
                &batch.puts,
                &batch.deletes,
                self.mode.retry(),
            )
            .await?;
        record(&mut self.stats.write_hist, started.elapsed());
        self.stats.write_conflict_retries = self
            .stats
            .write_conflict_retries
            .checked_add(retries)
            .context("write conflict retry count overflow")?;
        self.stats.writes += batch.puts.len() as u64;
        self.stats.deletes += batch.deletes.len() as u64;
        self.stats.completed += batch.len() as u64;
        Ok(())
    }

    fn write_id(&mut self) -> u64 {
        match self.spec.write_pattern {
            WritePattern::Append => self.append_id(),
            WritePattern::UpdateUniform => self.rng.random_range(0..self.spec.dataset.records),
            WritePattern::UpdateHot => sample_existing_id(
                &mut self.rng,
                self.spec.dataset.records,
                match self.spec.access_pattern {
                    AccessPattern::Auto | AccessPattern::Uniform => AccessPattern::Hot80,
                    other => other,
                },
                self.spec.workload,
            ),
        }
    }

    async fn write_only<C: BackendOps + Send>(&mut self, engine: &mut C) -> Result<()> {
        let txn = if matches!(self.spec.workload, Workload::TinyTxn) {
            1
        } else {
            self.spec.txn_size
        };
        let delete = matches!(self.spec.workload, Workload::DeleteBurst);
        while self.remaining() != 0 {
            let n = usize::try_from(self.remaining())
                .unwrap_or(usize::MAX)
                .min(txn);
            let done = self.stats.completed;
            let mut batch = Batch {
                puts: Vec::with_capacity(if delete { 0 } else { n }),
                deletes: Vec::with_capacity(if delete { n } else { 0 }),
            };
            for local in 0..n {
                let local = local as u64;
                if delete {
                    batch
                        .deletes
                        .push(self.spec.key(self.partition.offset + done + local));
                } else {
                    let id = self.write_id();
                    let salt = self.spec.dataset.seed
                        ^ 0x1111
                        ^ self.partition.offset
                        ^ done
                        ^ self.mode.local_salt(local);
                    batch.puts.push(self.spec.put(id, salt));
                }
            }
            self.commit(engine, batch).await?;
            if !delete {
                self.mode.checkpoint(self.stats.completed)?;
            }
        }
        Ok(())
    }

    fn mixed_epoch(&mut self) -> Result<Vec<Unit>> {
        let targets = mixed_epoch_targets(
            self.spec.workload,
            self.remaining().min(1_000),
            self.pool.is_some(),
        );
        debug_assert_eq!(targets.total(), self.remaining().min(1_000));
        let salt = self.spec.dataset.seed ^ self.partition.offset ^ self.stats.completed;
        let mut units = Vec::with_capacity(usize::try_from(targets.reads)?.saturating_add(16));
        for _ in 0..targets.reads {
            let (id, _) = self.spec.sample_read(&mut self.rng, self.miss_base);
            units.push(Unit::Read(self.spec.key(id)));
        }
        let mut specs = Vec::with_capacity(usize::try_from(
            targets.updates + targets.inserts + targets.deletes,
        )?);
        for _ in 0..targets.updates {
            let id = self.spec.sample_existing(&mut self.rng);
            specs.push(WriteSpec::Put(self.spec.put(id, salt)));
        }
        if let Some(pool) = &mut self.pool {
            if targets.inserts != targets.deletes {
                bail!("bounded churn requires paired insert/delete targets");
            }
            for _ in 0..targets.inserts {
                specs.push(pool.toggle(self.spec, salt)?);
            }
        } else {
            for _ in 0..targets.inserts {
                let id = self.append_id();
                specs.push(WriteSpec::Put(self.spec.put(id, salt)));
            }
            for _ in 0..targets.deletes {
                let id = self.spec.sample_existing(&mut self.rng);
                specs.push(WriteSpec::Delete(self.spec.key(id)));
            }
        }
        specs.shuffle(&mut self.rng);
        pack_writes(specs, self.spec.txn_size, &mut units);
        units.shuffle(&mut self.rng);
        Ok(units)
    }

    async fn mixed<C: BackendOps + Send>(&mut self, engine: &mut C) -> Result<()> {
        while self.remaining() != 0 {
            for unit in self.mixed_epoch()? {
                match unit {
                    Unit::Read(key) => {
                        let started = Instant::now();
                        // Mutations in this workload invalidate a static hit oracle.
                        let _ = engine.read(&key).await?;
                        record(&mut self.stats.read_hist, started.elapsed());
                        self.stats.reads += 1;
                        self.stats.completed += 1;
                    }
                    Unit::Write(batch) => self.commit(engine, batch).await?,
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Batch, ChurnPool, WorkExecutor, WorkMode, WorkSpec, WorkStats};
    use crate::kv::{
        backend_ops::{BackendOps, RetryMode},
        data::value,
        lifecycle::DatasetSpec,
        types::{
            AccessPattern, Durability, EngineKind, KeyShape, ValuePattern, Workload, WritePattern,
        },
    };
    use all_db_bench_common::{concurrency::WorkPartition, single::SingleOptions};
    use anyhow::Result;
    use std::{
        collections::BTreeMap,
        future::{Future, ready},
        path::PathBuf,
    };

    struct MemoryBackend {
        rows: BTreeMap<Vec<u8>, Vec<u8>>,
        transactions: Vec<Batch>,
    }

    impl MemoryBackend {
        fn new(spec: WorkSpec, slots: u64) -> Self {
            Self {
                rows: (0..spec.dataset.records + slots)
                    .map(|id| spec.put(id, spec.dataset.seed))
                    .collect(),
                transactions: Vec::new(),
            }
        }
    }

    impl BackendOps for MemoryBackend {
        fn read(&mut self, key: &[u8]) -> impl Future<Output = Result<Option<Vec<u8>>>> {
            ready(Ok(self.rows.get(key).cloned()))
        }

        fn commit(
            &mut self,
            _: Durability,
            puts: &[(Vec<u8>, Vec<u8>)],
            deletes: &[Vec<u8>],
            mode: RetryMode,
        ) -> impl Future<Output = Result<u64>> {
            self.rows.extend(puts.iter().cloned());
            for key in deletes {
                self.rows.remove(key);
            }
            self.transactions.push(Batch {
                puts: puts.to_vec(),
                deletes: deletes.to_vec(),
            });
            ready(Ok(u64::from(mode == RetryMode::Concurrent)))
        }

        fn count_range(&mut self, start: &[u8], end: &[u8]) -> impl Future<Output = Result<usize>> {
            ready(Ok(self.rows.range(start.to_vec()..end.to_vec()).count()))
        }
    }

    const fn spec(workload: Workload, total_ops: u64) -> WorkSpec {
        WorkSpec {
            dataset: DatasetSpec {
                records: 64,
                key_bytes: 16,
                key_shape: KeyShape::Sequential,
                seed: 0x1234,
                value_bytes: 23,
                value_pattern: ValuePattern::PseudoRandom,
                durability: Durability::Sync,
            },
            workload,
            total_ops,
            access_pattern: AccessPattern::Auto,
            miss_percent: 0,
            write_pattern: WritePattern::Append,
            txn_size: 7,
            scan_len: 5,
        }
    }

    fn single() -> SingleOptions {
        SingleOptions {
            root: PathBuf::new(),
            output: "-".into(),
            keep_db: false,
            reuse_db: false,
            skip_prefill: false,
            warmup_reads: 0,
            db_name: None,
            progress_file: None,
            verify_prefix_records: None,
            verify_tail_records: 0,
        }
    }

    fn executor(
        spec: WorkSpec,
        partition: WorkPartition,
        mode: WorkMode<'_>,
        pool: Option<ChurnPool>,
    ) -> Result<WorkExecutor<'_>> {
        spec.validate(EngineKind::Redb, pool.as_ref().map_or(0, |pool| pool.total))?;
        WorkExecutor::new(spec, partition, mode, 0x5678, pool, WorkStats::new()?)
    }

    #[tokio::test]
    async fn append_payloads_and_partial_transactions_preserve_lane_salts() -> Result<()> {
        let spec = spec(Workload::WriteBurst, 22);
        let options = single();
        for (mode, partition) in [
            (
                WorkMode::Single(&options),
                WorkPartition {
                    offset: 0,
                    count: 22,
                },
            ),
            (
                WorkMode::Concurrent,
                WorkPartition {
                    offset: 6,
                    count: 16,
                },
            ),
        ] {
            let mut engine = MemoryBackend::new(spec, 0);
            let mut run = executor(spec, partition, mode, None)?;
            run.run(&mut engine).await?;
            assert_eq!(run.stats.completed, partition.count);
            assert_eq!(run.stats.writes, partition.count);
            assert_eq!(run.stats.write_hist.len(), partition.count.div_ceil(7));
            for (index, batch) in engine.transactions.iter().enumerate() {
                let done = u64::try_from(index)? * 7;
                assert_eq!(batch.puts.len() as u64, (partition.count - done).min(7));
                for (local, (key, bytes)) in batch.puts.iter().enumerate() {
                    let local = u64::try_from(local)?;
                    let id = spec.dataset.records + partition.offset + done + local;
                    let salt = spec.dataset.seed
                        ^ 0x1111
                        ^ partition.offset
                        ^ done
                        ^ if matches!(mode, WorkMode::Concurrent) {
                            local
                        } else {
                            0
                        };
                    assert_eq!(key, &spec.key(id));
                    assert_eq!(
                        bytes,
                        &value(
                            id,
                            spec.dataset.value_bytes,
                            salt,
                            spec.dataset.value_pattern
                        )
                    );
                }
            }
            assert_eq!(
                run.stats.write_conflict_retries,
                if matches!(mode, WorkMode::Concurrent) {
                    partition.count.div_ceil(7)
                } else {
                    0
                }
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn read_and_delete_workloads_account_for_logical_ops_and_rows() -> Result<()> {
        let options = single();
        let partition = WorkPartition {
            offset: 0,
            count: 37,
        };
        for (workload, reads, deletes) in [
            (Workload::PointRead, 37, 0),
            (Workload::RangeScan, 185, 0),
            (Workload::DeleteBurst, 0, 37),
        ] {
            let mut spec = spec(workload, partition.count);
            spec.miss_percent = 33;
            let mut engine = MemoryBackend::new(spec, 0);
            let mut run = executor(spec, partition, WorkMode::Single(&options), None)?;
            run.run(&mut engine).await?;
            assert_eq!(run.stats.completed, 37);
            assert_eq!(run.stats.reads, reads);
            assert_eq!(run.stats.deletes, deletes);
            assert_eq!(engine.rows.len() as u64, spec.dataset.records - deletes);
            assert_eq!(run.stats.read_hist.len(), if deletes == 0 { 37 } else { 0 });
            assert_eq!(run.stats.write_hist.len(), deletes.div_ceil(7));
        }
        Ok(())
    }

    #[tokio::test]
    async fn bounded_churn_preserves_disjoint_pairs_across_shuffled_epochs() -> Result<()> {
        let spec = spec(Workload::Churn, 2074);
        let mut engine = MemoryBackend::new(spec, 600);
        for client in 0..2 {
            let partition = WorkPartition {
                offset: client * 1037,
                count: 1037,
            };
            let pool = ChurnPool::new(
                600,
                WorkPartition {
                    offset: client * 300,
                    count: 300,
                },
            )?;
            let mut run = executor(spec, partition, WorkMode::Concurrent, Some(pool))?;
            run.run(&mut engine).await?;
            assert_eq!(
                (
                    run.stats.completed,
                    run.stats.reads,
                    run.stats.writes,
                    run.stats.deletes
                ),
                (1037, 0, 726, 311)
            );
            assert_eq!(run.stats.write_conflict_retries, run.stats.write_hist.len());
        }
        assert_eq!(engine.rows.len(), 664);
        for slot in 0..600 {
            let a = spec.dataset.records + slot;
            let b = a + 600;
            assert_eq!(engine.rows.contains_key(&spec.key(a)), slot % 300 < 11);
            assert_eq!(engine.rows.contains_key(&spec.key(b)), slot % 300 >= 11);
        }
        for batch in &engine.transactions {
            assert!(batch.len() <= spec.txn_size);
            for key in &batch.deletes {
                let id = u64::from_be_bytes(key[..8].try_into()?);
                let other = if id < spec.dataset.records + 600 {
                    id + 600
                } else {
                    id - 600
                };
                assert!(batch.puts.iter().any(|(key, _)| *key == spec.key(other)));
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn mixed_growth_stream_is_deterministic_and_preserves_epoch_counts() -> Result<()> {
        let spec = spec(Workload::Balanced, 1037);
        let partition = WorkPartition {
            offset: 0,
            count: 1037,
        };
        let options = single();
        let mut a = MemoryBackend::new(spec, 0);
        let mut b = MemoryBackend::new(spec, 0);
        let mut first = executor(spec, partition, WorkMode::Single(&options), None)?;
        let mut second = executor(spec, partition, WorkMode::Single(&options), None)?;
        first.run(&mut a).await?;
        second.run(&mut b).await?;
        assert_eq!(
            (
                first.stats.completed,
                first.stats.reads,
                first.stats.writes,
                first.stats.deletes
            ),
            (1037, 520, 414, 103)
        );
        assert_eq!(a.transactions, b.transactions);
        assert_eq!(a.rows, b.rows);
        assert!(
            a.transactions
                .iter()
                .all(|batch| batch.len() <= spec.txn_size)
        );
        Ok(())
    }

    #[test]
    fn invalid_contracts_are_rejected_before_work() -> Result<()> {
        let mut invalid = spec(Workload::RangeScan, 100);
        invalid.dataset.key_shape = KeyShape::Hashed;
        assert!(invalid.validate(EngineKind::Redb, 0).is_err());
        let mut invalid = spec(Workload::WriteBurst, 100);
        invalid.txn_size = 0;
        assert!(invalid.validate(EngineKind::Redb, 0).is_err());
        invalid.txn_size = 7;
        invalid.dataset.records = u64::MAX - 10;
        assert!(invalid.validate(EngineKind::Redb, 0).is_err());
        let mut invalid = spec(Workload::Balanced, 100);
        invalid.miss_percent = 10;
        assert!(invalid.validate(EngineKind::Redb, 1_000_000).is_ok());
        assert_eq!(invalid.miss_base(1_000_000)?, 2_000_064);
        assert!(invalid.validate(EngineKind::Redb, u64::MAX).is_err());
        let valid = spec(Workload::Churn, 100);
        let pool = ChurnPool::new(
            300,
            WorkPartition {
                offset: 0,
                count: 299,
            },
        )?;
        assert!(
            executor(
                valid,
                WorkPartition {
                    offset: 0,
                    count: 100
                },
                WorkMode::Concurrent,
                Some(pool)
            )
            .is_err()
        );
        Ok(())
    }
}
