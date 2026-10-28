use crate::json_io::read_json;
use crate::json_value::json_truthy;
use crate::json_value::py_string;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{collections::HashMap, fs, path::Path};

pub(crate) const SIZING_POLICY_VERSION: i64 = 3;
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
pub(crate) struct SizingConfig {
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

pub(crate) struct AuditArgs<'a> {
    pub(crate) results: &'a Path,
    pub(crate) expect_trials: i64,
    pub(crate) config: SizingConfig,
    pub(crate) json_out: Option<&'a Path>,
    pub(crate) markdown_out: Option<&'a Path>,
}

pub(crate) struct RefineArgs<'a> {
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

fn round_up_ops(value: f64) -> i64 {
    let requested = value.ceil().max(1.0) as i64;
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
    ((requested + quantum - 1) / quantum) * quantum
}

fn round_up_odd_trials(value: f64) -> i64 {
    let mut requested = (value.ceil() as i64).max(3);
    if requested.is_multiple_of(2) {
        requested += 1;
    }
    requested
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
    Ok(round_up_odd_trials(value).min(cap))
}

fn runner_ops_override(row: &Value, effective_ops: i64) -> Result<i64> {
    if row.get("workload").and_then(Value::as_str) != Some("tiny-txn") {
        return Ok(effective_ops);
    }
    let lane = row.get("lane").and_then(Value::as_str).unwrap_or("kv");
    match lane {
        "kv" => Ok(effective_ops * 10),
        "record" => Ok(effective_ops * 2),
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
        Some((values[mid - 1] + values[mid]) / 2.0)
    } else {
        Some(values[mid])
    }
}

