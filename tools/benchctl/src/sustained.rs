use crate::baseline_identity::matches_trials;
use crate::json_io::{read_json, read_ndjson};
use crate::json_value::py_string;
use crate::numeric::sample_quantile;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value, json};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy)]
pub struct SustainedArgs<'a> {
    pub(crate) input: &'a Path,
    pub(crate) expect_trials: Option<usize>,
    pub(crate) json_out: Option<&'a Path>,
    pub(crate) markdown_out: Option<&'a Path>,
}

const NUMERIC_FIELDS: [&str; 31] = [
    "wall_ops_per_s",
    "active_ops_per_s",
    "instrumentation_fraction",
    "aggregate_cpu_ns_per_op",
    "aggregate_process_write_bytes_per_logical_mutated_byte",
    "baseline_ops_per_s",
    "window_ops_per_s_median",
    "window_ops_per_s_min",
    "tail_ops_per_s_median",
    "tail_throughput_ratio_vs_baseline",
    "throughput_cv",
    "normalized_throughput_slope_per_window",
    "min_throughput_ratio_vs_baseline",
    "windows_below_75pct_baseline",
    "windows_below_50pct_baseline",
    "windows_below_25pct_baseline",
    "longest_run_below_50pct_baseline",
    "baseline_p99_txn_us",
    "worst_p99_txn_us",
    "worst_p99_multiplier_vs_baseline",
    "cpu_ns_per_op_median_window",
    "cpu_ns_per_op_max_window",
    "process_write_bytes_per_logical_mutated_byte_median_window",
    "process_write_bytes_per_logical_mutated_byte_max_window",
    "runqueue_wait_fraction_median_window",
    "runqueue_wait_fraction_max_window",
    "cpu_psi_some_fraction_median_window",
    "cpu_psi_some_fraction_max_window",
    "io_psi_full_fraction_median_window",
    "io_psi_full_fraction_max_window",
    "foreground_db_growth_bytes",
];

const IDENTITY_FIELDS: [&str; 22] = [
    "format_version",
    "lane",
    "scenario",
    "engine",
    "engine_version",
    "durability",
    "durability_mapping",
    "pattern",
    "records",
    "ops_requested",
    "window_ops",
    "value_bytes",
    "payload_bytes",
    "value_pattern",
    "key_bytes",
    "key_shape",
    "txn_size",
    "read_materialization",
    "write_materialization",
    "seed",
    "settle_requested_ms",
    "settle_sample_ms",
];

fn f64_value(value: f64) -> Result<Value> {
    Number::from_f64(value)
        .map(Value::Number)
        .with_context(|| format!("cannot encode non-finite sustained value: {value}"))
}

fn finite(value: f64, label: &str) -> Result<f64> {
    if !value.is_finite() {
        bail!("{label} must be finite");
    }
    Ok(value)
}

fn positive(value: f64, label: &str) -> Result<f64> {
    if finite(value, label)? <= 0.0 {
        bail!("{label} must be positive");
    }
    Ok(value)
}

fn nonnegative(value: f64, label: &str) -> Result<f64> {
    if finite(value, label)? < 0.0 {
        bail!("{label} must be nonnegative");
    }
    Ok(value)
}

fn consistent(actual: f64, expected: f64, label: &str) -> Result<()> {
    let scale = actual.abs().max(expected.abs()).max(f64::MIN_POSITIVE);
    if !actual.is_finite() || !expected.is_finite() || (actual - expected).abs() > 1e-8 * scale {
        bail!("{label} mismatch: measured={actual}, expected={expected}");
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct MutationBytes {
    put: u64,
    delete: u64,
}

impl MutationBytes {
    fn from_identity(raw: &Value) -> Result<Self> {
        let count = |key: &str| {
            raw.get(key)
                .and_then(Value::as_u64)
                .with_context(|| format!("missing sustained identity: {key}"))
        };
        match raw.get("lane").and_then(Value::as_str) {
            Some("kv-sustained") => {
                let key_bytes = count("key_bytes")?;
                if key_bytes < 8 {
                    bail!("invalid KV key width");
                }
                Ok(Self {
                    put: key_bytes.saturating_add(count("value_bytes")?),
                    delete: key_bytes,
                })
            }
            Some("record-sustained") => Ok(Self {
                put: 12_u64.saturating_add(count("payload_bytes")?),
                delete: 8,
            }),
            _ => bail!("invalid sustained lane"),
        }
    }

    const fn logical(self, counts: Counts) -> u64 {
        counts
            .puts
            .saturating_mul(self.put)
            .saturating_add(counts.deletes.saturating_mul(self.delete))
    }
}

#[derive(Deserialize)]
struct Latency {
    count: u64,
    p99_us: f64,
}

#[derive(Deserialize)]
struct Process {
    cpu_runtime_ns: u64,
    write_bytes: u64,
    runqueue_wait_fraction_of_wall: f64,
}

#[derive(Deserialize)]
struct System {
    accounting_wall_ns: u64,
    psi_cpu_some_us: u64,
    psi_io_full_us: u64,
}

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq)]
struct Counts {
    puts: u64,
    deletes: u64,
    transactions: u64,
    logical_mutated_bytes: u64,
}

impl Counts {
    fn validate(
        self,
        ops: u64,
        txn_size: u64,
        latency: &Latency,
        bytes: MutationBytes,
    ) -> Result<()> {
        if self.puts.checked_add(self.deletes) != Some(ops)
            || self.transactions != ops / txn_size
            || latency.count != self.transactions
            || self.logical_mutated_bytes != bytes.logical(self)
        {
            bail!("inconsistent sustained mutation/transaction counts");
        }
        positive(latency.p99_us, "p99 transaction latency")?;
        Ok(())
    }

