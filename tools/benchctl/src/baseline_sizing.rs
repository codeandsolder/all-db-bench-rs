use crate::json_io::read_json;
use crate::json_value::json_truthy;
use crate::json_value::py_string;
use crate::numeric::align_up;
use anyhow::{Context, Result, bail};
use num_traits::ToPrimitive;
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
};

pub const SIZING_POLICY_VERSION: i64 = 3;
const READ_ONLY_WORKLOADS: [&str; 3] = ["indexed-read", "point-read", "range-scan"];
const GROUP_FIELDS: [&str; 21] = [
    "lane",
    "scenario",
    "engine",
    "engine_version",
    "durability",
    "workload",
    "read_materialization",
    "write_materialization",
    "records",
    "ops_requested",
    "clients",
    "value_bytes",
    "value_pattern",
    "key_bytes",
    "key_shape",
    "access_pattern",
    "miss_percent",
    "write_pattern",
    "txn_size",
    "scan_len",
    "settle_ms",
];

#[derive(Clone, Copy)]
pub struct SizingConfig {
    pub(crate) read_only_min_seconds: f64,
    pub(crate) read_only_target_seconds: f64,
    pub(crate) stateful_min_total_seconds: f64,
    pub(crate) stateful_target_total_seconds: f64,
    pub(crate) stateful_max_trials: i64,
    pub(crate) max_cv: f64,
    pub(crate) max_relative_spread: f64,
}

impl Default for SizingConfig {
    fn default() -> Self {
        Self {
            read_only_min_seconds: 2.0,
            read_only_target_seconds: 3.0,
            stateful_min_total_seconds: 0.75,
            stateful_target_total_seconds: 1.0,
            stateful_max_trials: 51,
            max_cv: 0.10,
            max_relative_spread: 0.25,
        }
    }
}

#[derive(Clone, Copy)]
pub struct AuditArgs<'a> {
    pub(crate) results: &'a Path,
    pub(crate) expect_trials: i64,
    pub(crate) config: SizingConfig,
    pub(crate) json_out: Option<&'a Path>,
    pub(crate) markdown_out: Option<&'a Path>,
}

#[derive(Clone, Copy)]
pub struct RefineArgs<'a> {
    pub(crate) quality_plan: &'a Path,
    pub(crate) sizing_audit: &'a Path,
    pub(crate) resize_root: &'a Path,
    pub(crate) resize_prefix: &'a str,
    pub(crate) json_out: &'a Path,
    pub(crate) markdown_out: Option<&'a Path>,
}

fn value_i64(value: Option<&Value>) -> Result<i64> {
    match value.context("missing integer value")? {
        Value::Number(value) => value.as_i64().context("integer is outside i64 range"),
        Value::String(raw) => raw
            .parse::<i64>()
            .with_context(|| format!("invalid integer {raw:?}")),
        other => bail!("expected integer-compatible value, got {other}"),
    }
}

fn value_f64(value: Option<&Value>) -> Result<f64> {
    match value.context("missing numeric value")? {
        Value::Number(value) => value.as_f64().context("number is outside f64 range"),
        Value::String(raw) => raw
            .parse::<f64>()
            .with_context(|| format!("invalid number {raw:?}")),
        other => bail!("expected numeric-compatible value, got {other}"),
    }
}

fn identity_value(row: &Value, field: &str) -> Value {
    if let Some(value) = row.get(field) {
        return value.clone();
    }
    match field {
        "lane" => json!("kv"),
        "read_materialization" => json!("legacy-read-v0"),
        "write_materialization" => json!("legacy-return-v0"),
        _ => Value::Null,
    }
}

fn group_key(row: &Value) -> Vec<Value> {
    GROUP_FIELDS
        .into_iter()
        .map(|field| identity_value(row, field))
        .collect()
}

fn encoded_key(key: &[Value]) -> Result<String> {
    serde_json::to_string(key).context("failed to serialize sizing group key")
}

fn sort_key(key: &[Value]) -> Vec<String> {
    key.iter().map(py_string).collect()
}

fn round_up_ops(value: f64) -> Result<i64> {
    if !value.is_finite() || value <= 0.0 {
        bail!("invalid operation recommendation: {value}");
    }
    let requested = value
        .ceil()
        .max(1.0)
        .to_i64()
        .context("operation recommendation exceeds integer range")?;
    let quantum = if requested < 1_000 {
        10
    } else if requested < 10_000 {
        100
    } else if requested < 100_000 {
        1_000
    } else if requested < 1_000_000 {
        10_000
    } else {
        100_000
    };
    align_up(requested, quantum)
}

fn round_up_odd_trials(value: f64) -> Result<i64> {
    let mut requested = value
        .ceil()
        .to_i64()
        .context("trial recommendation exceeds integer range")?
        .max(3);
    if requested % 2 == 0 {
        requested += 1;
    }
    Ok(requested)
}

