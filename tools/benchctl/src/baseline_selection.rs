use crate::summary;
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use std::{collections::HashMap, fs, path::Path};

const IDENTITY_FIELDS: [&str; 21] = [
    "lane",
    "scenario",
    "engine",
    "engine_version",
    "durability",
    "workload",
    "read_materialization",
    "write_materialization",
    "records",
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
    "configuration",
];
const AUDIT_FIELDS: [&str; 8] = [
    "engine",
    "engine_version",
    "durability",
    "workload",
    "read_materialization",
    "write_materialization",
    "records",
    "ops_requested",
];
const FINAL_CONFIRMATION_KEY_FIELDS: [&str; 4] = ["engine", "durability", "workload", "records"];
const FINAL_CONFIRMATION_STRATEGIES: [&str; 5] = [
    "final-five-trial-common-work-v3",
    "final-common-work-v3",
    "final-five-trial-common-work-v4",
    "final-five-trial-common-work-v5",
    "final-five-trial-common-work-v6",
];

pub(crate) struct SelectArgs<'a> {
    pub(crate) stock_results: &'a Path,
    pub(crate) audit: &'a Path,
    pub(crate) resize_root: &'a Path,
    pub(crate) ndjson_out: &'a Path,
    pub(crate) manifest_out: &'a Path,
    pub(crate) resize_prefix: &'a str,
    pub(crate) quality_repair_plan: Option<&'a Path>,
    pub(crate) allow_missing_resize: bool,
}

pub(crate) struct FinalizeLane<'a> {
    pub(crate) stock_results: &'a Path,
    pub(crate) audit: &'a Path,
    pub(crate) resize_prefix: &'a str,
    pub(crate) out_dir: &'a Path,
    pub(crate) quality_repair_plan: Option<&'a Path>,
    pub(crate) expected_groups: i64,
}

fn py_string(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        other => other.to_string(),
    }
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

fn json_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(value)) => value.as_f64().is_some_and(|value| value != 0.0),
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Array(value)) => !value.is_empty(),
        Some(Value::Object(value)) => !value.is_empty(),
    }
}

fn identity_value(row: &Value, field: &str) -> Value {
    if let Some(value) = row.get(field) {
        return value.clone();
    }
    match field {
        "read_materialization" => json!("legacy-read-v0"),
        "write_materialization" => json!("legacy-return-v0"),
        _ => Value::Null,
    }
}

fn key_for(row: &Value, fields: &[&str]) -> Vec<Value> {
    fields
        .iter()
        .map(|field| identity_value(row, field))
        .collect()
}

fn identity(row: &Value) -> Vec<Value> {
    key_for(row, &IDENTITY_FIELDS)
}

fn audit_key(row: &Value) -> Vec<Value> {
    key_for(row, &AUDIT_FIELDS)
}

fn final_confirmation_key(row: &Value) -> Vec<Value> {
    key_for(row, &FINAL_CONFIRMATION_KEY_FIELDS)
}

fn encoded_key(key: &[Value]) -> Result<String> {
    serde_json::to_string(key).context("failed to encode baseline selection identity")
}

fn sort_key(key: &[Value]) -> Vec<String> {
    key.iter().map(py_string).collect()
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

pub(crate) fn run_id(group: &Value, resize_prefix: &str) -> Result<String> {
    Ok(format!(
        "{resize_prefix}-{}-{}-{}-e{}-t{}",
        slug(&py_string(group.get("engine").unwrap_or(&Value::Null))),
        slug(&py_string(group.get("durability").unwrap_or(&Value::Null))),
        slug(&py_string(group.get("workload").unwrap_or(&Value::Null))),
        value_i64(group.get("suggested_effective_ops"))?,
        value_i64(group.get("suggested_trials"))?,
    ))
}

fn read_ndjson(path: &Path) -> Result<Vec<Value>> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    content
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            serde_json::from_str(line)
                .with_context(|| format!("invalid JSON at {}:{}", path.display(), index + 1))
        })
        .collect()
}

fn read_json(path: &Path) -> Result<Value> {
    serde_json::from_str(
        &fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?,
    )
    .with_context(|| format!("invalid JSON in {}", path.display()))
}