    fn accumulate(&mut self, other: Self) -> Result<()> {
        self.puts = self
            .puts
            .checked_add(other.puts)
            .context("put count overflow")?;
        self.deletes = self
            .deletes
            .checked_add(other.deletes)
            .context("delete count overflow")?;
        self.transactions = self
            .transactions
            .checked_add(other.transactions)
            .context("transaction count overflow")?;
        self.logical_mutated_bytes = self
            .logical_mutated_bytes
            .saturating_add(other.logical_mutated_bytes);
        Ok(())
    }
}

#[derive(Deserialize)]
struct Window {
    index: usize,
    ops_start: u64,
    ops_end: u64,
    ops_completed: u64,
    elapsed_s: f64,
    ops_per_s: f64,
    #[serde(flatten)]
    counts: Counts,
    transaction_latency: Latency,
    process: Process,
    system: System,
}

#[derive(Deserialize)]
struct Settle {
    requested_ms: u64,
    sample_ms: u64,
    elapsed_s: f64,
    process: Process,
    db_bytes_before: u64,
    db_bytes_after: u64,
    samples: Vec<serde::de::IgnoredAny>,
}

#[derive(Deserialize)]
struct CaseEvidence {
    ops_requested: u64,
    ops_completed: u64,
    window_ops: u64,
    txn_size: u64,
    trial: u32,
    elapsed_s: f64,
    active_elapsed_s: f64,
    ops_per_s: f64,
    active_ops_per_s: f64,
    instrumentation_fraction_of_wall: f64,
    #[serde(flatten)]
    counts: Counts,
    transaction_latency: Latency,
    measured_process: Process,
    db_bytes_before: u64,
    db_bytes_after_foreground: u64,
    db_bytes_final: u64,
    post_workload_settle: Option<Settle>,
    windows: Vec<Window>,
}

impl CaseEvidence {
    fn validate(&self, bytes: MutationBytes) -> Result<()> {
        if self.trial == 0
            || self.ops_requested == 0
            || self.txn_size == 0
            || self.window_ops == 0
            || self.ops_completed != self.ops_requested
            || !self.ops_requested.is_multiple_of(self.txn_size)
            || !self.window_ops.is_multiple_of(self.txn_size)
        {
            bail!("invalid sustained fixed-work dimensions");
        }
        positive(self.elapsed_s, "elapsed_s")?;
        positive(self.active_elapsed_s, "active_elapsed_s")?;
        positive(self.ops_per_s, "wall throughput")?;
        positive(self.active_ops_per_s, "active throughput")?;
        if self.active_elapsed_s > self.elapsed_s
            || !(0.0..=1.0).contains(&self.instrumentation_fraction_of_wall)
        {
            bail!("invalid sustained foreground timing");
        }
        self.counts.validate(
            self.ops_completed,
            self.txn_size,
            &self.transaction_latency,
            bytes,
        )?;
        consistent(
            self.ops_per_s,
            int_as_f64!(self.ops_completed) / self.elapsed_s,
            "wall throughput",
        )?;
        consistent(
            self.active_ops_per_s,
            int_as_f64!(self.ops_completed) / self.active_elapsed_s,
            "active throughput",
        )?;
        // Compare the active fraction rather than the small difference of two large times.
        consistent(
            1.0 - self.instrumentation_fraction_of_wall,
            self.active_elapsed_s / self.elapsed_s,
            "active fraction",
        )?;
        let mut elapsed = 0.0;
        let mut end = 0;
        let mut counts = Counts::default();
        for (index, window) in self.windows.iter().enumerate() {
            let expected = (self.ops_completed - end).min(self.window_ops);
            if expected == 0
                || window.index != index
                || window.ops_start != end
                || window.ops_completed != expected
                || window.ops_end != end + expected
            {
                bail!("invalid sustained window coverage at index {index}");
            }
            window
                .counts
                .validate(expected, self.txn_size, &window.transaction_latency, bytes)?;
            positive(window.elapsed_s, "window elapsed_s")?;
            positive(window.ops_per_s, "window throughput")?;
            nonnegative(
                window.process.runqueue_wait_fraction_of_wall,
                "runqueue fraction",
            )?;
            consistent(
                window.ops_per_s,
                int_as_f64!(expected) / window.elapsed_s,
                "window throughput",
            )?;
            elapsed += window.elapsed_s;
            counts.accumulate(window.counts)?;
            end = window.ops_end;
        }
        if end != self.ops_completed || counts != self.counts {
            bail!("sustained windows do not cover the foreground totals");
        }
        consistent(self.active_elapsed_s, elapsed, "summed active window time")?;
        if let Some(settle) = &self.post_workload_settle {
            positive(settle.elapsed_s, "settle elapsed_s")?;
            if settle.requested_ms == 0 || settle.sample_ms == 0 {
                bail!("invalid sustained settling evidence");
            }
        }
        Ok(())
    }
}

fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    Some(if values.len().is_multiple_of(2) {
        f64::midpoint(values[middle - 1], values[middle])
    } else {
        values[middle]
    })
}

fn quantile(values: &[f64], q: f64) -> Option<f64> {
    sample_quantile(&mut values.to_vec(), q)
}

fn longest_run(flags: &[bool]) -> usize {
    let mut best = 0;
    let mut current = 0;
    for flag in flags {
        if *flag {
            current += 1;
            best = best.max(current);
        } else {
            current = 0;
        }
    }
    best
}

