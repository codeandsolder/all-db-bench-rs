use crate::{
    measurement::{
        Quantiles, SettleMeasurement, SetupTimings, count_as_f64, histogram, quantiles,
        record_duration,
    },
    metrics::{ProcDelta, ProcSnapshot, SystemDelta, SystemSnapshot},
};
use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use hdrhistogram::Histogram;
use rand::{RngExt, SeedableRng, rngs::SmallRng, seq::SliceRandom};
use serde::Serialize;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum SustainedPattern {
    Append,
    UpdateUniform,
    UpdateHot95,
    Churn,
}

/// Fixed logical work; every window and transaction ends at a whole-transaction boundary.
#[derive(Clone, Copy)]
pub struct WorkPlan {
    records: u64,
    ops: u64,
    window_ops: u64,
    txn_ops: u64,
    txn_size: usize,
}

impl WorkPlan {
    /// Validate logical operation counts before creating the database.
    ///
    /// # Errors
    /// Rejects zero dimensions and partial transactions.
    pub fn new(records: u64, ops: u64, window_ops: u64, txn_size: usize) -> Result<Self> {
        if records == 0 || ops == 0 || window_ops == 0 || txn_size == 0 {
            bail!("records, ops, window-ops and txn-size must all be greater than zero");
        }
        let txn_ops = u64::try_from(txn_size)?;
        if !window_ops.is_multiple_of(txn_ops) || !ops.is_multiple_of(txn_ops) {
            bail!("--window-ops and --ops must be multiples of --txn-size");
        }
        Ok(Self {
            records,
            ops,
            window_ops,
            txn_ops,
            txn_size,
        })
    }

    pub fn windows(self) -> impl Iterator<Item = WorkWindow> {
        std::iter::successors(Some(0u64), move |start| {
            let next = start.saturating_add(self.window_ops);
            (next < self.ops).then_some(next)
        })
        .map(move |start| WorkWindow {
            start,
            end: self.ops.min(start.saturating_add(self.window_ops)),
            txn_ops: self.txn_ops,
        })
    }

    fn contains_window(self, work: WorkWindow) -> bool {
        work.start < self.ops
            && work.start.is_multiple_of(self.window_ops)
            && work.end == self.ops.min(work.start.saturating_add(self.window_ops))
            && work.txn_ops == self.txn_ops
    }

    // Use the largest whole-transaction epoch no greater than 1000,
    // or one transaction if transactions themselves exceed 1000.
    fn churn_epoch(self, remaining: u64) -> u64 {
        let epoch_ops = (1000 / self.txn_ops).max(1) * self.txn_ops;
        remaining.min(epoch_ops)
    }
}

#[derive(Clone, Copy)]
pub struct WorkWindow {
    start: u64,
    end: u64,
    txn_ops: u64,
}

impl WorkWindow {
    pub fn transactions(self) -> impl Iterator<Item = u64> {
        std::iter::successors(Some(self.start), move |start| {
            let next = start.saturating_add(self.txn_ops);
            (next < self.end).then_some(next)
        })
    }
}

#[derive(Clone, Copy)]
enum ChurnOp {
    Update,
    Insert,
    Delete,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mutation {
    Put { id: u64, salt: u64 },
    Delete { id: u64 },
}

/// A deterministic logical mutation stream. The lane builds native keys/records
/// directly from the emitted mutations, with no extra per-transaction allocation.
pub struct MutationStream {
    pattern: SustainedPattern,
    plan: WorkPlan,
    seed: u64,
    rng: SmallRng,
    next_id: u64,
    epoch: Vec<ChurnOp>,
    epoch_offset: usize,
    done: u64,
}

impl MutationStream {
    /// Construct a workload with the same initial RNG state used by both sustained lanes.
    #[must_use]
    pub fn new(pattern: SustainedPattern, plan: WorkPlan, seed: u64, trial: u32) -> Self {
        Self {
            pattern,
            plan,
            seed,
            rng: SmallRng::seed_from_u64(seed ^ u64::from(trial) ^ 0x5155_5354),
            next_id: plan.records,
            epoch: Vec::new(),
            epoch_offset: 0,
            done: 0,
        }
    }

