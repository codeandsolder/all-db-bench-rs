//! Fixed-work record execution, shared by single-client and concurrent lanes.
use crate::{RecordData, Workload, backend::RecordBackend, dataset::RecordDataset, hist, record};
use all_db_bench_common::single::SingleOptions;
use anyhow::{Context, Result, bail};
use hdrhistogram::Histogram;
use rand::{RngExt, SeedableRng, rngs::SmallRng};
use std::time::Instant;

#[derive(Clone, Copy)]
pub enum WorkMode<'a> {
    Single(&'a SingleOptions),
    Concurrent { offset: u64, bounded: bool },
}

impl WorkMode<'_> {
    const fn offset(self) -> u64 {
        match self {
            Self::Single(_) => 0,
            Self::Concurrent { offset, .. } => offset,
        }
    }

    const fn bounded(self) -> bool {
        matches!(self, Self::Concurrent { bounded: true, .. })
    }

    const fn concurrent(self) -> bool {
        matches!(self, Self::Concurrent { .. })
    }

    fn checkpoint(self, completed: u64) -> Result<()> {
        if let Self::Single(options) = self {
            options.checkpoint(completed)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub struct WorkSpec<'a> {
    pub dataset: RecordDataset,
    pub workload: Workload,
    pub ops: u64,
    pub txn_size: usize,
    pub rng_seed: u64,
    pub mode: WorkMode<'a>,
}

impl WorkSpec<'_> {
    pub fn validate(self) -> Result<()> {
        if self.dataset.records == 0 || self.ops == 0 || self.txn_size == 0 {
            bail!("record workload requires positive records, ops and txn-size");
        }
        let end = self
            .mode
            .offset()
            .checked_add(self.ops)
            .context("logical record operation range overflow")?;
        if matches!(self.workload, Workload::TinyTxn | Workload::WriteBurst) && !self.mode.bounded()
        {
            self.dataset
                .records
                .checked_add(end - 1)
                .context("append record ID range overflow")?;
        }
        if matches!(self.workload, Workload::WriteBurst) && self.mode.concurrent() {
            let txn = u64::try_from(self.txn_size)?;
            if !self.ops.is_multiple_of(txn) {
                bail!("concurrent write-burst requires whole transactions");
            }
            if self.mode.bounded() && txn > self.dataset.records {
                bail!("bounded write-burst requires txn-size <= records");
            }
        }
        Ok(())
    }

    const fn growth_salt(self, done: u64) -> u64 {
        self.dataset.seed ^ self.mode.offset() ^ done
    }

    fn write_row(self, done: u64, local: u64) -> (u64, RecordData) {
        let logical = self.mode.offset() + done + local;
        let (id, salt) = if self.mode.bounded() {
            (logical % self.dataset.records, self.dataset.seed ^ logical)
        } else {
            (self.dataset.records + logical, self.growth_salt(done))
        };
        (id, self.dataset.row(id, salt))
    }
}

pub struct WorkStats {
    pub completed: u64,
    pub reads: u64,
    pub writes: u64,
    pub transactions: u64,
    pub write_conflict_retries: u64,
    pub operation_hist: Histogram<u64>,
    pub transaction_hist: Histogram<u64>,
}

impl WorkStats {
    pub fn new() -> Result<Self> {
        Ok(Self {
            completed: 0,
            reads: 0,
            writes: 0,
            transactions: 0,
            write_conflict_retries: 0,
            operation_hist: hist()?,
            transaction_hist: hist()?,
        })
    }
}