#[derive(Serialize)]
struct RateStats {
    baseline_windows: usize,
    baseline_ops_per_s: f64,
    window_ops_per_s_median: f64,
    window_ops_per_s_q1: f64,
    window_ops_per_s_q3: f64,
    window_ops_per_s_min: f64,
    window_ops_per_s_max: f64,
    tail_windows: usize,
    tail_ops_per_s_median: f64,
    tail_throughput_ratio_vs_baseline: f64,
    throughput_cv: f64,
    normalized_throughput_slope_per_window: f64,
    min_throughput_ratio_vs_baseline: f64,
}

impl RateStats {
    fn from_rates(rates: &[f64], baseline: f64) -> Result<Self> {
        let count = rates.len().min(3);
        let tail =
            median(&mut rates[rates.len() - count..].to_vec()).context("empty sustained tail")?;
        let mean = rates.iter().sum::<f64>() / int_as_f64!(rates.len());
        let cv = if rates.len() > 1 {
            (rates.iter().map(|rate| (*rate - mean).powi(2)).sum::<f64>()
                / int_as_f64!(rates.len()))
            .sqrt()
                / mean
        } else {
            0.0
        };
        let slope = if rates.len() > 1 {
            let x_mean = int_as_f64!(rates.len() - 1) / 2.0;
            let denominator = (0..rates.len())
                .map(|index| (int_as_f64!(index) - x_mean).powi(2))
                .sum::<f64>();
            rates
                .iter()
                .enumerate()
                .map(|(index, rate)| (int_as_f64!(index) - x_mean) * (*rate - mean))
                .sum::<f64>()
                / denominator
                / baseline
        } else {
            0.0
        };
        let minimum = rates
            .iter()
            .copied()
            .reduce(f64::min)
            .context("empty rates")?;
        Ok(Self {
            baseline_windows: count,
            baseline_ops_per_s: baseline,
            window_ops_per_s_median: median(&mut rates.to_vec()).context("empty rates")?,
            window_ops_per_s_q1: quantile(rates, 0.25).context("empty q1")?,
            window_ops_per_s_q3: quantile(rates, 0.75).context("empty q3")?,
            window_ops_per_s_min: minimum,
            window_ops_per_s_max: rates
                .iter()
                .copied()
                .reduce(f64::max)
                .context("empty rates")?,
            tail_windows: count,
            tail_ops_per_s_median: tail,
            tail_throughput_ratio_vs_baseline: finite(tail / baseline, "tail ratio")?,
            throughput_cv: finite(cv, "throughput CV")?,
            normalized_throughput_slope_per_window: finite(slope, "throughput slope")?,
            min_throughput_ratio_vs_baseline: minimum / baseline,
        })
    }
}

#[derive(Serialize)]
struct Cliffs {
    windows_below_75pct_baseline: usize,
    windows_below_50pct_baseline: usize,
    windows_below_25pct_baseline: usize,
    longest_run_below_50pct_baseline: usize,
    first_below_50pct_window: Option<usize>,
    first_below_50pct_ops_start: Option<u64>,
    first_recovered_90pct_window: Option<usize>,
    max_throughput_ratio_after_first_50pct_cliff: Option<f64>,
}

impl Cliffs {
    fn from_rates(windows: &[Window], rates: &[f64], baseline: f64) -> Result<Self> {
        let ratios = rates
            .iter()
            .map(|rate| finite(rate / baseline, "throughput ratio"))
            .collect::<Result<Vec<_>>>()?;
        let below50 = ratios.iter().map(|value| *value < 0.5).collect::<Vec<_>>();
        let first = below50.iter().position(|flag| *flag);
        let (recovered, max_after) = first.map_or((None, None), |first| {
            let later = &ratios[first + 1..];
            (
                later
                    .iter()
                    .position(|value| *value >= 0.90)
                    .map(|index| first + 1 + index),
                later.iter().copied().reduce(f64::max),
            )
        });
        Ok(Self {
            windows_below_75pct_baseline: ratios.iter().filter(|&&v| v < 0.75).count(),
            windows_below_50pct_baseline: below50.iter().filter(|&&v| v).count(),
            windows_below_25pct_baseline: ratios.iter().filter(|&&v| v < 0.25).count(),
            longest_run_below_50pct_baseline: longest_run(&below50),
            first_below_50pct_window: first,
            first_below_50pct_ops_start: first.map(|i| windows[i].ops_start),
            first_recovered_90pct_window: recovered,
            max_throughput_ratio_after_first_50pct_cliff: max_after,
        })
    }
}

#[derive(Serialize)]
struct LatencyStats {
    baseline_p99_txn_us: f64,
    worst_p99_txn_us: f64,
    worst_p99_multiplier_vs_baseline: f64,
}

impl LatencyStats {
    fn from_windows(windows: &[Window]) -> Result<Self> {
        let p99 = windows
            .iter()
            .map(|window| window.transaction_latency.p99_us)
            .collect::<Vec<_>>();
        let baseline =
            median(&mut p99[..p99.len().min(3)].to_vec()).context("empty latency baseline")?;
        let worst = p99
            .into_iter()
            .reduce(f64::max)
            .context("empty sustained latency")?;
        Ok(Self {
            baseline_p99_txn_us: baseline,
            worst_p99_txn_us: worst,
            worst_p99_multiplier_vs_baseline: finite(worst / baseline, "p99 ratio")?,
        })
    }
}

fn insert_distribution(
    case: &mut Map<String, Value>,
    names: (&str, &str),
    mut values: Vec<f64>,
) -> Result<()> {
    let maximum = values
        .iter()
        .copied()
        .reduce(f64::max)
        .context("empty sustained metric")?;
    let med = median(&mut values).context("empty sustained metric")?;
    case.insert(names.0.to_owned(), f64_value(med)?);
    case.insert(names.1.to_owned(), f64_value(maximum)?);
    Ok(())
}