fn round_up_trials(value: f64, maximum: i64) -> Result<i64> {
    if maximum < 3 {
        bail!("stateful maximum trials must be at least 3");
    }
    let cap = if maximum % 2 == 1 {
        maximum
    } else {
        maximum - 1
    };
    Ok(round_up_odd_trials(value)?.min(cap))
}

fn runner_ops_override(row: &Value, effective_ops: i64) -> Result<i64> {
    if row.get("workload").and_then(Value::as_str) != Some("tiny-txn") {
        return Ok(effective_ops);
    }
    let lane = row.get("lane").and_then(Value::as_str).unwrap_or("kv");
    match lane {
        "kv" => effective_ops
            .checked_mul(10)
            .context("KV runner operation count overflow"),
        "record" => effective_ops
            .checked_mul(2)
            .context("record runner operation count overflow"),
        other => bail!("unknown tiny-txn baseline lane: {other:?}"),
    }
}

fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut values = values.to_vec();
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        Some(f64::midpoint(values[mid - 1], values[mid]))
    } else {
        Some(values[mid])
    }
}

fn cv(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mean = values.iter().sum::<f64>() / int_as_f64!(values.len());
    if values.len() < 2 || mean == 0.0 {
        return 0.0;
    }
    let variance = values
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f64>()
        / int_as_f64!((values.len() - 1));
    variance.sqrt() / mean
}

fn comma_int(value: i64) -> String {
    let negative = value < 0;
    let digits = value.unsigned_abs().to_string();
    let mut out = String::new();
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    if negative { format!("-{out}") } else { out }
}

fn field_string(row: &Value, field: &str) -> String {
    row.get(field).map_or_else(|| "None".to_owned(), py_string)
}

#[derive(Serialize)]
struct TrialStatistics {
    trials: Vec<i64>,
    #[serde(rename = "median_elapsed_s")]
    median_elapsed: f64,
    #[serde(rename = "measured_total_s")]
    measured_total: f64,
    #[serde(rename = "min_elapsed_s")]
    min_elapsed: f64,
    #[serde(rename = "max_elapsed_s")]
    max_elapsed: f64,
    #[serde(rename = "median_ops_per_s")]
    median_rate: f64,
    throughput_cv: f64,
    throughput_relative_spread: f64,
}

impl TrialStatistics {
    fn from_rows(rows: &[&Value]) -> Result<Self> {
        let mut trials = Vec::with_capacity(rows.len());
        let mut elapsed = Vec::with_capacity(rows.len());
        let mut rates = Vec::with_capacity(rows.len());
        for row in rows {
            trials.push(value_i64(row.get("trial"))?);
            let seconds = value_f64(row.get("elapsed_s"))?;
            let rate = value_f64(row.get("ops_per_s"))?;
            if !seconds.is_finite() || seconds <= 0.0 || !rate.is_finite() || rate <= 0.0 {
                bail!("invalid sizing sample: elapsed_s={seconds}, ops_per_s={rate}");
            }
            elapsed.push(seconds);
            rates.push(rate);
        }
        trials.sort_unstable();
        let median_elapsed = median(&elapsed).context("sizing group has no elapsed values")?;
        let measured_total = elapsed.iter().sum::<f64>();
        let median_rate = median(&rates).context("sizing group has no throughput values")?;
        let throughput_cv = cv(&rates);
        let throughput_relative_spread = (rates.iter().copied().fold(f64::NEG_INFINITY, f64::max)
            - rates.iter().copied().fold(f64::INFINITY, f64::min))
            / median_rate;
        if !measured_total.is_finite()
            || !throughput_cv.is_finite()
            || !throughput_relative_spread.is_finite()
        {
            bail!("non-finite sizing statistics");
        }
        Ok(Self {
            trials,
            median_elapsed,
            measured_total,
            min_elapsed: elapsed.iter().copied().fold(f64::INFINITY, f64::min),
            max_elapsed: elapsed.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            median_rate,
            throughput_cv,
            throughput_relative_spread,
        })
    }

    fn validate_trials(&self, first: &Value, expect_trials: i64, problems: &mut Vec<String>) {
        if usize::try_from(expect_trials).ok() != Some(self.trials.len())
            || self
                .trials
                .iter()
                .enumerate()
                .any(|(index, trial)| i64::try_from(index + 1).ok() != Some(*trial))
        {
            problems.push(format!(
                "{}/{}/{}: trials={:?}, expected 1..={expect_trials}",
                field_string(first, "engine"),
                field_string(first, "durability"),
                field_string(first, "workload"),
                self.trials,
            ));
        }
    }
}

#[derive(Serialize)]
struct ResizeDecision {
    resize_strategy: Option<&'static str>,
    suggested_effective_ops: Option<i64>,
    runner_ops_override: Option<i64>,
    suggested_trials: Option<i64>,
    minimum_required_trials: Option<i64>,
    sampling_cap_insufficient: bool,
}