fn semantic_manifest_fields(row: &Value) -> Map<String, Value> {
    let mut fields = Map::new();
    if row.get("lane").and_then(Value::as_str) == Some("record") {
        fields.insert(
            "read_materialization".to_owned(),
            identity_value(row, "read_materialization"),
        );
        fields.insert(
            "write_materialization".to_owned(),
            identity_value(row, "write_materialization"),
        );
    }
    fields
}

fn rejected_pressure_attempts(run_dir: &Path) -> Result<i64> {
    let root = run_dir.join("rejected-pressure");
    if !root.is_dir() {
        return Ok(0);
    }
    let mut count = 0_i64;
    for case in fs::read_dir(&root).with_context(|| format!("failed to read {}", root.display()))? {
        let case = case?.path();
        if !case.is_dir() {
            continue;
        }
        for attempt in
            fs::read_dir(&case).with_context(|| format!("failed to read {}", case.display()))?
        {
            let attempt = attempt?.path();
            let name = attempt
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or_default();
            if attempt.is_dir()
                && name.starts_with("attempt-")
                && attempt.join("rejection.json").is_file()
            {
                count += 1;
            }
        }
    }
    Ok(count)
}

fn validate_trials(rows: &[Value], label: &str, expected_count: i64) -> Result<()> {
    let mut trials = rows
        .iter()
        .map(|row| value_i64(row.get("trial")))
        .collect::<Result<Vec<_>>>()?;
    trials.sort_unstable();
    let expected = (1..=expected_count).collect::<Vec<_>>();
    if trials != expected {
        bail!("{label}: expected trials {expected:?}, got {trials:?}");
    }
    Ok(())
}

fn median(values: &[f64]) -> Result<f64> {
    if values.is_empty() {
        bail!("cannot compute median of no values");
    }
    let mut values = values.to_vec();
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    Ok(if values.len().is_multiple_of(2) {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    })
}

fn sample_cv(values: &[f64]) -> f64 {
    if values.len() < 2 {
        return 0.0;
    }
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    if mean == 0.0 {
        return 0.0;
    }
    let variance = values
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f64>()
        / (values.len() - 1) as f64;
    variance.sqrt() / mean
}

pub(crate) fn final_quality(rows: &[Value], thresholds: &Value) -> Result<Value> {
    let elapsed = rows
        .iter()
        .map(|row| value_f64(row.get("elapsed_s")))
        .collect::<Result<Vec<_>>>()?;
    let rates = rows
        .iter()
        .map(|row| value_f64(row.get("ops_per_s")))
        .collect::<Result<Vec<_>>>()?;
    let median_elapsed = median(&elapsed)?;
    let total_elapsed = elapsed.iter().sum::<f64>();
    let median_rate = median(&rates)?;
    let throughput_cv = sample_cv(&rates);
    let relative_spread = if median_rate == 0.0 {
        f64::INFINITY
    } else {
        (rates.iter().copied().fold(f64::NEG_INFINITY, f64::max)
            - rates.iter().copied().fold(f64::INFINITY, f64::min))
            / median_rate
    };
    if !relative_spread.is_finite() {
        bail!("final selection group has zero median throughput");
    }
    let workload = rows
        .first()
        .and_then(|row| row.get("workload"))
        .map(py_string)
        .unwrap_or_else(|| "None".to_owned());
    let read_only = thresholds
        .get("read_only_workloads")
        .and_then(Value::as_array)
        .context("missing read_only_workloads")?
        .iter()
        .any(|value| value.as_str() == Some(workload.as_str()));
    let undersized = if read_only {
        median_elapsed < value_f64(thresholds.get("read_only_min_seconds"))?
    } else {
        total_elapsed < value_f64(thresholds.get("stateful_min_total_seconds"))?
    };
    let variable = throughput_cv > value_f64(thresholds.get("max_cv"))?
        || relative_spread > value_f64(thresholds.get("max_relative_spread"))?;
    let status = if undersized {
        "undersized"
    } else if variable {
        "variable"
    } else {
        "accepted"
    };
    Ok(json!({
        "final_status": status,
        "median_elapsed_s": median_elapsed,
        "total_elapsed_s": total_elapsed,
        "throughput_cv": throughput_cv,
        "throughput_relative_spread": relative_spread,
    }))
}