fn window_resources(case: &mut Map<String, Value>, windows: &[Window]) -> Result<()> {
    type Metric = (&'static str, &'static str, fn(&Window) -> f64);
    let metrics: [Metric; 5] = [
        (
            "cpu_ns_per_op_median_window",
            "cpu_ns_per_op_max_window",
            |w| int_as_f64!(w.process.cpu_runtime_ns) / int_as_f64!(w.ops_completed),
        ),
        (
            "process_write_bytes_per_logical_mutated_byte_median_window",
            "process_write_bytes_per_logical_mutated_byte_max_window",
            |w| {
                int_as_f64!(w.process.write_bytes)
                    / int_as_f64!(w.counts.logical_mutated_bytes.max(1))
            },
        ),
        (
            "runqueue_wait_fraction_median_window",
            "runqueue_wait_fraction_max_window",
            |w| w.process.runqueue_wait_fraction_of_wall,
        ),
        (
            "cpu_psi_some_fraction_median_window",
            "cpu_psi_some_fraction_max_window",
            |w| {
                int_as_f64!(w.system.psi_cpu_some_us) * 1000.0
                    / int_as_f64!(w.system.accounting_wall_ns.max(1))
            },
        ),
        (
            "io_psi_full_fraction_median_window",
            "io_psi_full_fraction_max_window",
            |w| {
                int_as_f64!(w.system.psi_io_full_us) * 1000.0
                    / int_as_f64!(w.system.accounting_wall_ns.max(1))
            },
        ),
    ];
    for (med, max, sample) in metrics {
        insert_distribution(case, (med, max), windows.iter().map(sample).collect())?;
    }
    Ok(())
}

fn signed_delta(after: u64, before: u64) -> Result<i64> {
    i64::try_from(i128::from(after) - i128::from(before))
        .context("database growth outside signed 64-bit range")
}

#[derive(Serialize)]
struct SettleSummary {
    elapsed_s: f64,
    cpu_runtime_ns: u64,
    write_bytes: u64,
    db_bytes_delta: i64,
    sample_count: usize,
}

#[derive(Serialize)]
struct CaseResources {
    wall_ops_per_s: f64,
    active_ops_per_s: f64,
    instrumentation_fraction: f64,
    aggregate_cpu_ns_per_op: f64,
    aggregate_process_write_bytes_per_logical_mutated_byte: f64,
    db_bytes_before: u64,
    db_bytes_after_foreground: u64,
    db_bytes_final: u64,
    foreground_db_growth_bytes: i64,
    settle: Option<SettleSummary>,
}

impl CaseResources {
    fn from_evidence(evidence: &CaseEvidence) -> Result<Self> {
        let settle = evidence
            .post_workload_settle
            .as_ref()
            .map(|s| -> Result<_> {
                Ok(SettleSummary {
                    elapsed_s: s.elapsed_s,
                    cpu_runtime_ns: s.process.cpu_runtime_ns,
                    write_bytes: s.process.write_bytes,
                    db_bytes_delta: signed_delta(s.db_bytes_after, s.db_bytes_before)?,
                    sample_count: s.samples.len(),
                })
            })
            .transpose()?;
        Ok(Self {
            wall_ops_per_s: evidence.ops_per_s,
            active_ops_per_s: evidence.active_ops_per_s,
            instrumentation_fraction: evidence.instrumentation_fraction_of_wall,
            aggregate_cpu_ns_per_op: int_as_f64!(evidence.measured_process.cpu_runtime_ns)
                / int_as_f64!(evidence.ops_completed),
            aggregate_process_write_bytes_per_logical_mutated_byte: int_as_f64!(
                evidence.measured_process.write_bytes
            ) / int_as_f64!(
                evidence.counts.logical_mutated_bytes.max(1)
            ),
            db_bytes_before: evidence.db_bytes_before,
            db_bytes_after_foreground: evidence.db_bytes_after_foreground,
            db_bytes_final: evidence.db_bytes_final,
            foreground_db_growth_bytes: signed_delta(
                evidence.db_bytes_after_foreground,
                evidence.db_bytes_before,
            )?,
            settle,
        })
    }
}

#[derive(Serialize)]
struct CaseSummary {
    #[serde(flatten)]
    identity: Map<String, Value>,
    trial: u32,
    ops_completed: u64,
    window_count: usize,
    #[serde(flatten)]
    throughput: RateStats,
    #[serde(flatten)]
    cliffs: Cliffs,
    #[serde(flatten)]
    latency: LatencyStats,
    #[serde(flatten)]
    resources: CaseResources,
    #[serde(flatten)]
    window_resources: Map<String, Value>,
}

fn case_identity(raw: &Value, evidence: &CaseEvidence) -> Result<Map<String, Value>> {
    let mut identity = IDENTITY_FIELDS
        .iter()
        .copied()
        .zip(group_key(raw))
        .map(|(key, value)| (key.to_owned(), value))
        .collect::<Map<_, _>>();
    for field in ["format_version", "records", "seed"] {
        if !raw.get(field).is_some_and(Value::is_u64) {
            bail!("missing or invalid sustained identity: {field}");
        }
    }
    if raw.get("records").and_then(Value::as_u64) == Some(0) {
        bail!("empty dataset");
    }
    for field in [
        "lane",
        "scenario",
        "engine",
        "engine_version",
        "durability",
        "durability_mapping",
        "pattern",
    ] {
        if raw
            .get(field)
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            bail!("missing or invalid sustained identity: {field}");
        }
    }
    match raw.get("lane").and_then(Value::as_str) {
        Some("kv-sustained") => {
            for key in ["value_pattern", "key_shape"] {
                if raw
                    .get(key)
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
                {
                    bail!("missing KV identity: {key}");
                }
            }
        }
        Some("record-sustained") => {}
        _ => bail!("invalid sustained lane"),
    }
    identity.insert(
        "settle_requested_ms".into(),
        json!(
            evidence
                .post_workload_settle
                .as_ref()
                .map(|s| s.requested_ms)
        ),
    );
    identity.insert(
        "settle_sample_ms".into(),
        json!(evidence.post_workload_settle.as_ref().map(|s| s.sample_ms)),
    );
    Ok(identity)
}