impl ResizeDecision {
    const fn none() -> Self {
        Self {
            resize_strategy: None,
            suggested_effective_ops: None,
            runner_ops_override: None,
            suggested_trials: None,
            minimum_required_trials: None,
            sampling_cap_insufficient: false,
        }
    }

    fn needed(
        first: &Value,
        stats: &TrialStatistics,
        read_only: bool,
        expect_trials: i64,
        config: SizingConfig,
    ) -> Result<Self> {
        let effective_ops = value_i64(first.get("ops_requested"))?;
        if read_only {
            let scaled = int_as_f64!(effective_ops) * config.read_only_target_seconds
                / stats.median_elapsed.max(1e-9);
            let suggested = round_up_ops(scaled)?;
            return Ok(Self {
                resize_strategy: Some("more-ops"),
                suggested_effective_ops: Some(suggested),
                runner_ops_override: Some(runner_ops_override(first, suggested)?),
                suggested_trials: Some(expect_trials),
                ..Self::none()
            });
        }
        let minimum = round_up_odd_trials(
            config.stateful_min_total_seconds / stats.median_elapsed.max(1e-9),
        )?;
        let suggested = round_up_trials(
            config.stateful_target_total_seconds / stats.median_elapsed.max(1e-9),
            config.stateful_max_trials,
        )?;
        let cap = if config.stateful_max_trials % 2 == 1 {
            config.stateful_max_trials
        } else {
            config.stateful_max_trials - 1
        };
        Ok(Self {
            resize_strategy: Some("more-trials"),
            suggested_effective_ops: Some(effective_ops),
            runner_ops_override: Some(runner_ops_override(first, effective_ops)?),
            suggested_trials: Some(suggested),
            minimum_required_trials: Some(minimum),
            sampling_cap_insufficient: minimum > cap,
        })
    }
}

fn report_group(
    rows: &[&Value],
    expect_trials: i64,
    config: SizingConfig,
    problems: &mut Vec<String>,
) -> Result<Value> {
    let first = rows.first().context("internal empty sizing group")?;
    let evidence = TrialStatistics::from_rows(rows)?;
    evidence.validate_trials(first, expect_trials, problems);
    let workload = field_string(first, "workload");
    let read_only = READ_ONLY_WORKLOADS.contains(&workload.as_str());
    let undersized = if read_only {
        evidence.median_elapsed < config.read_only_min_seconds
    } else {
        evidence.measured_total < config.stateful_min_total_seconds
    };
    let variable = evidence.throughput_cv > config.max_cv
        || evidence.throughput_relative_spread > config.max_relative_spread;
    let status = if undersized {
        "undersized"
    } else if variable {
        "variable"
    } else {
        "accepted"
    };
    let decision = if undersized {
        ResizeDecision::needed(first, &evidence, read_only, expect_trials, config)?
    } else {
        ResizeDecision::none()
    };
    if decision.sampling_cap_insufficient {
        let cap = if config.stateful_max_trials % 2 == 1 {
            config.stateful_max_trials
        } else {
            config.stateful_max_trials - 1
        };
        let minimum = decision
            .minimum_required_trials
            .context("sampling requirement missing")?;
        problems.push(format!(
            "{}/{}/{workload}: stateful sampling cap={cap} trials cannot meet the minimum floor; requires at least {minimum} trials",
            field_string(first, "engine"), field_string(first, "durability"),
        ));
    }
    let effective_ops = value_i64(first.get("ops_requested"))?;
    if effective_ops <= 0 {
        bail!("invalid sizing operation count: {effective_ops}");
    }
    let mut report = serde_json::Map::from_iter([
        ("engine".into(), identity_value(first, "engine")),
        (
            "engine_version".into(),
            identity_value(first, "engine_version"),
        ),
        ("durability".into(), identity_value(first, "durability")),
        ("workload".into(), json!(workload)),
        (
            "read_materialization".into(),
            identity_value(first, "read_materialization"),
        ),
        (
            "write_materialization".into(),
            identity_value(first, "write_materialization"),
        ),
        ("records".into(), identity_value(first, "records")),
        ("ops_requested".into(), json!(effective_ops)),
        ("read_only".into(), json!(read_only)),
        ("status".into(), json!(status)),
    ]);
    for metrics in [
        serde_json::to_value(evidence)?,
        serde_json::to_value(decision)?,
    ] {
        let Value::Object(fields) = metrics else {
            bail!("invalid sizing report fields");
        };
        report.extend(fields);
    }
    Ok(Value::Object(report))
}