fn cv(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    if values.len() < 2 || mean == 0.0 {
        return 0.0;
    }
    let variance = values
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f64>()
        / (values.len() - 1) as f64;
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

fn report_group(
    first: &Value,
    rows: &[Value],
    expect_trials: i64,
    config: SizingConfig,
    problems: &mut Vec<String>,
) -> Result<Value> {
    let mut trials = rows
        .iter()
        .map(|row| value_i64(row.get("trial")))
        .collect::<Result<Vec<_>>>()?;
    trials.sort_unstable();
    let expected = (1..=expect_trials).collect::<Vec<_>>();
    if trials != expected {
        problems.push(format!(
            "{}/{}/{}: trials={trials:?}, expected={expected:?}",
            field_string(first, "engine"),
            field_string(first, "durability"),
            field_string(first, "workload"),
        ));
    }

    let elapsed = rows
        .iter()
        .map(|row| value_f64(row.get("elapsed_s")))
        .collect::<Result<Vec<_>>>()?;
    let rates = rows
        .iter()
        .map(|row| value_f64(row.get("ops_per_s")))
        .collect::<Result<Vec<_>>>()?;
    let median_elapsed = median(&elapsed).context("sizing group has no elapsed values")?;
    let measured_total = elapsed.iter().sum::<f64>();
    let median_rate = median(&rates).context("sizing group has no throughput values")?;
    let throughput_cv = cv(&rates);
    let relative_spread = if median_rate == 0.0 {
        f64::INFINITY
    } else {
        (rates.iter().copied().fold(f64::NEG_INFINITY, f64::max)
            - rates.iter().copied().fold(f64::INFINITY, f64::min))
            / median_rate
    };
    if !relative_spread.is_finite() {
        bail!("sizing group has zero median throughput");
    }
    let workload = field_string(first, "workload");
    let read_only = READ_ONLY_WORKLOADS.contains(&workload.as_str());
    let undersized = if read_only {
        median_elapsed < config.read_only_min_seconds
    } else {
        measured_total < config.stateful_min_total_seconds
    };
    let variable = throughput_cv > config.max_cv || relative_spread > config.max_relative_spread;
    let status = if undersized {
        "undersized"
    } else if variable {
        "variable"
    } else {
        "accepted"
    };

    let effective_ops = value_i64(first.get("ops_requested"))?;
    let mut suggested_effective_ops = None;
    let mut runner_override = None;
    let mut suggested_trials = None;
    let mut minimum_required_trials = None;
    let mut sampling_cap_insufficient = false;
    let mut resize_strategy = None;

    if undersized && read_only {
        let scaled =
            effective_ops as f64 * config.read_only_target_seconds / median_elapsed.max(1e-9);
        let suggested = round_up_ops(scaled);
        suggested_effective_ops = Some(suggested);
        runner_override = Some(runner_ops_override(first, suggested)?);
        suggested_trials = Some(expect_trials);
        resize_strategy = Some("more-ops");
    } else if undersized {
        suggested_effective_ops = Some(effective_ops);
        runner_override = Some(runner_ops_override(first, effective_ops)?);
        let minimum =
            round_up_odd_trials(config.stateful_min_total_seconds / median_elapsed.max(1e-9));
        let suggested = round_up_trials(
            config.stateful_target_total_seconds / median_elapsed.max(1e-9),
            config.stateful_max_trials,
        )?;
        minimum_required_trials = Some(minimum);
        suggested_trials = Some(suggested);
        let effective_cap = if config.stateful_max_trials % 2 == 1 {
            config.stateful_max_trials
        } else {
            config.stateful_max_trials - 1
        };
        sampling_cap_insufficient = minimum > effective_cap;
        if sampling_cap_insufficient {
            problems.push(format!(
                "{}/{}/{workload}: stateful sampling cap={effective_cap} trials cannot meet the minimum floor; requires at least {minimum} trials",
                field_string(first, "engine"),
                field_string(first, "durability"),
            ));
        }
        resize_strategy = Some("more-trials");
    }

    let min_elapsed = elapsed.iter().copied().fold(f64::INFINITY, f64::min);
    let max_elapsed = elapsed.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    Ok(json!({
        "engine": first.get("engine").cloned().unwrap_or(Value::Null),
        "engine_version": first.get("engine_version").cloned().unwrap_or(Value::Null),
        "durability": first.get("durability").cloned().unwrap_or(Value::Null),
        "workload": workload,
        "read_materialization": first.get("read_materialization").cloned().unwrap_or_else(|| json!("legacy-read-v0")),
        "write_materialization": first.get("write_materialization").cloned().unwrap_or_else(|| json!("legacy-return-v0")),
        "records": first.get("records").cloned().unwrap_or(Value::Null),
        "ops_requested": effective_ops,
        "trials": trials,
        "median_elapsed_s": median_elapsed,
        "measured_total_s": measured_total,
        "min_elapsed_s": min_elapsed,
        "max_elapsed_s": max_elapsed,
        "median_ops_per_s": median_rate,
        "throughput_cv": throughput_cv,
        "throughput_relative_spread": relative_spread,
        "read_only": read_only,
        "status": status,
        "resize_strategy": resize_strategy,
        "suggested_effective_ops": suggested_effective_ops,
        "runner_ops_override": runner_override,
        "suggested_trials": suggested_trials,
        "minimum_required_trials": minimum_required_trials,
        "sampling_cap_insufficient": sampling_cap_insufficient,
    }))
}

pub(crate) fn audit_rows(
    rows: &[Value],
    expect_trials: i64,
    config: SizingConfig,
) -> Result<Value> {
    let mut grouped = HashMap::<String, (Vec<Value>, Vec<Value>)>::new();
    for row in rows {
        let key = group_key(row);
        let encoded = encoded_key(&key)?;
        grouped
            .entry(encoded)
            .and_modify(|(_, group)| group.push(row.clone()))
            .or_insert_with(|| (key, vec![row.clone()]));
    }
    let mut groups = grouped.into_values().collect::<Vec<_>>();
    groups.sort_by_key(|(key, _)| sort_key(key));

    let mut output_groups = Vec::new();
    let mut problems = Vec::new();
    for (_, rows) in groups {
        let first = rows.first().context("internal empty sizing group")?;
        output_groups.push(report_group(
            first,
            &rows,
            expect_trials,
            config,
            &mut problems,
        )?);
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
    format!("{:.*}%", digits, value * 100.0)
}

pub(crate) fn render_audit_markdown(report: &Value) -> Result<String> {
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
        lines.push(format!(
            "| {} | {} | {} | {} | {} | {} | {} | {:.3} | {:.3} | {} | {} | {} | {} | {} |",
            group.get("status").and_then(Value::as_str).unwrap_or(""),
            group
                .get("resize_strategy")
                .and_then(Value::as_str)
                .unwrap_or("—"),
            field_string(group, "engine"),
            field_string(group, "durability"),
            field_string(group, "workload"),
            comma_int(value_i64(group.get("ops_requested"))?),
            trials,
            value_f64(group.get("measured_total_s"))?,
            value_f64(group.get("median_elapsed_s"))?,
            percent(value_f64(group.get("throughput_cv"))?, 1),
            percent(value_f64(group.get("throughput_relative_spread"))?, 1),
            suggested.map_or_else(|| "—".to_owned(), comma_int),
            suggested_trials.map_or_else(|| "—".to_owned(), |value| value.to_string()),
            runner.map_or_else(|| "—".to_owned(), comma_int),
        ));
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

pub(crate) fn run_audit(args: AuditArgs<'_>) -> Result<i32> {
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
    Ok(if json_truthy(report.get("problems")) {
        1
    } else {
        0
    })
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

pub(crate) fn refine_plan(
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
        .cloned()
        .unwrap_or_default();
    let mut refined = Vec::new();
    let mut promoted = 0_i64;
    for original in source_groups {
        let initial_rid = group_run_id(&original, resize_prefix)?;
        let run_dir = resize_root.join(&initial_rid);
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
            promoted += 1;
        }
        refined.push(Value::Object(item));
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

pub(crate) fn render_refine_markdown(plan: &Value, resize_prefix: &str) -> Result<String> {
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
        lines.push(format!(
            "| {} | {} | {} | {} | {} | {} | {} | `{}` |",
            field_string(group, "engine"),
            field_string(group, "durability"),
            field_string(group, "workload"),
            status,
            strategy,
            comma_int(value_i64(group.get("suggested_effective_ops"))?),
            value_i64(group.get("suggested_trials"))?,
            group_run_id(group, resize_prefix)?,
        ));
    }
    Ok(lines.join("\n") + "\n")
}

pub(crate) fn run_refine(args: RefineArgs<'_>) -> Result<()> {
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
    use super::{SizingConfig, audit_rows, round_up_ops};
    use anyhow::Result;
    use serde_json::{Value, json};

    fn row(engine: &str, elapsed: f64, rate: f64, trial: i64, workload: &str) -> Value {
        json!({
            "lane":"kv","scenario":"baseline-core","engine":engine,"engine_version":"1",
            "durability":"sync","workload":workload,"records":100000,"ops_requested":50000,
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
                rows.push(row(engine, elapsed, rate, index as i64 + 1, "point-read"));
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
        assert_eq!(short["suggested_effective_ops"], 300000);
        assert_eq!(short["runner_ops_override"], 300000);
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
        assert_eq!(group["runner_ops_override"], 500000);
        assert_eq!(group["suggested_trials"], 11);
        Ok(())
    }

    #[test]
    fn operation_rounding_matches_policy() {
        assert_eq!(round_up_ops(1.0), 10);
        assert_eq!(round_up_ops(999.1), 1000);
        assert_eq!(round_up_ops(300000.0), 300000);
        assert_eq!(round_up_ops(1000001.0), 1100000);
    }
}