fn summarize_case(raw: &Value) -> Result<Value> {
    let evidence = CaseEvidence::deserialize(raw).context("invalid sustained evidence")?;
    evidence.validate(MutationBytes::from_identity(raw)?)?;
    let identity = case_identity(raw, &evidence)?;
    let rates = evidence
        .windows
        .iter()
        .map(|window| window.ops_per_s)
        .collect::<Vec<_>>();
    let baseline =
        median(&mut rates[..rates.len().min(3)].to_vec()).context("empty sustained baseline")?;
    let mut resources = Map::new();
    window_resources(&mut resources, &evidence.windows)?;
    Ok(serde_json::to_value(CaseSummary {
        identity,
        trial: evidence.trial,
        ops_completed: evidence.ops_completed,
        window_count: evidence.windows.len(),
        throughput: RateStats::from_rates(&rates, baseline)?,
        cliffs: Cliffs::from_rates(&evidence.windows, &rates, baseline)?,
        latency: LatencyStats::from_windows(&evidence.windows)?,
        resources: CaseResources::from_evidence(&evidence)?,
        window_resources: resources,
    })?)
}

fn group_key(case: &Value) -> Vec<Value> {
    IDENTITY_FIELDS
        .iter()
        .map(|field| {
            case.get(*field).cloned().unwrap_or_else(|| match *field {
                "read_materialization" => Value::String("legacy-read-v0".to_owned()),
                "write_materialization" => Value::String("legacy-return-v0".to_owned()),
                _ => Value::Null,
            })
        })
        .collect()
}

struct TrialGroup<'a> {
    key: Vec<Value>,
    rows: Vec<&'a Value>,
    seen: HashSet<u64>,
}

impl TrialGroup<'_> {
    fn summary(&self) -> Result<Value> {
        let mut group = IDENTITY_FIELDS
            .iter()
            .copied()
            .zip(self.key.iter().cloned())
            .map(|(key, value)| (key.to_owned(), value))
            .collect::<Map<_, _>>();
        group.insert("trials".into(), json!(self.rows.len()));
        let mut trial_ids = self.seen.iter().copied().collect::<Vec<_>>();
        trial_ids.sort_unstable();
        group.insert("trial_ids".into(), json!(trial_ids));
        for field in NUMERIC_FIELDS {
            let mut values = self
                .rows
                .iter()
                .map(|row| {
                    row.get(field)
                        .and_then(Value::as_f64)
                        .with_context(|| format!("missing sustained metric: {field}"))
                })
                .collect::<Result<Vec<_>>>()?;
            let med = median(&mut values).context("empty group")?;
            group.insert(format!("{field}_median"), f64_value(med)?);
            group.insert(format!("{field}_min"), f64_value(values[0])?);
            group.insert(format!("{field}_max"), f64_value(values[values.len() - 1])?);
        }
        self.cliff_summary(&mut group)?;
        self.settle_summary(&mut group)?;
        Ok(Value::Object(group))
    }

    fn cliff_summary(&self, group: &mut Map<String, Value>) -> Result<()> {
        let mut cliffs = self
            .rows
            .iter()
            .filter_map(|row| row.get("first_below_50pct_window").and_then(Value::as_f64))
            .collect::<Vec<_>>();
        group.insert("trials_with_50pct_cliff".into(), json!(cliffs.len()));
        group.insert(
            "first_below_50pct_window_median_when_present".into(),
            median(&mut cliffs).map_or(Ok(Value::Null), f64_value)?,
        );
        let recovered = self
            .rows
            .iter()
            .filter(|row| {
                !row["first_below_50pct_window"].is_null()
                    && !row["first_recovered_90pct_window"].is_null()
            })
            .count();
        group.insert(
            "trials_recovered_to_90pct_after_cliff".into(),
            json!(recovered),
        );
        Ok(())
    }

    fn settle_summary(&self, group: &mut Map<String, Value>) -> Result<()> {
        for (field, out) in [
            ("cpu_runtime_ns", "settle_cpu_runtime_ns_median"),
            ("write_bytes", "settle_write_bytes_median"),
            ("db_bytes_delta", "settle_db_bytes_delta_median"),
        ] {
            let mut values = self
                .rows
                .iter()
                .filter_map(|row| row["settle"].get(field))
                .map(|value| value.as_f64().context("invalid settle metric"))
                .collect::<Result<Vec<_>>>()?;
            group.insert(
                out.to_owned(),
                median(&mut values).map_or(Ok(Value::Null), f64_value)?,
            );
        }
        Ok(())
    }
}

fn aggregate(cases: &[Value]) -> Result<Vec<Value>> {
    let mut groups = HashMap::<String, TrialGroup<'_>>::new();
    for case in cases {
        let key = group_key(case);
        let trial = case
            .get("trial")
            .and_then(Value::as_u64)
            .filter(|&t| t > 0)
            .context("invalid sustained trial ID")?;
        let group = groups
            .entry(serde_json::to_string(&key)?)
            .or_insert_with(|| TrialGroup {
                key,
                rows: Vec::new(),
                seen: HashSet::new(),
            });
        if !group.seen.insert(trial) {
            bail!("duplicate sustained trial {trial} for {:?}", group.key);
        }
        group.rows.push(case);
    }
    let mut groups = groups.into_iter().collect::<Vec<_>>();
    groups.sort_by_cached_key(|(key, group)| {
        (
            group.key.iter().map(py_string).collect::<Vec<_>>(),
            key.clone(),
        )
    });
    groups.iter().map(|(_, group)| group.summary()).collect()
}