pub fn audit_rows(rows: &[Value], expect_trials: i64, config: SizingConfig) -> Result<Value> {
    if expect_trials <= 0 {
        bail!("expected trials must be positive");
    }
    let mut grouped = HashMap::<String, (Vec<Value>, Vec<&Value>)>::new();
    for row in rows {
        let key = group_key(row);
        let encoded = encoded_key(&key)?;
        grouped
            .entry(encoded)
            .or_insert_with(|| (key, Vec::new()))
            .1
            .push(row);
    }
    let mut groups = grouped.into_values().collect::<Vec<_>>();
    groups.sort_by_key(|(key, _)| sort_key(key));

    let mut output_groups = Vec::new();
    let mut problems = Vec::new();
    for (_, rows) in groups {
        output_groups.push(report_group(&rows, expect_trials, config, &mut problems)?);
    }

    let count_status = |status: &str| {
        output_groups
            .iter()
            .filter(|group| group.get("status").and_then(Value::as_str) == Some(status))
            .count()
    };
    let count_strategy = |strategy: &str| {
        output_groups
            .iter()
            .filter(|group| group.get("resize_strategy").and_then(Value::as_str) == Some(strategy))
            .count()
    };
    Ok(json!({
        "sizing_policy_version": SIZING_POLICY_VERSION,
        "row_count": rows.len(),
        "group_count": output_groups.len(),
        "thresholds": {
            "expect_trials": expect_trials,
            "read_only_workloads": READ_ONLY_WORKLOADS,
            "read_only_min_seconds": config.read_only_min_seconds,
            "read_only_target_seconds": config.read_only_target_seconds,
            "stateful_min_total_seconds": config.stateful_min_total_seconds,
            "stateful_target_total_seconds": config.stateful_target_total_seconds,
            "stateful_max_trials": config.stateful_max_trials,
            "max_cv": config.max_cv,
            "max_relative_spread": config.max_relative_spread,
        },
        "counts": {
            "accepted": count_status("accepted"),
            "undersized": count_status("undersized"),
            "variable": count_status("variable"),
        },
        "strategy_counts": {
            "more-ops": count_strategy("more-ops"),
            "more-trials": count_strategy("more-trials"),
        },
        "problems": problems,
        "groups": output_groups,
    }))
}

fn percent(value: f64, digits: usize) -> String {
    let percentage = value * 100.0;
    format!("{percentage:.digits$}%")
}

fn render_audit_group(group: &Value) -> Result<String> {
    let suggested = group.get("suggested_effective_ops").and_then(|value| {
        if value.is_null() {
            None
        } else {
            value_i64(Some(value)).ok()
        }
    });
    let runner = group.get("runner_ops_override").and_then(|value| {
        if value.is_null() {
            None
        } else {
            value_i64(Some(value)).ok()
        }
    });
    let suggested_trials = group.get("suggested_trials").and_then(|value| {
        if value.is_null() {
            None
        } else {
            value_i64(Some(value)).ok()
        }
    });
    let trials = group
        .get("trials")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let status = group.get("status").and_then(Value::as_str).unwrap_or("");
    let strategy = group
        .get("resize_strategy")
        .and_then(Value::as_str)
        .unwrap_or("—");
    let engine = field_string(group, "engine");
    let durability = field_string(group, "durability");
    let workload = field_string(group, "workload");
    let ops = comma_int(value_i64(group.get("ops_requested"))?);
    let measured_total_s = value_f64(group.get("measured_total_s"))?;
    let median_elapsed_s = value_f64(group.get("median_elapsed_s"))?;
    let cv = percent(value_f64(group.get("throughput_cv"))?, 1);
    let spread = percent(value_f64(group.get("throughput_relative_spread"))?, 1);
    let suggested_ops = suggested.map_or_else(|| "—".to_owned(), comma_int);
    let suggested_trials =
        suggested_trials.map_or_else(|| "—".to_owned(), |value| value.to_string());
    let runner_ops = runner.map_or_else(|| "—".to_owned(), comma_int);
    Ok(format!(
        "| {status} | {strategy} | {engine} | {durability} | {workload} | {ops} | {trials} | {measured_total_s:.3} | {median_elapsed_s:.3} | {cv} | {spread} | {suggested_ops} | {suggested_trials} | {runner_ops} |"
    ))
}