#[derive(Clone, Copy)]
enum Upsert<'a> {
    One(u64, &'a RecordData),
    Batch(&'a [(u64, RecordData)]),
}

impl Upsert<'_> {
    async fn commit<B: RecordBackend>(self, engine: &mut B, retry: bool) -> Result<u64> {
        const MAX_CONFLICT_RETRIES: u64 = 10_000;
        let mut retries = 0;
        loop {
            let result = match self {
                Self::One(id, data) => engine.upsert_one(id, data).await,
                Self::Batch(rows) => engine.upsert_batch(rows).await,
            };
            match result {
                Ok(()) => return Ok(retries),
                Err(error)
                    if retry
                        && B::is_transaction_conflict(&error)
                        && retries < MAX_CONFLICT_RETRIES =>
                {
                    retries += 1;
                    if retries <= 8 {
                        std::hint::spin_loop();
                    } else {
                        tokio::task::yield_now().await;
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }
}

pub struct WorkExecutor<'a> {
    spec: WorkSpec<'a>,
    rng: SmallRng,
    pub stats: WorkStats,
}

impl<'a> WorkExecutor<'a> {
    pub fn new(spec: WorkSpec<'a>, stats: WorkStats) -> Result<Self> {
        spec.validate()?;
        Ok(Self {
            rng: SmallRng::seed_from_u64(spec.rng_seed),
            spec,
            stats,
        })
    }

    pub async fn run<B: RecordBackend>(&mut self, engine: &mut B) -> Result<()> {
        match self.spec.workload {
            Workload::PointRead | Workload::IndexedRead => self.read_only(engine).await,
            Workload::ReadHeavy => self.read_heavy(engine).await,
            Workload::TinyTxn | Workload::WriteBurst => self.write_only(engine).await,
        }
    }

    async fn point_read<B: RecordBackend>(&mut self, engine: &mut B) -> Result<()> {
        let id = self.rng.random_range(0..self.spec.dataset.records);
        let started = Instant::now();
        let read = engine.get(id).await?;
        if read.is_none() {
            bail!("prefilled record missing: {id}");
        }
        std::hint::black_box(read);
        record(&mut self.stats.operation_hist, started.elapsed());
        self.stats.completed += 1;
        self.stats.reads += 1;
        Ok(())
    }

    async fn read_only<B: RecordBackend>(&mut self, engine: &mut B) -> Result<()> {
        match self.spec.workload {
            Workload::PointRead => {
                while self.stats.completed < self.spec.ops {
                    self.point_read(engine).await?;
                }
            }
            Workload::IndexedRead => {
                while self.stats.completed < self.spec.ops {
                    let group = self.rng.random_range(0..100);
                    let started = Instant::now();
                    let read = engine.indexed_read(group).await?;
                    std::hint::black_box(read);
                    record(&mut self.stats.operation_hist, started.elapsed());
                    self.stats.completed += 1;
                    self.stats.reads += 1;
                }
            }
            _ => bail!("not a read-only workload"),
        }
        Ok(())
    }

    async fn read_heavy<B: RecordBackend>(&mut self, engine: &mut B) -> Result<()> {
        while self.stats.completed < self.spec.ops {
            for _ in 0..19 {
                if self.stats.completed == self.spec.ops {
                    break;
                }
                self.point_read(engine).await?;
            }
            if self.stats.completed < self.spec.ops {
                let id = self.rng.random_range(0..self.spec.dataset.records);
                let data = self
                    .spec
                    .dataset
                    .row(id, self.spec.growth_salt(self.stats.completed));
                let started = Instant::now();
                let retries = Upsert::One(id, &data)
                    .commit(engine, self.spec.mode.concurrent())
                    .await?;
                record(&mut self.stats.operation_hist, started.elapsed());
                self.record_commit(1, retries)?;
            }
        }
        Ok(())
    }

    fn record_commit(&mut self, count: u64, retries: u64) -> Result<()> {
        self.stats.completed += count;
        self.stats.writes += count;
        self.stats.transactions += 1;
        self.stats.write_conflict_retries = self
            .stats
            .write_conflict_retries
            .checked_add(retries)
            .context("write conflict retry count overflow")?;
        Ok(())
    }

    async fn write_only<B: RecordBackend>(&mut self, engine: &mut B) -> Result<()> {
        let tiny = matches!(self.spec.workload, Workload::TinyTxn);
        let batch = if tiny {
            1
        } else {
            u64::try_from(self.spec.txn_size)?
        };
        while self.stats.completed < self.spec.ops {
            let done = self.stats.completed;
            let count = (self.spec.ops - done).min(batch);
            let retries = if tiny {
                let (id, data) = self.spec.write_row(done, 0);
                let started = Instant::now();
                let retries = Upsert::One(id, &data)
                    .commit(engine, self.spec.mode.concurrent())
                    .await?;
                record(&mut self.stats.transaction_hist, started.elapsed());
                retries
            } else {
                let mut rows = Vec::with_capacity(usize::try_from(count)?);
                for local in 0..count {
                    rows.push(self.spec.write_row(done, local));
                }
                let started = Instant::now();
                let retries = Upsert::Batch(&rows)
                    .commit(engine, self.spec.mode.concurrent())
                    .await?;
                record(&mut self.stats.transaction_hist, started.elapsed());
                retries
            };
            self.record_commit(count, retries)?;
            self.spec.mode.checkpoint(self.stats.completed)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{WorkExecutor, WorkMode, WorkSpec, WorkStats};
    use crate::{
        Durability, IndexedRead, ReadDigest, RecordData, Workload,
        backend::RecordBackend,
        dataset::{RecordDataset, Warmup},
    };
    use all_db_bench_common::{
        concurrency::partition_work,
        record_payload::{PayloadPattern, payload_for},
        single::SingleOptions,
    };
    use anyhow::{Result, bail};
    use std::{
        cell::Cell,
        collections::BTreeMap,
        future::{Future, ready},
        path::{Path, PathBuf},
    };

    #[derive(Debug)]
    struct Conflict;
    impl std::fmt::Display for Conflict {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("test transaction conflict")
        }
    }
    impl std::error::Error for Conflict {}

    // Cell deliberately makes this a Send but non-Sync connection, like SQLite.
    #[derive(Default)]
    struct MemoryBackend {
        rows: BTreeMap<u64, RecordData>,
        batches: Vec<usize>,
        reads: Vec<u64>,
        conflicts: Cell<u64>,
        attempts: u64,
        hard_failure: bool,
    }

    impl MemoryBackend {
        fn attempt(&mut self) -> Result<()> {
            self.attempts += 1;
            if self.hard_failure {
                bail!("test permanent failure");
            }
            let remaining = self.conflicts.get();
            if remaining > 0 {
                self.conflicts.set(remaining - 1);
                return Err(Conflict.into());
            }
            Ok(())
        }

        fn apply(&mut self, rows: &[(u64, RecordData)], deletes: &[u64]) -> Result<()> {
            self.attempt()?;
            self.batches.push(rows.len());
            self.rows.extend(rows.iter().cloned());
            for id in deletes {
                self.rows.remove(id);
            }
            Ok(())
        }
    }

    impl RecordBackend for MemoryBackend {
        type Kind = ();
        fn version((): ()) -> &'static str {
            "test"
        }
        fn mapping((): (), _: Durability) -> &'static str {
            "memory"
        }
        fn is_transaction_conflict(error: &anyhow::Error) -> bool {
            error.is::<Conflict>()
        }
        fn open((): (), _: Durability, _: &Path) -> impl Future<Output = Result<Self>> + Send {
            ready(Ok(Self::default()))
        }
        fn get(&mut self, id: u64) -> impl Future<Output = Result<Option<ReadDigest>>> + Send {
            self.reads.push(id);
            ready(Ok(self
                .rows
                .get(&id)
                .map(|row| ReadDigest::new(row.bucket, row.payload.len()))))
        }
        fn indexed_read(&mut self, group: u32) -> impl Future<Output = Result<IndexedRead>> + Send {
            let mut result = IndexedRead {
                rows: 0,
                checksum: 0,
            };
            for row in self.rows.values().filter(|row| row.bucket == group) {
                result.rows += 1;
                result.checksum = result
                    .checksum
                    .wrapping_add(ReadDigest::new(row.bucket, row.payload.len()).checksum());
            }
            ready(Ok(result))
        }
        fn upsert_one(
            &mut self,
            id: u64,
            data: &RecordData,
        ) -> impl Future<Output = Result<()>> + Send {
            ready(self.apply(&[(id, data.clone())], &[]))
        }
        fn upsert_batch(
            &mut self,
            rows: &[(u64, RecordData)],
        ) -> impl Future<Output = Result<()>> + Send {
            ready(self.apply(rows, &[]))
        }
        fn write_batch(
            &mut self,
            rows: &[(u64, RecordData)],
            deletes: &[u64],
        ) -> impl Future<Output = Result<()>> + Send {
            ready(self.apply(rows, deletes))
        }
        fn make_clients(
            &mut self,
            _: usize,
            _: &Path,
            _: Durability,
        ) -> impl Future<Output = Result<(Vec<Self>, &'static str)>> + Send {
            ready(Err(anyhow::anyhow!(
                "test backend has no shared connection factory"
            )))
        }
    }

    fn runtime() -> Result<tokio::runtime::Runtime> {
        Ok(tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()?)
    }

    const fn dataset(records: u64) -> RecordDataset {
        RecordDataset {
            records,
            payload_bytes: 16,
            payload_pattern: PayloadPattern::HexRandom,
            seed: 42,
        }
    }

    fn single_options() -> SingleOptions {
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

    fn spec(
        dataset: RecordDataset,
        workload: Workload,
        ops: u64,
        mode: WorkMode<'_>,
    ) -> WorkSpec<'_> {
        WorkSpec {
            dataset,
            workload,
            ops,
            txn_size: 3,
            rng_seed: 99,
            mode,
        }
    }

    #[test]
    fn bounded_stream_is_identical_across_client_counts() -> Result<()> {
        let dataset = dataset(64);
        let total = 513;
        let serial = spec(
            dataset,
            Workload::TinyTxn,
            total,
            WorkMode::Concurrent {
                offset: 0,
                bounded: true,
            },
        );
        let signature = |spec: WorkSpec<'_>, op| {
            let (id, row) = spec.write_row(op, 0);
            (id, row.bucket, row.payload)
        };
        let expected = (0..total)
            .map(|op| signature(serial, op))
            .collect::<Vec<_>>();
        for clients in [1, 2, 4, 8] {
            let mut actual = Vec::new();
            for part in partition_work(total, clients, 1)? {
                let local = spec(
                    dataset,
                    Workload::TinyTxn,
                    part.count,
                    WorkMode::Concurrent {
                        offset: part.offset,
                        bounded: true,
                    },
                );
                actual.extend((0..part.count).map(|op| signature(local, op)));
            }
            assert_eq!(actual, expected);
        }
        Ok(())
    }

    #[test]
    fn shared_dataset_and_single_burst_preserve_exact_rows_and_tail_transaction() -> Result<()> {
        runtime()?.block_on(async {
            let dataset = dataset(7);
            let mut engine = MemoryBackend::default();
            dataset.prefill(&mut engine, 3).await?;
            assert_eq!(engine.batches, [3, 3, 1]);
            for (&id, row) in &engine.rows {
                assert_eq!(
                    row.payload,
                    payload_for(PayloadPattern::HexRandom, id, 16, 42)
                );
            }
            dataset.warm(&mut engine, 10, Warmup::Sequential).await?;
            assert_eq!(engine.reads, (0..7).collect::<Vec<_>>());
            engine.batches.clear();
            let options = single_options();
            let mut run = WorkExecutor::new(
                spec(dataset, Workload::WriteBurst, 7, WorkMode::Single(&options)),
                WorkStats::new()?,
            )?;
            run.run(&mut engine).await?;
            assert_eq!(engine.batches, [3, 3, 1]);
            assert_eq!(engine.rows.len(), 14);
            assert_eq!(run.stats.completed, 7);
            assert_eq!(run.stats.writes, 7);
            assert_eq!(run.stats.transactions, 3);
            assert_eq!(run.stats.transaction_hist.len(), 3);
            for id in 7..14 {
                let row = engine
                    .rows
                    .get(&id)
                    .ok_or_else(|| anyhow::anyhow!("missing appended row {id}"))?;
                assert_eq!(
                    row.payload,
                    payload_for(
                        PayloadPattern::HexRandom,
                        id,
                        16,
                        dataset.seed ^ ((id - 7) / 3 * 3)
                    )
                );
            }
            Ok(())
        })
    }

    #[test]
    fn concurrent_retry_is_typed_and_single_attempt_never_retries() -> Result<()> {
        runtime()?.block_on(async {
            let dataset = dataset(7);
            let mut engine = MemoryBackend::default();
            dataset.prefill(&mut engine, 3).await?;
            engine.batches.clear();
            engine.conflicts.set(2);
            let mut run = WorkExecutor::new(
                spec(
                    dataset,
                    Workload::WriteBurst,
                    6,
                    WorkMode::Concurrent {
                        offset: 3,
                        bounded: true,
                    },
                ),
                WorkStats::new()?,
            )?;
            run.run(&mut engine).await?;
            assert_eq!(run.stats.completed, 6);
            assert_eq!(run.stats.transactions, 2);
            assert_eq!(run.stats.write_conflict_retries, 2);
            assert_eq!(engine.batches, [3, 3]);
            assert_eq!(engine.rows.len(), 7);
            engine.conflicts.set(1);
            let before = engine.attempts;
            let options = single_options();
            let mut run = WorkExecutor::new(
                spec(dataset, Workload::TinyTxn, 1, WorkMode::Single(&options)),
                WorkStats::new()?,
            )?;
            assert!(run.run(&mut engine).await.is_err());
            assert_eq!(engine.attempts, before + 1);
            assert_eq!(run.stats.completed, 0);
            engine.hard_failure = true;
            let before = engine.attempts;
            let mut run = WorkExecutor::new(
                spec(
                    dataset,
                    Workload::TinyTxn,
                    1,
                    WorkMode::Concurrent {
                        offset: 0,
                        bounded: false,
                    },
                ),
                WorkStats::new()?,
            )?;
            assert!(run.run(&mut engine).await.is_err());
            assert_eq!(engine.attempts, before + 1);
            Ok(())
        })
    }

    #[test]
    fn read_heavy_preserves_operation_counts_and_latency_class() -> Result<()> {
        runtime()?.block_on(async {
            let dataset = dataset(7);
            let mut engine = MemoryBackend::default();
            dataset.prefill(&mut engine, 3).await?;
            let options = single_options();
            let mut run = WorkExecutor::new(
                spec(dataset, Workload::ReadHeavy, 42, WorkMode::Single(&options)),
                WorkStats::new()?,
            )?;
            run.run(&mut engine).await?;
            assert_eq!(
                (
                    run.stats.completed,
                    run.stats.reads,
                    run.stats.writes,
                    run.stats.transactions
                ),
                (42, 40, 2, 2)
            );
            assert_eq!(run.stats.operation_hist.len(), 42);
            assert!(run.stats.transaction_hist.is_empty());
            assert_eq!(engine.rows.len(), 7);
            Ok(())
        })
    }
}