fn read_input(path: &Path) -> Result<Vec<Value>> {
    if path.is_dir() {
        let case_dir = path.join("cases");
        let mut paths = fs::read_dir(&case_dir)
            .with_context(|| format!("failed to list {}", case_dir.display()))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<PathBuf>>>()?;
        paths.retain(|path| path.extension().and_then(|value| value.to_str()) == Some("json"));
        paths.sort();
        paths.iter().map(|path| read_json(path)).collect()
    } else if path.extension().and_then(|value| value.to_str()) == Some("ndjson") {
        read_ndjson(path)
    } else {
        Ok(vec![read_json(path)?])
    }
}

fn fmt(value: Option<&Value>, digits: usize) -> String {
    let Some(value) = value else {
        return "—".to_owned();
    };
    if value.is_null() {
        return "—".to_owned();
    }
    if let Some(number) = value.as_f64() {
        return format!("{number:.digits$}");
    }
    py_string(value)
}

fn render_markdown(groups: &[Value]) -> String {
    let mut lines = vec![
        "# Sustained-write / compaction summary".to_owned(),
        String::new(),
        "Threshold columns are diagnostics relative to each trial's first three windows; they are not pass/fail criteria.".to_owned(),
        "Cases with >10% between-window instrumentation overhead are warned in summary.json and need larger windows before performance/cliff magnitude is interpreted.".to_owned(),
        String::new(),
        "| lane | scenario | engine | dur | pattern | trials | wall ops/s | tail/base | slope/window | CV | min/base | 50% cliff trials | longest <50% | worst p99/base | write B/logical B agg | write B/logical B max-win | rq max % | IO PSI full max % | instr % | settle write MiB |".to_owned(),
        "|---|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|".to_owned(),
    ];
    for group in groups {
        let settle = group
            .get("settle_write_bytes_median")
            .and_then(Value::as_f64)
            .map(|value| value / 1024.0 / 1024.0);
        let percent = |key: &str| {
            group
                .get(key)
                .and_then(Value::as_f64)
                .map(|value| value * 100.0)
        };
        lines.push(format!(
            "| {} | {} | {} {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            py_string(group.get("lane").unwrap_or(&Value::Null)),
            py_string(group.get("scenario").unwrap_or(&Value::Null)),
            py_string(group.get("engine").unwrap_or(&Value::Null)),
            py_string(group.get("engine_version").unwrap_or(&Value::Null)),
            py_string(group.get("durability").unwrap_or(&Value::Null)),
            py_string(group.get("pattern").unwrap_or(&Value::Null)),
            py_string(group.get("trials").unwrap_or(&Value::Null)),
            fmt(group.get("wall_ops_per_s_median"), 3),
            fmt(group.get("tail_throughput_ratio_vs_baseline_median"), 3),
            fmt(group.get("normalized_throughput_slope_per_window_median"), 3),
            fmt(group.get("throughput_cv_median"), 3),
            fmt(group.get("min_throughput_ratio_vs_baseline_median"), 3),
            py_string(group.get("trials_with_50pct_cliff").unwrap_or(&Value::Null)),
            fmt(group.get("longest_run_below_50pct_baseline_median"), 3),
            fmt(group.get("worst_p99_multiplier_vs_baseline_median"), 3),
            fmt(group.get("aggregate_process_write_bytes_per_logical_mutated_byte_median"), 3),
            fmt(group.get("process_write_bytes_per_logical_mutated_byte_max_window_median"), 3),
            percent("runqueue_wait_fraction_max_window_median").map_or_else(|| "—".to_owned(), |value| format!("{value:.3}")),
            percent("io_psi_full_fraction_max_window_median").map_or_else(|| "—".to_owned(), |value| format!("{value:.3}")),
            percent("instrumentation_fraction_median").map_or_else(|| "—".to_owned(), |value| format!("{value:.3}")),
            settle.map_or_else(|| "—".to_owned(), |value| format!("{value:.3}")),
        ));
    }
    lines.join("\n") + "\n"
}

fn trial_problems(groups: &[Value], expected: Option<usize>) -> Result<Vec<Value>> {
    let mut problems = Vec::new();
    if let Some(expected) = expected {
        for group in groups {
            let ids = group
                .get("trial_ids")
                .and_then(Value::as_array)
                .context("missing trial IDs")?
                .iter()
                .map(|v| v.as_i64().context("invalid trial ID"))
                .collect::<Result<Vec<_>>>()?;
            if !matches_trials(&ids, i64::try_from(expected)?) {
                problems.push(Value::String(format!(
                    "{} {} {} {}: trial IDs {ids:?}, expected 1..={expected}",
                    py_string(group.get("scenario").unwrap_or(&Value::Null)),
                    py_string(group.get("engine").unwrap_or(&Value::Null)),
                    py_string(group.get("durability").unwrap_or(&Value::Null)),
                    py_string(group.get("pattern").unwrap_or(&Value::Null)),
                )));
            }
        }
    }
    Ok(problems)
}