    fn append_id(&mut self) -> Result<u64> {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .context("sustained ID range overflow")?;
        Ok(id)
    }

    fn refill_epoch(&mut self, remaining: u64) -> Result<()> {
        let epoch = usize::try_from(self.plan.churn_epoch(remaining))?;
        // Calculate proportions without multiplying large usize values.
        let update = (epoch / 100) * 40 + (epoch % 100) * 40 / 100;
        let insert = (epoch / 100) * 30 + (epoch % 100) * 30 / 100;
        let delete = epoch - update - insert;
        self.epoch.clear();
        self.epoch.reserve(epoch);
        self.epoch
            .extend(std::iter::repeat_n(ChurnOp::Update, update));
        self.epoch
            .extend(std::iter::repeat_n(ChurnOp::Insert, insert));
        self.epoch
            .extend(std::iter::repeat_n(ChurnOp::Delete, delete));
        self.epoch.shuffle(&mut self.rng);
        self.epoch_offset = 0;
        Ok(())
    }

    /// Materialize a transaction directly into the backend's native batch vectors.
    /// # Errors
    /// Rejects out-of-order work or an overflowing generated ID.
    pub fn batch<P, D>(
        &mut self,
        window: WorkWindow,
        start: u64,
        hot: impl FnMut(&mut SmallRng, u64) -> u64,
        mut put: impl FnMut(u64, u64) -> P,
        mut delete: impl FnMut(u64) -> D,
    ) -> Result<MutationBatch<P, D>> {
        let mut puts = Vec::with_capacity(self.plan.txn_size);
        let mut deletes = if matches!(self.pattern, SustainedPattern::Churn) {
            Vec::with_capacity(self.plan.txn_size)
        } else {
            Vec::new()
        };
        self.transaction(window, start, hot, |mutation| match mutation {
            Mutation::Put { id, salt } => puts.push(put(id, salt)),
            Mutation::Delete { id } => deletes.push(delete(id)),
        })?;
        Ok(MutationBatch { puts, deletes })
    }