struct SourceSpec<'a> {
    source: &'a str,
    resize_strategy: Option<&'a str>,
    run_id: Option<&'a str>,
    ops_requested: i64,
    trials: i64,
    rejected: Option<i64>,
}

fn source_manifest(first: &Value, spec: SourceSpec<'_>, quality: &Value) -> Result<Value> {
    let mut object = Map::new();
    object.insert(
        "engine".to_owned(),
        first.get("engine").cloned().unwrap_or(Value::Null),
    );
    object.insert(
        "durability".to_owned(),
        first.get("durability").cloned().unwrap_or(Value::Null),
    );
    object.insert(
        "workload".to_owned(),
        first.get("workload").cloned().unwrap_or(Value::Null),
    );
    object.extend(semantic_manifest_fields(first));
    object.insert("source".to_owned(), json!(spec.source));
    if let Some(strategy) = spec.resize_strategy {
        object.insert("resize_strategy".to_owned(), json!(strategy));
    }
    if let Some(run_id) = spec.run_id {
        object.insert("run_id".to_owned(), json!(run_id));
    }
    object.insert("ops_requested".to_owned(), json!(spec.ops_requested));
    object.insert("trials".to_owned(), json!(spec.trials));
    if let Some(rejected) = spec.rejected {
        object.insert("rejected_pressure_attempts".to_owned(), json!(rejected));
    }
    for key in [
        "final_status",
        "median_elapsed_s",
        "total_elapsed_s",
        "throughput_cv",
        "throughput_relative_spread",
    ] {
        object.insert(
            key.to_owned(),
            quality.get(key).cloned().context("quality field missing")?,
        );
    }
    Ok(Value::Object(object))
}