pub fn run(args: SustainedArgs<'_>) -> Result<i32> {
    let raw = read_input(args.input)?;
    let cases = raw
        .iter()
        .enumerate()
        .map(|(index, row)| {
            summarize_case(row).with_context(|| format!("sustained input row {}", index + 1))
        })
        .collect::<Result<Vec<_>>>()?;
    if cases.is_empty() {
        bail!("no sustained measurements found");
    }
    let groups = aggregate(&cases)?;
    let mut warnings = Vec::<Value>::new();
    for case in &cases {
        let instrumentation = case
            .get("instrumentation_fraction")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        if instrumentation > 0.10 {
            warnings.push(Value::String(format!(
                "{} {} {} {} trial={}: instrumentation is {:.1}% of sustained foreground wall time; use larger windows before interpreting cliff/performance magnitude",
                py_string(case.get("scenario").unwrap_or(&Value::Null)),
                py_string(case.get("engine").unwrap_or(&Value::Null)),
                py_string(case.get("durability").unwrap_or(&Value::Null)),
                py_string(case.get("pattern").unwrap_or(&Value::Null)),
                py_string(case.get("trial").unwrap_or(&Value::Null)),
                100.0 * instrumentation,
            )));
        }
    }
    let problems = trial_problems(&groups, args.expect_trials)?;
    let result = json!({
        "case_count": cases.len(),
        "group_count": groups.len(),
        "problems": problems,
        "warnings": warnings,
        "cases": cases,
        "groups": groups,
    });
    let json_text = serde_json::to_string_pretty(&result)? + "\n";
    if let Some(path) = args.json_out {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, json_text)?;
    } else {
        print!("{json_text}");
    }
    if let Some(path) = args.markdown_out {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(
            path,
            render_markdown(result["groups"].as_array().context("groups missing")?),
        )?;
    }
    Ok(i32::from(
        result["problems"]
            .as_array()
            .is_some_and(|problems| !problems.is_empty()),
    ))
}

#[cfg(test)]
mod tests {
    use super::{aggregate, group_key, summarize_case, trial_problems};
    use anyhow::{Context, Result};
    use serde_json::{Value, json};

    fn identity_case() -> Value {
        json!({
            "format_version": 7,
            "lane": "record-sustained",
            "scenario": "record-sustained",
            "engine": "sqlite",
            "engine_version": "1",
            "durability": "sync",
            "durability_mapping": "full",
            "pattern": "append",
            "records": 100,
            "ops_requested": 1000,
            "window_ops": 100,
            "value_bytes": null,
            "payload_bytes": 512,
            "value_pattern": null,
            "key_bytes": null,
            "key_shape": null,
            "txn_size": 100,
            "read_materialization": "full-record-v1",
            "write_materialization": "no-return-v1",
        })
    }

    fn raw_case(trial: i64, instrumentation: f64) -> Result<Value> {
        let mut raw = json!({
            "format_version": 7,
            "lane": "record-sustained",
            "scenario": "record-sustained",
            "engine": "sqlite",
            "engine_version": "1",
            "durability": "sync",
            "durability_mapping": "full",
            "pattern": "append",
            "records": 100,
            "ops_requested": 500,
            "ops_completed": 500,
            "window_ops": 100,
            "value_bytes": null,
            "payload_bytes": 512,
            "value_pattern": null,
            "key_bytes": null,
            "key_shape": null,
            "txn_size": 100,
            "read_materialization": "full-record-v1",
            "write_materialization": "no-return-v1",
            "trial": trial,
            "seed": 42, "elapsed_s": 6.25, "active_elapsed_s": 5.5,
            "puts": 500, "deletes": 0, "transactions": 5, "transaction_latency": {"count": 5, "p99_us": 30.0},
            "ops_per_s": 80.0,
            "active_ops_per_s": 90.0,
            "instrumentation_fraction_of_wall": instrumentation,
            "logical_mutated_bytes": 262_000,
            "measured_process": {"cpu_runtime_ns": 100_000, "write_bytes": 20000, "runqueue_wait_fraction_of_wall": 0.1},
            "db_bytes_before": 1000,
            "db_bytes_after_foreground": 2000,
            "db_bytes_final": 2200,
            "windows": [
                {"ops_start":0,"ops_completed":100,"ops_per_s":100.0,"logical_mutated_bytes":52400,"transaction_latency":{"p99_us":10.0},"process":{"cpu_runtime_ns":1000,"write_bytes":2000,"runqueue_wait_fraction_of_wall":0.1},"system":{"accounting_wall_ns":1_000_000,"psi_cpu_some_us":10,"psi_io_full_us":5}},
                {"ops_start":100,"ops_completed":100,"ops_per_s":100.0,"logical_mutated_bytes":52400,"transaction_latency":{"p99_us":12.0},"process":{"cpu_runtime_ns":1100,"write_bytes":2100,"runqueue_wait_fraction_of_wall":0.2},"system":{"accounting_wall_ns":1_000_000,"psi_cpu_some_us":20,"psi_io_full_us":6}},
                {"ops_start":200,"ops_completed":100,"ops_per_s":100.0,"logical_mutated_bytes":52400,"transaction_latency":{"p99_us":11.0},"process":{"cpu_runtime_ns":1200,"write_bytes":2200,"runqueue_wait_fraction_of_wall":0.3},"system":{"accounting_wall_ns":1_000_000,"psi_cpu_some_us":30,"psi_io_full_us":7}},
                {"ops_start":300,"ops_completed":100,"ops_per_s":40.0,"logical_mutated_bytes":52400,"transaction_latency":{"p99_us":30.0},"process":{"cpu_runtime_ns":1300,"write_bytes":2300,"runqueue_wait_fraction_of_wall":0.4},"system":{"accounting_wall_ns":1_000_000,"psi_cpu_some_us":40,"psi_io_full_us":8}},
                {"ops_start":400,"ops_completed":100,"ops_per_s":95.0,"logical_mutated_bytes":52400,"transaction_latency":{"p99_us":15.0},"process":{"cpu_runtime_ns":1400,"write_bytes":2400,"runqueue_wait_fraction_of_wall":0.5},"system":{"accounting_wall_ns":1_000_000,"psi_cpu_some_us":50,"psi_io_full_us":9}}
            ],
            "post_workload_settle": {"requested_ms":1000,"sample_ms":500,"elapsed_s":1.0,"process":{"cpu_runtime_ns":50,"write_bytes":500,"runqueue_wait_fraction_of_wall":0.1},"db_bytes_before":2000,"db_bytes_after":2100,"samples":[1,2]}
        });
        let mut active = 0.0;
        for (index, window) in raw["windows"]
            .as_array_mut()
            .context("fixture windows missing")?
            .iter_mut()
            .enumerate()
        {
            window["index"] = json!(index);
            window["ops_end"] = json!((index + 1) * 100);
            window["elapsed_s"] = json!(
                100.0
                    / window["ops_per_s"]
                        .as_f64()
                        .context("fixture rate missing")?
            );
            active += window["elapsed_s"]
                .as_f64()
                .context("fixture elapsed missing")?;
            window["puts"] = json!(100);
            window["deletes"] = json!(0);
            window["transactions"] = json!(1);
            window["transaction_latency"]["count"] = json!(1);
        }
        let wall = active / (1.0 - instrumentation);
        raw["elapsed_s"] = json!(wall);
        raw["active_elapsed_s"] = json!(active);
        raw["ops_per_s"] = json!(500.0 / wall);
        raw["active_ops_per_s"] = json!(500.0 / active);
        Ok(raw)
    }