    /// Generate exactly one whole transaction within the given window.
    /// The lane supplies its existing hot-set sampler, preserving the small-dataset policy.
    ///
    /// # Errors
    /// Rejects out-of-order or out-of-window generation.
    pub fn transaction(
        &mut self,
        window: WorkWindow,
        start: u64,
        mut hot: impl FnMut(&mut SmallRng, u64) -> u64,
        mut emit: impl FnMut(Mutation),
    ) -> Result<()> {
        if !self.plan.contains_window(window)
            || start != self.done
            || start < window.start
            || window.end.saturating_sub(start) < self.plan.txn_ops
        {
            bail!("invalid sustained transaction boundary: start={start}");
        }
        if matches!(self.pattern, SustainedPattern::Churn) && self.epoch_offset == self.epoch.len()
        {
            self.refill_epoch(window.end - start)?;
        }
        for offset in 0..self.plan.txn_size {
            let salt = self.seed ^ start ^ u64::try_from(offset)?;
            let mutation = match self.pattern {
                SustainedPattern::Append => Mutation::Put {
                    id: self.append_id()?,
                    salt,
                },
                SustainedPattern::UpdateUniform => Mutation::Put {
                    id: self.rng.random_range(0..self.plan.records),
                    salt,
                },
                SustainedPattern::UpdateHot95 => Mutation::Put {
                    id: hot(&mut self.rng, self.plan.records),
                    salt,
                },
                SustainedPattern::Churn => {
                    let action = self.epoch[self.epoch_offset];
                    self.epoch_offset += 1;
                    match action {
                        ChurnOp::Update => Mutation::Put {
                            id: self.rng.random_range(0..self.plan.records),
                            salt,
                        },
                        ChurnOp::Insert => Mutation::Put {
                            id: self.append_id()?,
                            salt,
                        },
                        ChurnOp::Delete => Mutation::Delete {
                            id: self.rng.random_range(0..self.plan.records),
                        },
                    }
                }
            };
            emit(mutation);
        }
        self.done += self.plan.txn_ops;
        Ok(())
    }
}

pub struct MutationBatch<P, D> {
    pub puts: Vec<P>,
    pub deletes: Vec<D>,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
struct WriteCounts {
    puts: u64,
    deletes: u64,
    transactions: u64,
    logical_mutated_bytes: u64,
}

impl WriteCounts {
    const fn add(&mut self, other: Self) {
        self.puts += other.puts;
        self.deletes += other.deletes;
        self.transactions += other.transactions;
        self.logical_mutated_bytes = self
            .logical_mutated_bytes
            .saturating_add(other.logical_mutated_bytes);
    }
}

#[derive(Debug, Serialize)]
struct WindowMeasurement {
    index: u64,
    ops_start: u64,
    ops_end: u64,
    ops_completed: u64,
    elapsed_s: f64,
    ops_per_s: f64,
    #[serde(flatten)]
    counts: WriteCounts,
    transaction_latency: Quantiles,
    process: ProcDelta,
    system: SystemDelta,
}

pub struct ActiveWindow {
    work: WorkWindow,
    process_before: ProcSnapshot,
    system_before: SystemSnapshot,
    started: Instant,
    histogram: Histogram<u64>,
    counts: WriteCounts,
}

pub struct Foreground {
    plan: WorkPlan,
    process_before: ProcSnapshot,
    system_before: SystemSnapshot,
    started: Instant,
    active_elapsed: Duration,
    completed: u64,
    totals: WriteCounts,
    histogram: Histogram<u64>,
    windows: Vec<WindowMeasurement>,
}

impl Foreground {
    /// Start the wall-clock and process accounting window, before workload setup.
    ///
    /// # Errors
    /// Returns an error if the latency histogram cannot be created.
    pub fn start(plan: WorkPlan) -> Result<Self> {
        let process_before = ProcSnapshot::capture();
        let system_before = SystemSnapshot::capture();
        let started = Instant::now();
        Ok(Self {
            plan,
            process_before,
            system_before,
            started,
            active_elapsed: Duration::ZERO,
            completed: 0,
            totals: WriteCounts::default(),
            histogram: histogram()?,
            windows: Vec::new(),
        })
    }

    /// Start the next active window after collecting its initial snapshots.
    ///
    /// # Errors
    /// Rejects an out-of-order window or histogram initialization failure.
    pub fn start_window(&self, work: WorkWindow) -> Result<ActiveWindow> {
        if !self.plan.contains_window(work) || work.start != self.completed {
            bail!("invalid sustained window");
        }
        let process_before = ProcSnapshot::capture();
        let system_before = SystemSnapshot::capture();
        let started = Instant::now();
        Ok(ActiveWindow {
            work,
            process_before,
            system_before,
            started,
            histogram: histogram()?,
            counts: WriteCounts::default(),
        })
    }

    /// Accumulate the transaction latency in both window and foreground histograms.
    /// # Errors
    /// Rejects an incorrect transaction size.
    pub fn record_transaction(
        &mut self,
        window: &mut ActiveWindow,
        batch: &MutationBatch<impl Sized, impl Sized>,
        put_bytes: u64,
        delete_bytes: u64,
        latency: Duration,
    ) -> Result<()> {
        let puts = u64::try_from(batch.puts.len())?;
        let deletes = u64::try_from(batch.deletes.len())?;
        if puts.checked_add(deletes) != Some(self.plan.txn_ops) {
            bail!("incorrect sustained transaction size");
        }
        let logical_mutated_bytes = puts
            .saturating_mul(put_bytes)
            .saturating_add(deletes.saturating_mul(delete_bytes));
        record_duration(&mut window.histogram, latency);
        record_duration(&mut self.histogram, latency);
        window.counts.add(WriteCounts {
            puts,
            deletes,
            transactions: 1,
            logical_mutated_bytes,
        });
        Ok(())
    }