pub(crate) fn select_rows(
    stock_rows: &[Value],
    audit: &Value,
    resize_root: &Path,
    resize_prefix: &str,
    allow_missing_resize: bool,
    quality_repair_plan: Option<&Value>,
) -> Result<(Vec<Value>, Value)> {
    let policy_version = value_i64(audit.get("sizing_policy_version"))?;
    if !matches!(policy_version, 2 | 3) {
        bail!(
            "unsupported sizing policy: {:?}",
            audit.get("sizing_policy_version").unwrap_or(&Value::Null)
        );
    }
    if policy_version >= 3
        && stock_rows.iter().any(|row| {
            row.get("lane").and_then(Value::as_str) == Some("record")
                && (row
                    .get("read_materialization")
                    .and_then(Value::as_str)
                    .is_none()
                    || row
                        .get("write_materialization")
                        .and_then(Value::as_str)
                        .is_none())
        })
    {
        bail!("sizing policy v3 record rows require explicit read/write materialization");
    }
    let thresholds = audit
        .get("thresholds")
        .context("sizing audit thresholds missing")?;
    let stock_trial_count = thresholds
        .get("expect_trials")
        .map_or(Ok(3), |value| value_i64(Some(value)))?;

    let mut stock_groups: HashMap<String, (Vec<Value>, Vec<Value>)> = HashMap::new();
    for row in stock_rows {
        let key = identity(row);
        let encoded = encoded_key(&key)?;
        stock_groups
            .entry(encoded)
            .or_insert_with(|| (key, Vec::new()))
            .1
            .push(row.clone());
    }
    let mut ordered_stock = stock_groups.into_values().collect::<Vec<_>>();
    ordered_stock.sort_by_key(|(key, _)| sort_key(key));

    let mut audit_groups = HashMap::new();
    for group in audit
        .get("groups")
        .and_then(Value::as_array)
        .context("sizing audit groups missing")?
    {
        audit_groups.insert(encoded_key(&audit_key(group))?, group.clone());
    }

    let final_confirmation = quality_repair_plan
        .and_then(|plan| plan.get("strategy"))
        .and_then(Value::as_str)
        .is_some_and(|strategy| FINAL_CONFIRMATION_STRATEGIES.contains(&strategy));
    let mut quality_groups = HashMap::new();
    if let Some(plan) = quality_repair_plan {
        if value_i64(plan.get("quality_policy_version"))? != 1 {
            bail!(
                "unsupported stock quality policy: {:?}",
                plan.get("quality_policy_version").unwrap_or(&Value::Null)
            );
        }
        for group in plan
            .get("groups")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            let key = if final_confirmation {
                final_confirmation_key(&group)
            } else {
                audit_key(&group)
            };
            let encoded = encoded_key(&key)?;
            if quality_groups.insert(encoded.clone(), group).is_some() {
                bail!("duplicate quality-repair identity: {key:?}");
            }
        }
    }

    let mut output = Vec::new();
    let mut selected_stock = 0_i64;
    let mut selected_resize = 0_i64;
    let mut selected_quality_repair = 0_i64;
    let mut pending = Vec::<String>::new();
    let mut sources = Vec::<Value>::new();

    for (group_identity, rows) in ordered_stock {
        let first = rows.first().context("empty stock group")?;
        validate_trials(
            &rows,
            &format!(
                "stock {}/{}/{}",
                py_string(first.get("engine").unwrap_or(&Value::Null)),
                py_string(first.get("durability").unwrap_or(&Value::Null)),
                py_string(first.get("workload").unwrap_or(&Value::Null))
            ),
            stock_trial_count,
        )?;
        let key = audit_key(first);
        let group = audit_groups
            .get(&encoded_key(&key)?)
            .with_context(|| format!("stock group missing from audit: {key:?}"))?;

        let quality_key = if final_confirmation {
            final_confirmation_key(first)
        } else {
            key.clone()
        };
        if let Some(quality_group) = quality_groups.get(&encoded_key(&quality_key)?) {
            let rid = run_id(quality_group, resize_prefix)?;
            let run_dir = resize_root.join(&rid);
            let results_path = run_dir.join("results.ndjson");
            let summary_path = run_dir.join("summary.json");
            if !results_path.is_file() || !summary_path.is_file() {
                pending.push(rid.clone());
                if allow_missing_resize {
                    continue;
                }
                bail!("missing quality-repair result: {rid}");
            }
            let expected_trials = value_i64(quality_group.get("suggested_trials"))?;
            let run_summary = read_json(&summary_path)?;
            if value_i64(run_summary.get("row_count"))? != expected_trials
                || value_i64(run_summary.get("group_count"))? != 1
                || json_truthy(run_summary.get("problems"))
            {
                bail!("invalid quality-repair summary: {rid}");
            }
            let repaired = read_ndjson(&results_path)?;
            validate_trials(&repaired, &format!("quality repair {rid}"), expected_trials)?;
            let repaired_ids = repaired.iter().map(identity).collect::<Vec<_>>();
            if repaired_ids.is_empty() || repaired_ids.iter().any(|id| id != &group_identity) {
                bail!("quality-repair identity differs from stock group: {rid}");
            }
            let expected_ops = value_i64(quality_group.get("suggested_effective_ops"))?;
            let mut actual_ops = repaired
                .iter()
                .map(|row| value_i64(row.get("ops_requested")))
                .collect::<Result<Vec<_>>>()?;
            actual_ops.sort_unstable();
            actual_ops.dedup();
            if actual_ops != [expected_ops] {
                bail!(
                    "quality-repair op count mismatch for {rid}: expected {expected_ops}, got {actual_ops:?}"
                );
            }
            let quality = final_quality(&repaired, thresholds)?;
            if quality.get("final_status").and_then(Value::as_str) == Some("undersized") {
                bail!("quality-repair result remains undersized: {rid}");
            }
            output.extend(repaired);
            selected_quality_repair += 1;
            sources.push(source_manifest(
                first,
                SourceSpec {
                    source: "quality-repair",
                    resize_strategy: Some("quality-repair"),
                    run_id: Some(&rid),
                    ops_requested: expected_ops,
                    trials: expected_trials,
                    rejected: Some(rejected_pressure_attempts(&run_dir)?),
                },
                &quality,
            )?);
            continue;
        }

        if group.get("status").and_then(Value::as_str) != Some("undersized") {
            let quality = final_quality(&rows, thresholds)?;
            let final_status = quality.get("final_status").and_then(Value::as_str);
            let audit_status = group.get("status").and_then(Value::as_str);
            if final_status != audit_status {
                bail!(
                    "stock quality no longer matches audit for {key:?}: audit={} recomputed={}",
                    audit_status.unwrap_or("None"),
                    final_status.unwrap_or("None")
                );
            }
            output.extend(rows.clone());
            selected_stock += 1;
            sources.push(source_manifest(
                first,
                SourceSpec {
                    source: "stock",
                    resize_strategy: None,
                    run_id: None,
                    ops_requested: value_i64(first.get("ops_requested"))?,
                    trials: stock_trial_count,
                    rejected: None,
                },
                &quality,
            )?);
            continue;
        }

        let rid = run_id(group, resize_prefix)?;
        let run_dir = resize_root.join(&rid);
        let results_path = run_dir.join("results.ndjson");
        let summary_path = run_dir.join("summary.json");
        if !results_path.is_file() || !summary_path.is_file() {
            pending.push(rid.clone());
            if allow_missing_resize {
                continue;
            }
            bail!("missing resized result: {rid}");
        }
        let expected_trials = value_i64(group.get("suggested_trials"))?;
        let run_summary = read_json(&summary_path)?;
        if value_i64(run_summary.get("row_count"))? != expected_trials
            || value_i64(run_summary.get("group_count"))? != 1
            || json_truthy(run_summary.get("problems"))
        {
            bail!("invalid resized summary: {rid}");
        }
        let resized = read_ndjson(&results_path)?;
        validate_trials(&resized, &format!("resize {rid}"), expected_trials)?;
        let resized_ids = resized.iter().map(identity).collect::<Vec<_>>();
        if resized_ids.is_empty() || resized_ids.iter().any(|id| id != &group_identity) {
            bail!("resized identity differs from stock group: {rid}");
        }
        let expected_ops = value_i64(group.get("suggested_effective_ops"))?;
        let mut actual_ops = resized
            .iter()
            .map(|row| value_i64(row.get("ops_requested")))
            .collect::<Result<Vec<_>>>()?;
        actual_ops.sort_unstable();
        actual_ops.dedup();
        if actual_ops != [expected_ops] {
            bail!(
                "resized op count mismatch for {rid}: expected {expected_ops}, got {actual_ops:?}"
            );
        }
        let quality = final_quality(&resized, thresholds)?;
        if quality.get("final_status").and_then(Value::as_str) == Some("undersized") {
            bail!("resized result remains undersized: {rid}");
        }
        output.extend(resized);
        selected_resize += 1;
        sources.push(source_manifest(
            first,
            SourceSpec {
                source: "resize",
                resize_strategy: group.get("resize_strategy").and_then(Value::as_str),
                run_id: Some(&rid),
                ops_requested: expected_ops,
                trials: expected_trials,
                rejected: Some(rejected_pressure_attempts(&run_dir)?),
            },
            &quality,
        )?);
    }

    let expected_audit_groups = audit
        .get("group_count")
        .map_or(audit_groups.len() as i64, |value| {
            value_i64(Some(value)).unwrap_or(-1)
        });
    let stock_group_count = ordered_stock_len(stock_rows)?;
    if stock_group_count != expected_audit_groups {
        bail!(
            "stock/audit group count mismatch: stock={stock_group_count} audit={expected_audit_groups}"
        );
    }
    let status_count = |status: &str| -> i64 {
        sources
            .iter()
            .filter(|source| source.get("final_status").and_then(Value::as_str) == Some(status))
            .count() as i64
    };
    let rejected_total = sources
        .iter()
        .map(|source| {
            source
                .get("rejected_pressure_attempts")
                .and_then(Value::as_i64)
                .unwrap_or(0)
        })
        .sum::<i64>();
    let manifest = json!({
        "sizing_policy_version": audit.get("sizing_policy_version").cloned().unwrap_or(Value::Null),
        "stock_rows": stock_rows.len(),
        "stock_groups": stock_group_count,
        "selected_rows": output.len(),
        "selected_groups": selected_stock + selected_resize + selected_quality_repair,
        "selected_stock_groups": selected_stock,
        "selected_resize_groups": selected_resize,
        "selected_quality_repair_groups": selected_quality_repair,
        "pending_resize_groups": pending.len(),
        "pending_run_ids": pending,
        "complete": pending.is_empty(),
        "final_status_counts": {
            "accepted": status_count("accepted"),
            "variable": status_count("variable"),
            "undersized": status_count("undersized"),
        },
        "selected_followup_rejected_pressure_attempts": rejected_total,
        "sources": sources,
    });
    Ok((output, manifest))
}