pub fn render_audit_markdown(report: &Value) -> Result<String> {
    let counts = report
        .get("counts")
        .and_then(Value::as_object)
        .context("audit counts missing")?;
    let strategies = report
        .get("strategy_counts")
        .and_then(Value::as_object)
        .context("audit strategy_counts missing")?;
    let thresholds = report
        .get("thresholds")
        .and_then(Value::as_object)
        .context("audit thresholds missing")?;
    let read_only = thresholds
        .get("read_only_workloads")
        .and_then(Value::as_array)
        .context("read_only_workloads missing")?
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    let mut lines = vec![
        "# Baseline sizing audit".to_owned(),
        String::new(),
        format!(
            "Policy v{}. Rows: **{}**; groups: **{}**; accepted: **{}**; undersized: **{}**; variable: **{}**.",
            value_i64(report.get("sizing_policy_version"))?,
            value_i64(report.get("row_count"))?,
            value_i64(report.get("group_count"))?,
            value_i64(counts.get("accepted"))?,
            value_i64(counts.get("undersized"))?,
            value_i64(counts.get("variable"))?,
        ),
        String::new(),
        format!(
            "Read-only workloads ({read_only}) are undersized when their median trial is < {:.2} s and resize ops toward {:.2} s. Stateful workloads preserve the original per-trial op count and are undersized only when their existing trials total < {:.2} s; they add fresh-database trials toward {:.2} s, capped at {} trials. Planned resizes: {} more-ops, {} more-trials.",
            value_f64(thresholds.get("read_only_min_seconds"))?,
            value_f64(thresholds.get("read_only_target_seconds"))?,
            value_f64(thresholds.get("stateful_min_total_seconds"))?,
            value_f64(thresholds.get("stateful_target_total_seconds"))?,
            value_i64(thresholds.get("stateful_max_trials"))?,
            value_i64(strategies.get("more-ops"))?,
            value_i64(strategies.get("more-trials"))?,
        ),
        format!(
            "Groups not requiring more sampling are flagged variable when throughput CV > {} or relative spread > {}; operation count is never increased merely to hide real variability.",
            percent(value_f64(thresholds.get("max_cv"))?, 1),
            percent(value_f64(thresholds.get("max_relative_spread"))?, 1),
        ),
        String::new(),
        "| status | strategy | engine | durability | workload | ops | trials | measured s | median s | CV | spread | suggested ops | suggested trials | runner ops override |".to_owned(),
        "| --- | --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |".to_owned(),
    ];

    let mut groups = report
        .get("groups")
        .and_then(Value::as_array)
        .cloned()
        .context("audit groups missing")?;
    let order = |status: &str| match status {
        "undersized" => 0,
        "variable" => 1,
        _ => 2,
    };
    groups.sort_by(|left, right| {
        let l_status = left.get("status").and_then(Value::as_str).unwrap_or("");
        let r_status = right.get("status").and_then(Value::as_str).unwrap_or("");
        order(l_status)
            .cmp(&order(r_status))
            .then_with(|| {
                left.get("median_elapsed_s")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
                    .total_cmp(
                        &right
                            .get("median_elapsed_s")
                            .and_then(Value::as_f64)
                            .unwrap_or(0.0),
                    )
            })
            .then_with(|| field_string(left, "engine").cmp(&field_string(right, "engine")))
            .then_with(|| field_string(left, "durability").cmp(&field_string(right, "durability")))
            .then_with(|| field_string(left, "workload").cmp(&field_string(right, "workload")))
    });
    for group in &groups {
        lines.push(render_audit_group(group)?);
    }
    if let Some(problems) = report.get("problems").and_then(Value::as_array)
        && !problems.is_empty()
    {
        lines.push(String::new());
        lines.push("## Problems".to_owned());
        lines.push(String::new());
        for problem in problems {
            lines.push(format!("- {}", problem.as_str().unwrap_or("")));
        }
    }
    Ok(lines.join("\n") + "\n")
}

fn read_ndjson(path: &Path) -> Result<Vec<Value>> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .with_context(|| format!("invalid NDJSON in {}", path.display()))
        })
        .collect()
}