    /// Stop active timing before capturing final snapshots.
    ///
    /// # Errors
    /// Rejects a window that did not execute precisely its planned logical work.
    pub fn end_window(&mut self, window: ActiveWindow) -> Result<()> {
        let elapsed = window.started.elapsed();
        self.active_elapsed += elapsed;
        let process_after = ProcSnapshot::capture();
        let system_after = SystemSnapshot::capture();
        let completed = window.work.end - window.work.start;
        if window.work.start != self.completed
            || window.counts.puts.checked_add(window.counts.deletes) != Some(completed)
            || window.counts.transactions != completed / self.plan.txn_ops
        {
            bail!("incomplete sustained window: expected {completed} operations");
        }
        let transaction_latency = quantiles(&window.histogram);
        drop(window.histogram);
        self.windows.push(WindowMeasurement {
            index: u64::try_from(self.windows.len())?,
            ops_start: window.work.start,
            ops_end: window.work.end,
            ops_completed: completed,
            elapsed_s: elapsed.as_secs_f64(),
            ops_per_s: count_as_f64(completed) / elapsed.as_secs_f64().max(f64::MIN_POSITIVE),
            counts: window.counts,
            transaction_latency,
            process: window.process_before.delta(&process_after, elapsed),
            system: window.system_before.delta(&system_after),
        });
        self.completed = window.work.end;
        self.totals.add(window.counts);
        Ok(())
    }

    /// Stop the measured foreground before database settling and shutdown.
    ///
    /// # Errors
    /// Rejects incomplete runs.
    pub fn finish(self) -> Result<FinishedForeground> {
        let elapsed = self.started.elapsed();
        let process_after = ProcSnapshot::capture();
        let system_after = SystemSnapshot::capture();
        if self.completed != self.plan.ops {
            bail!(
                "incomplete sustained foreground: {} of {}",
                self.completed,
                self.plan.ops
            );
        }
        Ok(FinishedForeground {
            run: self,
            elapsed,
            process_after,
            system_after,
        })
    }
}

pub struct FinishedForeground {
    run: Foreground,
    elapsed: Duration,
    process_after: ProcSnapshot,
    system_after: SystemSnapshot,
}

#[derive(Debug, Serialize)]
pub struct ForegroundMeasurement {
    ops_completed: u64,
    elapsed_s: f64,
    active_elapsed_s: f64,
    instrumentation_s: f64,
    instrumentation_fraction_of_wall: f64,
    ops_per_s: f64,
    active_ops_per_s: f64,
    #[serde(flatten)]
    counts: WriteCounts,
    transaction_latency: Quantiles,
    windows: Vec<WindowMeasurement>,
    measured_process: ProcDelta,
    system_before: SystemSnapshot,
    system_after: SystemSnapshot,
    measured_system_delta: SystemDelta,
}

impl FinishedForeground {
    /// Serialize later, after settling/closing, to keep report preparation out of those phases.
    #[must_use]
    pub fn measurement(self) -> ForegroundMeasurement {
        let elapsed_s = self.elapsed.as_secs_f64();
        let active_elapsed_s = self.run.active_elapsed.as_secs_f64();
        let instrumentation_s = self
            .elapsed
            .saturating_sub(self.run.active_elapsed)
            .as_secs_f64();
        ForegroundMeasurement {
            ops_completed: self.run.completed,
            elapsed_s,
            active_elapsed_s,
            instrumentation_s,
            instrumentation_fraction_of_wall: instrumentation_s / elapsed_s.max(f64::MIN_POSITIVE),
            ops_per_s: count_as_f64(self.run.completed) / elapsed_s.max(f64::MIN_POSITIVE),
            active_ops_per_s: count_as_f64(self.run.completed)
                / active_elapsed_s.max(f64::MIN_POSITIVE),
            counts: self.run.totals,
            transaction_latency: quantiles(&self.run.histogram),
            windows: self.run.windows,
            measured_process: self
                .run
                .process_before
                .delta(&self.process_after, self.elapsed),
            measured_system_delta: self.run.system_before.delta(&self.system_after),
            system_before: self.run.system_before,
            system_after: self.system_after,
        }
    }
}

/// Measurements with identical semantics and schema in both sustained benchmark lanes.
#[derive(Debug, Serialize)]
pub struct SustainedEvidence {
    #[serde(flatten)]
    pub setup: SetupTimings,
    #[serde(flatten)]
    pub foreground: ForegroundMeasurement,
    pub db_bytes_before: u64,
    pub db_bytes_after_foreground: u64,
    pub db_bytes_final: u64,
    pub post_workload_settle: Option<SettleMeasurement>,
    pub peak_rss_kib: u64,
    pub path: String,
    pub db_kept: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn churn_transactions_never_split_at_epoch_boundaries() -> Result<()> {
        for (ops, window_ops, txn_size) in [
            (2100, 1050, 7),
            (2700, 1800, 300),
            (4500, 3000, 1500),
            (3000, 2000, 100),
        ] {
            let plan = WorkPlan::new(128, ops, window_ops, txn_size)?;
            let mut stream = MutationStream::new(SustainedPattern::Churn, plan, 42, 1);
            let mut other = MutationStream::new(SustainedPattern::Churn, plan, 42, 1);
            let mut completed = 0;
            for window in plan.windows() {
                for start in window.transactions() {
                    let batch = stream.batch(
                        window,
                        start,
                        |rng, records| rng.random_range(0..records),
                        |id, salt| (id, salt),
                        |id| id,
                    )?;
                    let repeat = other.batch(
                        window,
                        start,
                        |rng, records| rng.random_range(0..records),
                        |id, salt| (id, salt),
                        |id| id,
                    )?;
                    assert_eq!(batch.puts, repeat.puts);
                    assert_eq!(batch.deletes, repeat.deletes);
                    assert_eq!(batch.puts.len() + batch.deletes.len(), txn_size);
                    assert!(batch.deletes.iter().all(|&id| id < 128));
                    completed += u64::try_from(txn_size)?;
                }
            }
            assert_eq!(completed, ops);
        }
        Ok(())
    }