fn ordered_stock_len(stock_rows: &[Value]) -> Result<i64> {
    let mut keys = std::collections::HashSet::new();
    for row in stock_rows {
        keys.insert(encoded_key(&identity(row))?);
    }
    Ok(keys.len() as i64)
}

fn write_selection(
    rows: &[Value],
    manifest: &Value,
    ndjson_out: &Path,
    manifest_out: &Path,
) -> Result<()> {
    if let Some(parent) = ndjson_out.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut ndjson = String::new();
    for row in rows {
        ndjson.push_str(&serde_json::to_string(row)?);
        ndjson.push('\n');
    }
    fs::write(ndjson_out, ndjson)
        .with_context(|| format!("failed to write {}", ndjson_out.display()))?;
    if let Some(parent) = manifest_out.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(manifest_out, serde_json::to_string_pretty(manifest)? + "\n")
        .with_context(|| format!("failed to write {}", manifest_out.display()))?;
    Ok(())
}

pub(crate) fn run_select(args: SelectArgs<'_>) -> Result<()> {
    let stock_rows = read_ndjson(args.stock_results)?;
    let audit = read_json(args.audit)?;
    let quality = args.quality_repair_plan.map(read_json).transpose()?;
    let (rows, mut manifest) = select_rows(
        &stock_rows,
        &audit,
        args.resize_root,
        args.resize_prefix,
        args.allow_missing_resize,
        quality.as_ref(),
    )?;
    manifest
        .as_object_mut()
        .context("selection manifest is not an object")?
        .insert(
            "stock_rejected_pressure_attempts".to_owned(),
            json!(rejected_pressure_attempts(
                args.stock_results
                    .parent()
                    .context("stock results have no parent")?
            )?),
        );
    write_selection(&rows, &manifest, args.ndjson_out, args.manifest_out)?;
    println!(
        "{{\"complete\": {}, \"pending_resize_groups\": {}, \"selected_groups\": {}, \"selected_quality_repair_groups\": {}, \"selected_resize_groups\": {}, \"selected_rows\": {}, \"selected_stock_groups\": {}}}",
        manifest
            .get("complete")
            .context("manifest complete missing")?,
        manifest
            .get("pending_resize_groups")
            .context("manifest pending count missing")?,
        manifest
            .get("selected_groups")
            .context("manifest group count missing")?,
        manifest
            .get("selected_quality_repair_groups")
            .context("manifest quality count missing")?,
        manifest
            .get("selected_resize_groups")
            .context("manifest resize count missing")?,
        manifest
            .get("selected_rows")
            .context("manifest row count missing")?,
        manifest
            .get("selected_stock_groups")
            .context("manifest stock count missing")?,
    );
    Ok(())
}