pub fn run_audit(args: AuditArgs<'_>) -> Result<i32> {
    let rows = read_ndjson(args.results)?;
    let report = audit_rows(&rows, args.expect_trials, args.config)?;
    if let Some(path) = args.json_out {
        fs::write(path, serde_json::to_string_pretty(&report)? + "\n")
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    let markdown = render_audit_markdown(&report)?;
    if let Some(path) = args.markdown_out {
        fs::write(path, markdown).with_context(|| format!("failed to write {}", path.display()))?;
    } else {
        print!("{markdown}");
    }
    Ok(i32::from(json_truthy(report.get("problems"))))
}

fn slug(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

fn group_run_id(group: &Value, prefix: &str) -> Result<String> {
    Ok(format!(
        "{prefix}-{}-{}-{}-e{}-t{}",
        slug(&field_string(group, "engine")),
        slug(&field_string(group, "durability")),
        slug(&field_string(group, "workload")),
        value_i64(group.get("suggested_effective_ops"))?,
        value_i64(group.get("suggested_trials"))?,
    ))
}

fn config_from_audit(audit: &Value) -> Result<SizingConfig> {
    let thresholds = audit
        .get("thresholds")
        .and_then(Value::as_object)
        .context("sizing audit thresholds missing")?;
    Ok(SizingConfig {
        read_only_min_seconds: value_f64(thresholds.get("read_only_min_seconds"))?,
        read_only_target_seconds: value_f64(thresholds.get("read_only_target_seconds"))?,
        stateful_min_total_seconds: value_f64(thresholds.get("stateful_min_total_seconds"))?,
        stateful_target_total_seconds: value_f64(thresholds.get("stateful_target_total_seconds"))?,
        stateful_max_trials: value_i64(thresholds.get("stateful_max_trials"))?,
        max_cv: value_f64(thresholds.get("max_cv"))?,
        max_relative_spread: value_f64(thresholds.get("max_relative_spread"))?,
    })
}

fn load_repair_audit(
    original: &Value,
    resize_root: &Path,
    initial_rid: &str,
    config: SizingConfig,
) -> Result<Value> {
    let run_dir = resize_root.join(initial_rid);
    let results_path = run_dir.join("results.ndjson");
    let summary_path = run_dir.join("summary.json");
    if !results_path.is_file() || !summary_path.is_file() {
        bail!("missing initial quality-repair result: {initial_rid}");
    }
    let expected_trials = value_i64(original.get("suggested_trials"))?;
    let summary = read_json(&summary_path)?;
    if value_i64(summary.get("row_count"))? != expected_trials
        || value_i64(summary.get("group_count"))? != 1
        || json_truthy(summary.get("problems"))
    {
        bail!("invalid initial quality-repair summary: {initial_rid}");
    }
    let rows = read_ndjson(&results_path)?;
    let report = audit_rows(&rows, expected_trials, config)?;
    if json_truthy(report.get("problems")) || value_i64(report.get("group_count"))? != 1 {
        bail!(
            "clean repair audit failed: {:?}",
            report.get("problems").unwrap_or(&Value::Null)
        );
    }
    let audited = report
        .get("groups")
        .and_then(Value::as_array)
        .and_then(|groups| groups.first())
        .context("clean repair audit returned no group")?;
    validate_repair_identity(original, audited, initial_rid)?;
    Ok(audited.clone())
}

fn validate_repair_identity(original: &Value, audited: &Value, run_id: &str) -> Result<()> {
    for field in [
        "engine",
        "engine_version",
        "durability",
        "workload",
        "records",
        "read_materialization",
        "write_materialization",
    ] {
        if identity_value(original, field) != identity_value(audited, field) {
            bail!("quality repair identity mismatch for {run_id}: {field}");
        }
    }
    if value_i64(original.get("suggested_effective_ops"))?
        != value_i64(audited.get("ops_requested"))?
    {
        bail!("quality repair operation count mismatch for {run_id}");
    }
    Ok(())
}

fn refine_group(original: &Value, audited: &Value, initial_rid: &str) -> Result<(Value, bool)> {
    let mut item = original
        .as_object()
        .cloned()
        .context("quality repair group is not an object")?;
    item.insert(
        "quality_repair_initial_run_id".to_owned(),
        json!(initial_rid),
    );
    for (target, source) in [
        ("quality_repair_clean_status", "status"),
        ("quality_repair_clean_median_elapsed_s", "median_elapsed_s"),
        ("quality_repair_clean_measured_total_s", "measured_total_s"),
        ("quality_repair_clean_throughput_cv", "throughput_cv"),
        (
            "quality_repair_clean_throughput_relative_spread",
            "throughput_relative_spread",
        ),
    ] {
        item.insert(
            target.to_owned(),
            audited.get(source).cloned().unwrap_or(Value::Null),
        );
    }
    let undersized = audited.get("status").and_then(Value::as_str) == Some("undersized");
    item.insert("quality_repair_refined".to_owned(), json!(undersized));
    item.insert(
        "quality_repair_refinement_strategy".to_owned(),
        audited
            .get("resize_strategy")
            .cloned()
            .unwrap_or(Value::Null),
    );
    if undersized {
        if audited
            .get("sampling_cap_insufficient")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            bail!("quality repair cannot meet sampling floor within cap: {initial_rid}");
        }
        for field in [
            "suggested_effective_ops",
            "runner_ops_override",
            "suggested_trials",
        ] {
            item.insert(field.to_owned(), json!(value_i64(audited.get(field))?));
        }
    }
    Ok((Value::Object(item), undersized))
}

pub fn refine_plan(
    quality_plan: &Value,
    sizing_audit: &Value,
    resize_root: &Path,
    resize_prefix: &str,
) -> Result<Value> {
    if value_i64(quality_plan.get("quality_policy_version"))? != 1 {
        bail!(
            "unsupported quality policy: {:?}",
            quality_plan
                .get("quality_policy_version")
                .unwrap_or(&Value::Null)
        );
    }
    let sizing_version = value_i64(sizing_audit.get("sizing_policy_version"))?;
    if !matches!(sizing_version, 2 | 3) {
        bail!(
            "unsupported sizing policy: {:?}",
            sizing_audit
                .get("sizing_policy_version")
                .unwrap_or(&Value::Null)
        );
    }
    let config = config_from_audit(sizing_audit)?;
    let source_groups = quality_plan
        .get("groups")
        .and_then(Value::as_array)
        .context("quality plan groups missing")?;
    let mut refined = Vec::with_capacity(source_groups.len());
    let mut promoted = 0_i64;
    let mut seen = HashSet::new();
    for original in source_groups {
        let run_id = group_run_id(original, resize_prefix)?;
        if !seen.insert(run_id.clone()) {
            bail!("duplicate initial quality-repair run: {run_id}");
        }
        let audited = load_repair_audit(original, resize_root, &run_id, config)?;
        let (item, undersized) = refine_group(original, &audited, &run_id)?;
        promoted += i64::from(undersized);
        refined.push(item);
    }
    let mut result = quality_plan
        .as_object()
        .cloned()
        .context("quality plan is not an object")?;
    result.insert("refinement_version".to_owned(), json!(1));
    result.insert("source_quality_policy_version".to_owned(), json!(1));
    result.insert(
        "source_sizing_policy_version".to_owned(),
        json!(sizing_version),
    );
    result.insert("refined_group_count".to_owned(), json!(promoted));
    result.insert("groups".to_owned(), Value::Array(refined));
    Ok(Value::Object(result))
}

pub fn render_refine_markdown(plan: &Value, resize_prefix: &str) -> Result<String> {
    let groups = plan
        .get("groups")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut lines = vec![
        "# Refined stock short-pressure repair plan".to_owned(),
        String::new(),
        format!(
            "Repair groups: **{}**; promoted after clean re-audit: **{}**.",
            groups.len(),
            value_i64(plan.get("refined_group_count"))?
        ),
        String::new(),
        "| engine | durability | workload | clean status | refinement | ops | trials | run id |"
            .to_owned(),
        "| --- | --- | --- | --- | --- | ---: | ---: | --- |".to_owned(),
    ];
    for group in &groups {
        let status = group
            .get("quality_repair_clean_status")
            .map_or_else(|| "—".to_owned(), py_string);
        let strategy = group
            .get("quality_repair_refinement_strategy")
            .and_then(Value::as_str)
            .unwrap_or("—");
        let engine = field_string(group, "engine");
        let durability = field_string(group, "durability");
        let workload = field_string(group, "workload");
        let suggested_ops = comma_int(value_i64(group.get("suggested_effective_ops"))?);
        let suggested_trials = value_i64(group.get("suggested_trials"))?;
        let run_id = group_run_id(group, resize_prefix)?;
        lines.push(format!(
            "| {engine} | {durability} | {workload} | {status} | {strategy} | {suggested_ops} | {suggested_trials} | `{run_id}` |"
        ));
    }
    Ok(lines.join("\n") + "\n")
}

pub fn run_refine(args: RefineArgs<'_>) -> Result<()> {
    let quality = read_json(args.quality_plan)?;
    let sizing = read_json(args.sizing_audit)?;
    let refined = refine_plan(&quality, &sizing, args.resize_root, args.resize_prefix)?;
    if let Some(parent) = args.json_out.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        args.json_out,
        serde_json::to_string_pretty(&refined)? + "\n",
    )?;
    let markdown = render_refine_markdown(&refined, args.resize_prefix)?;
    if let Some(path) = args.markdown_out {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, markdown)?;
    } else {
        print!("{markdown}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{SizingConfig, audit_rows, group_run_id, refine_plan, round_up_ops};
    use anyhow::Context;
    use anyhow::Result;
    use serde_json::{Value, json};
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    fn row(engine: &str, elapsed: f64, rate: f64, trial: i64, workload: &str) -> Value {
        json!({
            "lane":"kv","scenario":"baseline-core","engine":engine,"engine_version":"1",
            "durability":"sync","workload":workload,"records":100_000,"ops_requested":50000,
            "clients":1,"value_bytes":256,"value_pattern":"pseudo-random","key_bytes":8,
            "key_shape":"sequential","access_pattern":"auto","miss_percent":0,
            "write_pattern":"append","txn_size":100,"scan_len":100,"settle_ms":0,
            "trial":trial,"elapsed_s":elapsed,"ops_per_s":rate
        })
    }

    #[test]
    fn read_only_classifies_and_resizes() -> Result<()> {
        let mut rows = Vec::new();
        for (engine, elapsed, rates) in [
            ("accepted", 3.0, [100.0, 102.0, 98.0]),
            ("short", 0.5, [1000.0, 1010.0, 990.0]),
            ("variable", 5.0, [100.0, 130.0, 70.0]),
        ] {
            for (index, rate) in rates.into_iter().enumerate() {
                rows.push(row(
                    engine,
                    elapsed,
                    rate,
                    i64::try_from(index).unwrap_or(i64::MAX) + 1,
                    "point-read",
                ));
            }
        }
        let report = audit_rows(&rows, 3, SizingConfig::default())?;
        assert_eq!(
            report["counts"],
            json!({"accepted":1,"undersized":1,"variable":1})
        );
        let short = report["groups"]
            .as_array()
            .and_then(|groups| groups.iter().find(|group| group["engine"] == "short"))
            .cloned()
            .unwrap_or(Value::Null);
        assert_eq!(short["suggested_effective_ops"], 300_000);
        assert_eq!(short["runner_ops_override"], 300_000);
        Ok(())
    }

    #[test]
    fn stateful_preserves_ops_and_adds_trials() -> Result<()> {
        let rows = (1..=3)
            .map(|trial| row("tiny", 0.1, 50000.0, trial, "tiny-txn"))
            .collect::<Vec<_>>();
        let report = audit_rows(&rows, 3, SizingConfig::default())?;
        let group = &report["groups"][0];
        assert_eq!(group["resize_strategy"], "more-trials");
        assert_eq!(group["suggested_effective_ops"], 50000);
        assert_eq!(group["runner_ops_override"], 500_000);
        assert_eq!(group["suggested_trials"], 11);
        Ok(())
    }

    #[test]
    fn rejects_nonpositive_and_nonfinite_sizing_samples() -> Result<()> {
        for (field, invalid) in [
            ("elapsed_s", json!(0.0)),
            ("elapsed_s", json!(-1.0)),
            ("elapsed_s", json!("NaN")),
            ("ops_per_s", json!(0.0)),
            ("ops_per_s", json!("inf")),
            ("ops_requested", json!(0)),
        ] {
            let mut invalid_row = row("bad", 3.0, 100.0, 1, "point-read");
            invalid_row[field] = invalid;
            assert!(
                audit_rows(&[invalid_row], 1, SizingConfig::default()).is_err(),
                "{field}"
            );
        }
        let report = audit_rows(
            &[row("huge", 3.0, 100.0, 1, "point-read")],
            i64::MAX,
            SizingConfig::default(),
        )?;
        assert_eq!(report["problems"].as_array().map(Vec::len), Some(1));
        Ok(())
    }

    #[test]
    fn quality_refinement_requires_matching_measurement_identity() -> Result<()> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "sizing-repair-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let stock = row("fast", 3.0, 20_000.0, 1, "point-read");
        let audit = audit_rows(std::slice::from_ref(&stock), 1, SizingConfig::default())?;
        let mut group = audit["groups"][0].clone();
        group["suggested_effective_ops"] = json!(50_000);
        group["suggested_trials"] = json!(1);
        let quality = json!({"quality_policy_version":1,"groups":[group.clone()]});
        let run_dir = root.join(group_run_id(&group, "resize")?);
        fs::create_dir_all(&run_dir)?;
        let result = (|| -> Result<()> {
            fs::write(
                run_dir.join("summary.json"),
                r#"{"row_count":1,"group_count":1,"problems":[]}"#,
            )?;
            let results = run_dir.join("results.ndjson");
            let write = |sample: &Value| -> Result<()> {
                fs::write(&results, format!("{}\n", serde_json::to_string(sample)?))?;
                Ok(())
            };
            write(&stock)?;
            let refined = refine_plan(&quality, &audit, &root, "resize")?;
            assert_eq!(refined["refined_group_count"], 0);
            assert_eq!(
                refined["groups"][0]["quality_repair_clean_status"],
                "accepted"
            );
            let mut shorter = stock.clone();
            shorter["elapsed_s"] = json!(0.5);
            write(&shorter)?;
            let refined = refine_plan(&quality, &audit, &root, "resize")?;
            assert_eq!(refined["refined_group_count"], 1);
            assert_eq!(refined["groups"][0]["suggested_effective_ops"], 300_000);
            for (field, value) in [
                ("engine", json!("other")),
                ("engine_version", json!("2")),
                ("records", json!(99_999)),
                ("ops_requested", json!(49_999)),
                ("workload", json!("range-scan")),
                ("durability", json!("relaxed")),
                ("read_materialization", json!("different")),
                ("write_materialization", json!("different")),
            ] {
                let mut invalid = stock.clone();
                invalid[field] = value;
                write(&invalid)?;
                assert!(
                    refine_plan(&quality, &audit, &root, "resize").is_err(),
                    "{field}"
                );
            }
            write(&stock)?;
            let duplicate = json!({"quality_policy_version":1,"groups":[group.clone(),group]});
            assert!(refine_plan(&duplicate, &audit, &root, "resize").is_err());
            Ok(())
        })();
        fs::remove_dir_all(root).context("remove quality refinement test directory")?;
        result
    }

    #[test]
    fn operation_rounding_matches_policy() -> Result<()> {
        assert_eq!(round_up_ops(1.0)?, 10);
        assert_eq!(round_up_ops(999.1)?, 1000);
        assert_eq!(round_up_ops(300_000.0)?, 300_000);
        assert_eq!(round_up_ops(1_000_001.0)?, 1_100_000);
        assert!(round_up_ops(f64::INFINITY).is_err());
        assert!(round_up_ops(9_223_372_036_854_775_808.0).is_err());
        Ok(())
    }
}