    #[test]
    fn materialization_markers_are_group_identity() {
        let corrected = identity_case();
        let mut old_read = corrected.clone();
        old_read["read_materialization"] = json!("legacy-read-v0");
        let mut old_write = corrected.clone();
        old_write["write_materialization"] = json!("legacy-return-v0");
        assert_ne!(group_key(&corrected), group_key(&old_read));
        assert_ne!(group_key(&corrected), group_key(&old_write));
    }

    #[test]
    fn missing_markers_default_to_legacy() {
        let mut missing = identity_case();
        missing
            .as_object_mut()
            .and_then(|map| map.remove("read_materialization"));
        missing
            .as_object_mut()
            .and_then(|map| map.remove("write_materialization"));
        let mut legacy = identity_case();
        legacy["read_materialization"] = json!("legacy-read-v0");
        legacy["write_materialization"] = json!("legacy-return-v0");
        assert_eq!(group_key(&missing), group_key(&legacy));
    }

    #[test]
    fn malformed_windows_and_duplicate_trials_are_rejected() -> Result<()> {
        let raw = raw_case(1, 0.12)?;
        for (key, value) in [
            ("index", json!(3)),
            ("ops_start", json!(999)),
            ("ops_end", json!(0)),
            ("ops_completed", json!(99)),
            ("transactions", json!(2)),
            ("puts", json!(99)),
            ("logical_mutated_bytes", json!(1)),
            ("ops_per_s", json!(0)),
            ("elapsed_s", json!(-1)),
        ] {
            let mut invalid = raw.clone();
            invalid["windows"][0][key] = value;
            assert!(summarize_case(&invalid).is_err(), "{key}");
        }
        for (field, value) in [
            ("ops_per_s", json!(42)),
            ("active_elapsed_s", json!(1)),
            ("instrumentation_fraction_of_wall", json!(0.5)),
            ("logical_mutated_bytes", json!(0)),
            ("trial", json!(0)),
        ] {
            let mut invalid = raw.clone();
            invalid[field] = value;
            assert!(summarize_case(&invalid).is_err(), "{field}");
        }
        let case = summarize_case(&raw)?;
        assert!(aggregate(&[case.clone(), case]).is_err());
        let mut truncated = raw.clone();
        truncated["windows"]
            .as_array_mut()
            .context("fixture windows")?
            .pop();
        assert!(summarize_case(&truncated).is_err());
        let mut other_seed = raw_case(2, 0.05)?;
        let mut other_settle = other_seed.clone();
        other_settle["post_workload_settle"]["requested_ms"] = json!(2000);
        assert_eq!(
            aggregate(&[summarize_case(&raw)?, summarize_case(&other_settle)?])?.len(),
            2
        );
        other_seed["seed"] = json!(43);
        assert_eq!(
            aggregate(&[summarize_case(&raw)?, summarize_case(&other_seed)?])?.len(),
            2
        );
        Ok(())
    }

    #[test]
    fn cliff_recovery_and_aggregation_are_preserved() -> Result<()> {
        let first = summarize_case(&raw_case(1, 0.12)?)?;
        let second = summarize_case(&raw_case(2, 0.05)?)?;
        assert_eq!(first["first_below_50pct_window"], 3);
        assert_eq!(first["first_recovered_90pct_window"], 4);
        assert_eq!(first["longest_run_below_50pct_baseline"], 1);
        let groups = aggregate(&[first, second])?;
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["trials"], 2);
        assert_eq!(groups[0]["trial_ids"], json!([1, 2]));
        assert_eq!(trial_problems(&groups, Some(2))?, Vec::<Value>::new());
        assert_eq!(trial_problems(&groups, Some(3))?.len(), 1);
        let missing_first = aggregate(&[
            summarize_case(&raw_case(2, 0.1)?)?,
            summarize_case(&raw_case(3, 0.1)?)?,
        ])?;
        assert_eq!(trial_problems(&missing_first, Some(2))?.len(), 1);
        assert_eq!(groups[0]["trials_with_50pct_cliff"], 2);
        Ok(())
    }
}