pub(crate) fn finalize_one(repo: &Path, lane: FinalizeLane<'_>) -> Result<()> {
    fs::create_dir_all(lane.out_dir)
        .with_context(|| format!("failed to create {}", lane.out_dir.display()))?;
    let ndjson = lane.out_dir.join("results.ndjson");
    let manifest_path = lane.out_dir.join("manifest.json");
    let summary_path = lane.out_dir.join("summary.json");
    let summary_md = lane.out_dir.join("summary.md");
    let stock_rows = read_ndjson(lane.stock_results)?;
    let audit = read_json(lane.audit)?;
    let quality = lane.quality_repair_plan.map(read_json).transpose()?;
    let resize_root = repo.join("results/runs");
    let (rows, mut manifest) = select_rows(
        &stock_rows,
        &audit,
        &resize_root,
        lane.resize_prefix,
        false,
        quality.as_ref(),
    )?;
    manifest
        .as_object_mut()
        .context("selection manifest is not an object")?
        .insert(
            "stock_rejected_pressure_attempts".to_owned(),
            json!(rejected_pressure_attempts(
                lane.stock_results
                    .parent()
                    .context("stock results have no parent")?
            )?),
        );
    write_selection(&rows, &manifest, &ndjson, &manifest_path)?;
    summary::summarize_file(&ndjson, Some(&summary_path), Some(&summary_md), None)?;
    let summary = read_json(&summary_path)?;
    if !manifest
        .get("complete")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        bail!("selection remained incomplete: {}", lane.out_dir.display());
    }
    let selected_rows = value_i64(manifest.get("selected_rows"))?;
    if value_i64(manifest.get("selected_groups"))? != lane.expected_groups {
        bail!("selection group-count mismatch: {}", lane.out_dir.display());
    }
    if value_i64(summary.get("row_count"))? != selected_rows
        || value_i64(summary.get("group_count"))? != lane.expected_groups
        || json_truthy(summary.get("problems"))
    {
        bail!("summary validation failed: {}", lane.out_dir.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{final_quality, run_id, select_rows};
    use anyhow::Result;
    use serde_json::{Value, json};
    use std::fs;

    fn row(engine: &str, workload: &str, trial: i64, ops: i64, elapsed: f64, rate: f64) -> Value {
        json!({
            "lane":"kv","scenario":"baseline-core","engine":engine,"engine_version":"1.0",
            "durability":"relaxed","workload":workload,"records":100,"ops_requested":ops,
            "clients":1,"value_bytes":256,"value_pattern":"pseudo-random","key_bytes":8,
            "key_shape":"sequential","access_pattern":"auto","miss_percent":0,"write_pattern":"append",
            "txn_size":100,"scan_len":100,"settle_ms":0,"configuration":"fixed","payload_pattern":"hex-random",
            "trial":trial,"elapsed_s":elapsed,"ops_per_s":rate
        })
    }

    fn audit_group(
        engine: &str,
        workload: &str,
        ops: i64,
        status: &str,
        suggested: Option<i64>,
        trials: Option<i64>,
        strategy: Option<&str>,
    ) -> Value {
        json!({
            "engine":engine,"engine_version":"1.0","durability":"relaxed","workload":workload,
            "records":100,"ops_requested":ops,"status":status,"resize_strategy":strategy,
            "suggested_effective_ops":suggested,"suggested_trials":trials
        })
    }

    fn audit(groups: Vec<Value>) -> Value {
        json!({
            "sizing_policy_version":2,"group_count":groups.len(),
            "thresholds":{"expect_trials":3,"max_cv":0.10,"max_relative_spread":0.25,
                "read_only_min_seconds":0.5,"read_only_workloads":["indexed-read","point-read","range-scan"],
                "stateful_min_total_seconds":1.0},"groups":groups
        })
    }

    fn temp_root(name: &str) -> Result<std::path::PathBuf> {
        let root = std::env::temp_dir().join(format!(
            "all-db-bench-selection-{name}-{}",
            std::process::id()
        ));
        if root.exists() {
            fs::remove_dir_all(&root)?;
        }
        fs::create_dir_all(&root)?;
        Ok(root)
    }

    #[test]
    fn run_id_accepts_record_prefix_and_trial_count() -> Result<()> {
        let target = audit_group(
            "sqlite",
            "tiny-txn",
            5_000,
            "undersized",
            Some(5_000),
            Some(11),
            Some("more-trials"),
        );
        assert_eq!(
            run_id(&target, "20261006-record-resize-v4")?,
            "20261006-record-resize-v4-sqlite-relaxed-tiny-txn-e5000-t11"
        );
        Ok(())
    }

    #[test]
    fn final_quality_matches_sampling_policy() -> Result<()> {
        let thresholds = json!({"max_cv":0.10,"max_relative_spread":0.25,"read_only_min_seconds":0.5,
            "read_only_workloads":["indexed-read","point-read","range-scan"],"stateful_min_total_seconds":1.0});
        let rows = (1..=5)
            .zip([100.0, 100.0, 100.0, 100.0, 40.0])
            .map(|(trial, rate)| row("x", "tiny-txn", trial, 100, 0.2, rate))
            .collect::<Vec<_>>();
        assert_eq!(
            final_quality(&rows, &thresholds)?["final_status"],
            "variable"
        );
        let read = (1..=3)
            .map(|trial| row("x", "point-read", trial, 100, 0.2, 100.0))
            .collect::<Vec<_>>();
        assert_eq!(
            final_quality(&read, &thresholds)?["final_status"],
            "undersized"
        );
        Ok(())
    }

    #[test]
    fn replaces_stateful_group_with_more_trials_same_ops() -> Result<()> {
        let root = temp_root("resize")?;
        let stock = (1..=3)
            .map(|t| row("a", "point-read", t, 100, 1.0, 100.0))
            .chain((1..=3).map(|t| row("b", "tiny-txn", t, 100, 1.0, 100.0)))
            .collect::<Vec<_>>();
        let target = audit_group(
            "b",
            "tiny-txn",
            100,
            "undersized",
            Some(100),
            Some(5),
            Some("more-trials"),
        );
        let plan = audit(vec![
            audit_group("a", "point-read", 100, "accepted", None, None, None),
            target.clone(),
        ]);
        let run = root.join(run_id(&target, "resize")?);
        fs::create_dir_all(&run)?;
        let resized = (1..=5)
            .map(|t| row("b", "tiny-txn", t, 100, 0.25, 100.0))
            .collect::<Vec<_>>();
        fs::write(
            run.join("results.ndjson"),
            resized
                .iter()
                .map(|v| serde_json::to_string(v).map(|s| s + "\n"))
                .collect::<Result<String, _>>()?,
        )?;
        fs::write(
            run.join("summary.json"),
            serde_json::to_string(&json!({"row_count":5,"group_count":1,"problems":[]}))?,
        )?;
        let (selected, manifest) = select_rows(&stock, &plan, &root, "resize", false, None)?;
        assert_eq!(selected.len(), 8);
        assert_eq!(manifest["selected_resize_groups"], 1);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn final_confirmation_can_override_ops() -> Result<()> {
        let root = temp_root("confirmation")?;
        let stock = (1..=3)
            .map(|t| row("sqlite", "point-read", t, 100, 0.2, 500.0))
            .collect::<Vec<_>>();
        let target = audit_group(
            "sqlite",
            "point-read",
            100,
            "undersized",
            Some(300),
            Some(3),
            Some("more-ops"),
        );
        let plan = audit(vec![target.clone()]);
        let mut confirmation = target;
        confirmation["suggested_effective_ops"] = json!(200);
        confirmation["suggested_trials"] = json!(5);
        confirmation["resize_strategy"] = json!("quality-repair");
        let quality = json!({"quality_policy_version":1,"strategy":"final-five-trial-common-work-v3","groups":[confirmation.clone()]});
        let run = root.join(run_id(&confirmation, "final")?);
        fs::create_dir_all(&run)?;
        let repaired = (1..=5)
            .map(|t| row("sqlite", "point-read", t, 200, 1.0, 200.0 + t as f64))
            .collect::<Vec<_>>();
        fs::write(
            run.join("results.ndjson"),
            repaired
                .iter()
                .map(|v| serde_json::to_string(v).map(|s| s + "\n"))
                .collect::<Result<String, _>>()?,
        )?;
        fs::write(
            run.join("summary.json"),
            serde_json::to_string(&json!({"row_count":5,"group_count":1,"problems":[]}))?,
        )?;
        let (selected, manifest) =
            select_rows(&stock, &plan, &root, "final", false, Some(&quality))?;
        assert_eq!(selected.len(), 5);
        assert_eq!(manifest["selected_quality_repair_groups"], 1);
        fs::remove_dir_all(root)?;
        Ok(())
    }
}