    #[test]
    fn invalid_work_and_out_of_order_mutations_are_rejected() -> Result<()> {
        assert!(WorkPlan::new(0, 6, 4, 2).is_err());
        assert!(WorkPlan::new(10, 5, 4, 2).is_err());
        assert!(WorkPlan::new(10, 6, 3, 2).is_err());
        assert!(WorkPlan::new(10, 6, 4, 0).is_err());
        let plan = WorkPlan::new(10, 6, 4, 2)?;
        let window = plan.windows().next().context("missing first window")?;
        let mut stream = MutationStream::new(SustainedPattern::Append, plan, 42, 1);
        assert!(stream.transaction(window, 2, |_, _| 0, |_| {}).is_err());
        assert!(Foreground::start(plan)?.finish().is_err());
        let mut run = Foreground::start(plan)?;
        let active = run.start_window(window)?;
        assert!(run.end_window(active).is_err());
        Ok(())
    }

    #[test]
    fn foreground_report_preserves_flat_counts_and_window_boundaries() -> Result<()> {
        let plan = WorkPlan::new(10, 6, 4, 2)?;
        let mut run = Foreground::start(plan)?;
        let mut stream = MutationStream::new(SustainedPattern::Append, plan, 42, 1);
        for work in plan.windows() {
            let mut active = run.start_window(work)?;
            for start in work.transactions() {
                let batch = stream.batch(work, start, |_, _| 0, |id, _| id, |id| id)?;
                run.record_transaction(&mut active, &batch, 10, 8, Duration::from_micros(10))?;
            }
            run.end_window(active)?;
        }
        let report = serde_json::to_value(run.finish()?.measurement())?;
        assert_eq!(report["ops_completed"], json!(6));
        assert_eq!(report["transactions"], json!(3));
        assert_eq!(report["puts"], json!(6));
        assert_eq!(report["deletes"], json!(0));
        assert_eq!(report["logical_mutated_bytes"], json!(60));
        assert_eq!(report["transaction_latency"]["count"], json!(3));
        assert_eq!(report["windows"][0]["ops_end"], json!(4));
        assert_eq!(report["windows"][0]["transactions"], json!(2));
        assert_eq!(report["windows"][1]["ops_start"], json!(4));
        assert_eq!(report["windows"][1]["ops_end"], json!(6));
        assert_eq!(report["windows"][1]["transactions"], json!(1));
        Ok(())
    }
}
