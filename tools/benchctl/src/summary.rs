use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct GroupKey {
    format_version: u64,
    lane: String,
    scenario: String,
    engine: String,
    engine_version: String,
    durability: String,
    workload: String,
    records: u64,
    ops_requested: u64,
    clients: u64,
    value_bytes: Option<u64>,
    value_pattern: String,
    payload_pattern: String,
    engine_compression: String,
    key_bytes: u64,
    key_shape: String,
    access_pattern: String,
    miss_percent: u64,
    write_pattern: String,
    state_evolution: String,
    bounded_churn_slots: u64,
    txn_size: u64,
    read_materialization: String,
    write_materialization: String,
    scan_len: Option<u64>,
    settle_ms: u64,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ConcurrencyKey {
    lane: String,
    format_version: u64,
    scenario: String,
    engine: String,
    engine_version: String,
    durability: String,
    workload: String,
    records: u64,
    ops_requested: u64,
    value_bytes: Option<u64>,
    value_pattern: String,
    payload_pattern: String,
    engine_compression: String,
    key_bytes: u64,
    key_shape: String,
    access_pattern: String,
    miss_percent: u64,
    write_pattern: String,
    state_evolution: String,
    bounded_churn_slots: u64,
    txn_size: u64,
    read_materialization: String,
    write_materialization: String,
    scan_len: Option<u64>,
    settle_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
struct SummaryGroup {
    format_version: u64,
    lane: String,
    scenario: String,
    engine: String,
    engine_version: String,
    durability: String,
    workload: String,
    records: u64,
    ops_requested: u64,
    clients: u64,
    value_bytes: Option<u64>,
    value_pattern: String,
    payload_pattern: String,
    engine_compression: String,
    engine_compression_mapping: Vec<String>,
    key_bytes: u64,
    key_shape: String,
    access_pattern: String,
    miss_percent: u64,
    write_pattern: String,
    state_evolution: String,
    bounded_churn_slots: u64,
    txn_size: u64,
    read_materialization: String,
    write_materialization: String,
    scan_len: Option<u64>,
    settle_ms: u64,
    configuration: String,
    trials: Vec<u64>,
    ops_per_s_median: f64,
    ops_per_s_q1: f64,
    ops_per_s_q3: f64,
    ops_per_s_min: f64,
    ops_per_s_max: f64,
    p99_read_us_median: Option<f64>,
    p99_operation_us_median: Option<f64>,
    p99_write_txn_us_median: Option<f64>,
    db_bytes_median: f64,
    peak_rss_kib_median: Option<f64>,
    open_s_median: Option<f64>,
    prefill_s_median: Option<f64>,
    client_setup_s_median: Option<f64>,
    cpu_ns_per_op_median: Option<f64>,
    cpu_cores_median: Option<f64>,
    client_throughput_max_min_ratio_median: Option<f64>,
    write_conflict_retries_median: Option<f64>,
    write_conflict_retries_per_k_write_ops_median: Option<f64>,
    runqueue_wait_fraction_median: Option<f64>,
    read_bytes_per_op_median: Option<f64>,
    write_bytes_per_op_median: Option<f64>,
    major_faults_per_kop_median: Option<f64>,
    cpu_psi_some_fraction_median: Option<f64>,
    io_psi_full_fraction_median: Option<f64>,
    swap_activity_pages_median: Option<f64>,
    settle_cpu_ns_median: Option<f64>,
    settle_write_bytes_median: Option<f64>,
    settle_db_delta_bytes_median: Option<f64>,
    durability_mapping: Vec<String>,
    speedup_vs_c1: Option<f64>,
    parallel_efficiency_vs_c1: Option<f64>,
    p99_read_multiplier_vs_c1: Option<f64>,
    p99_write_txn_multiplier_vs_c1: Option<f64>,
}

fn required<'a>(row: &'a Value, key: &str) -> Result<&'a Value> {
    row.get(key)
        .with_context(|| format!("missing required field {key}"))
}

fn required_str(row: &Value, key: &str) -> Result<String> {
    required(row, key)?
        .as_str()
        .map(ToOwned::to_owned)
        .with_context(|| format!("field {key} is not a string"))
}

fn required_u64(row: &Value, key: &str) -> Result<u64> {
    required(row, key)?
        .as_u64()
        .with_context(|| format!("field {key} is not an unsigned integer"))
}

fn required_f64(row: &Value, key: &str) -> Result<f64> {
    required(row, key)?
        .as_f64()
        .filter(|value| value.is_finite())
        .with_context(|| format!("field {key} is not a finite number"))
}

fn optional_str(row: &Value, key: &str, default: &str) -> String {
    row.get(key)
        .and_then(Value::as_str)
        .unwrap_or(default)
        .to_owned()
}

fn optional_u64(row: &Value, key: &str, default: u64) -> u64 {
    row.get(key).and_then(Value::as_u64).unwrap_or(default)
}

fn nested<'a>(row: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(row, |value, key| value.get(*key))
}

fn nested_u64(row: &Value, path: &[&str]) -> Option<u64> {
    nested(row, path).and_then(Value::as_u64)
}

fn nested_f64(row: &Value, path: &[&str]) -> Option<f64> {
    nested(row, path)
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
}

fn value_bytes(row: &Value) -> Option<u64> {
    row.get("value_bytes")
        .and_then(Value::as_u64)
        .or_else(|| row.get("payload_bytes").and_then(Value::as_u64))
}

fn group_key(row: &Value) -> Result<GroupKey> {
    Ok(GroupKey {
        format_version: optional_u64(row, "format_version", 1),
        lane: optional_str(row, "lane", "kv"),
        scenario: optional_str(row, "scenario", "legacy"),
        engine: required_str(row, "engine")?,
        engine_version: required_str(row, "engine_version")?,
        durability: required_str(row, "durability")?,
        workload: required_str(row, "workload")?,
        records: required_u64(row, "records")?,
        ops_requested: required_u64(row, "ops_requested")?,
        clients: optional_u64(row, "clients", 1),
        value_bytes: value_bytes(row),
        value_pattern: optional_str(row, "value_pattern", "pseudo-random"),
        payload_pattern: optional_str(row, "payload_pattern", "hex-random"),
        engine_compression: optional_str(row, "engine_compression", "default"),
        key_bytes: optional_u64(row, "key_bytes", 8),
        key_shape: optional_str(row, "key_shape", "sequential"),
        access_pattern: optional_str(row, "access_pattern", "auto"),
        miss_percent: optional_u64(row, "miss_percent", 0),
        write_pattern: optional_str(row, "write_pattern", "append"),
        state_evolution: optional_str(row, "state_evolution", "growth"),
        bounded_churn_slots: optional_u64(row, "bounded_churn_slots", 0),
        txn_size: required_u64(row, "txn_size")?,
        read_materialization: optional_str(row, "read_materialization", "legacy-read-v0"),
        write_materialization: optional_str(row, "write_materialization", "legacy-return-v0"),
        scan_len: row.get("scan_len").and_then(Value::as_u64),
        settle_ms: nested_u64(row, &["post_workload_settle", "requested_ms"]).unwrap_or(0),
    })
}

fn concurrency_key(group: &SummaryGroup) -> ConcurrencyKey {
    ConcurrencyKey {
        lane: group.lane.clone(),
        format_version: group.format_version,
        scenario: group.scenario.clone(),
        engine: group.engine.clone(),
        engine_version: group.engine_version.clone(),
        durability: group.durability.clone(),
        workload: group.workload.clone(),
        records: group.records,
        ops_requested: group.ops_requested,
        value_bytes: group.value_bytes,
        value_pattern: group.value_pattern.clone(),
        payload_pattern: group.payload_pattern.clone(),
        engine_compression: group.engine_compression.clone(),
        key_bytes: group.key_bytes,
        key_shape: group.key_shape.clone(),
        access_pattern: group.access_pattern.clone(),
        miss_percent: group.miss_percent,
        write_pattern: group.write_pattern.clone(),
        state_evolution: group.state_evolution.clone(),
        bounded_churn_slots: group.bounded_churn_slots,
        txn_size: group.txn_size,
        read_materialization: group.read_materialization.clone(),
        write_materialization: group.write_materialization.clone(),
        scan_len: group.scan_len,
        settle_ms: group.settle_ms,
    }
}

fn median(mut values: Vec<f64>) -> Option<f64> {
    values.retain(|value| value.is_finite());
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        Some((values[middle - 1] + values[middle]) / 2.0)
    } else {
        Some(values[middle])
    }
}

fn required_median(values: Vec<f64>, label: &str) -> Result<f64> {
    median(values).with_context(|| format!("no finite values for {label}"))
}

fn quantile(mut values: Vec<f64>, q: f64) -> Result<f64> {
    values.retain(|value| value.is_finite());
    if values.is_empty() {
        bail!("cannot compute quantile of empty values");
    }
    values.sort_by(f64::total_cmp);
    if values.len() == 1 {
        return Ok(values[0]);
    }
    let position = (values.len() - 1) as f64 * q;
    let low = position.floor() as usize;
    let high = position.ceil() as usize;
    if low == high {
        Ok(values[low])
    } else {
        Ok(values[low] * (high as f64 - position) + values[high] * (position - low as f64))
    }
}

fn latency_p99(row: &Value, field: &str) -> Option<f64> {
    if nested_u64(row, &[field, "count"]).unwrap_or(0) == 0 {
        return None;
    }
    nested_f64(row, &[field, "p99_us"])
}

fn write_latency_p99(row: &Value) -> Option<f64> {
    for field in ["write_txn_latency", "transaction_latency"] {
        if let Some(value) = latency_p99(row, field) {
            return Some(value);
        }
    }
    None
}

fn optional_metric(
    rows: &[&Value],
    present: impl Fn(&Value) -> bool,
    value: impl Fn(&Value) -> f64,
) -> Option<f64> {
    if rows.iter().any(|row| present(row)) {
        median(rows.iter().map(|row| value(row)).collect())
    } else {
        None
    }
}

fn filtered_metric(
    rows: &[&Value],
    include: impl Fn(&Value) -> bool,
    value: impl Fn(&Value) -> f64,
) -> Option<f64> {
    median(
        rows.iter()
            .filter(|row| include(row))
            .map(|row| value(row))
            .collect(),
    )
}

fn nonempty_object(row: &Value, key: &str) -> bool {
    row.get(key)
        .and_then(Value::as_object)
        .is_some_and(|object| !object.is_empty())
}

fn configuration(key: &GroupKey) -> String {
    if key.lane == "kv-concurrency" {
        format!(
            "clients={} k={}/{} v={}/{} compression={} access={} miss={}% write={} state={}/slots={}",
            key.clients,
            key.key_bytes,
            key.key_shape,
            key.value_bytes
                .map_or_else(|| "None".to_owned(), |value| value.to_string()),
            key.value_pattern,
            key.engine_compression,
            key.access_pattern,
            key.miss_percent,
            key.write_pattern,
            key.state_evolution,
            key.bounded_churn_slots
        )
    } else if key.lane == "record-concurrency" {
        format!(
            "clients={} payload={}/{} txn={}",
            key.clients,
            key.value_bytes
                .map_or_else(|| "None".to_owned(), |value| value.to_string()),
            key.payload_pattern,
            key.txn_size
        )
    } else if key.lane == "kv" {
        format!(
            "k={}/{} v={}/{} access={} miss={}% write={} settle={}ms",
            key.key_bytes,
            key.key_shape,
            key.value_bytes
                .map_or_else(|| "None".to_owned(), |value| value.to_string()),
            key.value_pattern,
            key.access_pattern,
            key.miss_percent,
            key.write_pattern,
            key.settle_ms
        )
    } else {
        format!(
            "payload={}/{} txn={}",
            key.value_bytes
                .map_or_else(|| "None".to_owned(), |value| value.to_string()),
            key.payload_pattern,
            key.txn_size
        )
    }
}

fn summarize_group(key: &GroupKey, rows: &[&Value]) -> Result<SummaryGroup> {
    let rates = rows
        .iter()
        .map(|row| required_f64(row, "ops_per_s"))
        .collect::<Result<Vec<_>>>()?;
    let trials = rows
        .iter()
        .map(|row| required_u64(row, "trial"))
        .collect::<Result<BTreeSet<_>>>()?
        .into_iter()
        .collect::<Vec<_>>();
    let completed = |row: &Value| required_u64(row, "ops_completed").unwrap_or(1).max(1) as f64;

    let engine_compression_mapping = rows
        .iter()
        .map(|row| optional_str(row, "engine_compression_mapping", "legacy/default"))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let durability_mapping = rows
        .iter()
        .map(|row| required_str(row, "durability_mapping"))
        .collect::<Result<BTreeSet<_>>>()?
        .into_iter()
        .collect();

    Ok(SummaryGroup {
        format_version: key.format_version,
        lane: key.lane.clone(),
        scenario: key.scenario.clone(),
        engine: key.engine.clone(),
        engine_version: key.engine_version.clone(),
        durability: key.durability.clone(),
        workload: key.workload.clone(),
        records: key.records,
        ops_requested: key.ops_requested,
        clients: key.clients,
        value_bytes: key.value_bytes,
        value_pattern: key.value_pattern.clone(),
        payload_pattern: key.payload_pattern.clone(),
        engine_compression: key.engine_compression.clone(),
        engine_compression_mapping,
        key_bytes: key.key_bytes,
        key_shape: key.key_shape.clone(),
        access_pattern: key.access_pattern.clone(),
        miss_percent: key.miss_percent,
        write_pattern: key.write_pattern.clone(),
        state_evolution: key.state_evolution.clone(),
        bounded_churn_slots: key.bounded_churn_slots,
        txn_size: key.txn_size,
        read_materialization: key.read_materialization.clone(),
        write_materialization: key.write_materialization.clone(),
        scan_len: key.scan_len,
        settle_ms: key.settle_ms,
        configuration: configuration(key),
        trials,
        ops_per_s_median: required_median(rates.clone(), "ops_per_s")?,
        ops_per_s_q1: quantile(rates.clone(), 0.25)?,
        ops_per_s_q3: quantile(rates.clone(), 0.75)?,
        ops_per_s_min: rates
            .iter()
            .copied()
            .reduce(f64::min)
            .context("missing ops_per_s")?,
        ops_per_s_max: rates
            .iter()
            .copied()
            .reduce(f64::max)
            .context("missing ops_per_s")?,
        p99_read_us_median: median(
            rows.iter()
                .filter_map(|row| latency_p99(row, "read_latency"))
                .collect(),
        ),
        p99_operation_us_median: median(
            rows.iter()
                .filter_map(|row| latency_p99(row, "operation_latency"))
                .collect(),
        ),
        p99_write_txn_us_median: median(
            rows.iter()
                .filter_map(|row| write_latency_p99(row))
                .collect(),
        ),
        db_bytes_median: required_median(
            rows.iter()
                .map(|row| required_u64(row, "db_bytes").map(|value| value as f64))
                .collect::<Result<Vec<_>>>()?,
            "db_bytes",
        )?,
        peak_rss_kib_median: filtered_metric(
            rows,
            |row| row.get("peak_rss_kib").is_some(),
            |row| row.get("peak_rss_kib").and_then(Value::as_u64).unwrap_or(0) as f64,
        ),
        open_s_median: filtered_metric(
            rows,
            |row| row.get("open_s").is_some(),
            |row| row.get("open_s").and_then(Value::as_f64).unwrap_or(0.0),
        ),
        prefill_s_median: filtered_metric(
            rows,
            |row| row.get("prefill_s").is_some(),
            |row| row.get("prefill_s").and_then(Value::as_f64).unwrap_or(0.0),
        ),
        client_setup_s_median: filtered_metric(
            rows,
            |row| row.get("client_setup_s").is_some(),
            |row| {
                row.get("client_setup_s")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
            },
        ),
        cpu_ns_per_op_median: optional_metric(
            rows,
            |row| row.get("measured_process").is_some(),
            |row| {
                nested_f64(row, &["measured_process", "cpu_runtime_ns"]).unwrap_or(0.0)
                    / completed(row)
            },
        ),
        cpu_cores_median: optional_metric(
            rows,
            |row| row.get("measured_process").is_some(),
            |row| {
                nested_f64(row, &["measured_process", "cpu_runtime_fraction_of_wall"])
                    .unwrap_or(0.0)
            },
        ),
        client_throughput_max_min_ratio_median: filtered_metric(
            rows,
            |row| row.get("client_throughput_max_min_ratio").is_some(),
            |row| {
                row.get("client_throughput_max_min_ratio")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
            },
        ),
        write_conflict_retries_median: optional_metric(
            rows,
            |row| row.get("write_conflict_retries").is_some(),
            |row| {
                row.get("write_conflict_retries")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as f64
            },
        ),
        write_conflict_retries_per_k_write_ops_median: optional_metric(
            rows,
            |row| row.get("write_conflict_retries").is_some(),
            |row| {
                let retries = row
                    .get("write_conflict_retries")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as f64;
                let writes = row.get("writes").and_then(Value::as_u64).unwrap_or(0);
                let deletes = row.get("deletes").and_then(Value::as_u64).unwrap_or(0);
                1000.0 * retries / writes.saturating_add(deletes).max(1) as f64
            },
        ),
        runqueue_wait_fraction_median: optional_metric(
            rows,
            |row| row.get("measured_process").is_some(),
            |row| {
                nested_f64(row, &["measured_process", "runqueue_wait_fraction_of_wall"])
                    .unwrap_or(0.0)
            },
        ),
        read_bytes_per_op_median: optional_metric(
            rows,
            |row| row.get("measured_process").is_some(),
            |row| {
                nested_f64(row, &["measured_process", "read_bytes"]).unwrap_or(0.0) / completed(row)
            },
        ),
        write_bytes_per_op_median: optional_metric(
            rows,
            |row| row.get("measured_process").is_some(),
            |row| {
                nested_f64(row, &["measured_process", "write_bytes"]).unwrap_or(0.0)
                    / completed(row)
            },
        ),
        major_faults_per_kop_median: optional_metric(
            rows,
            |row| row.get("measured_process").is_some(),
            |row| {
                1000.0 * nested_f64(row, &["measured_process", "majflt"]).unwrap_or(0.0)
                    / completed(row)
            },
        ),
        cpu_psi_some_fraction_median: optional_metric(
            rows,
            |row| row.get("measured_system_delta").is_some(),
            |row| {
                let micros =
                    nested_f64(row, &["measured_system_delta", "psi_cpu_some_us"]).unwrap_or(0.0);
                let accounting = nested_f64(row, &["measured_system_delta", "accounting_wall_ns"])
                    .unwrap_or(0.0)
                    / 1000.0;
                let fallback =
                    row.get("elapsed_s").and_then(Value::as_f64).unwrap_or(0.0) * 1_000_000.0;
                micros
                    / if accounting != 0.0 {
                        accounting
                    } else {
                        fallback
                    }
                    .max(1.0)
            },
        ),
        io_psi_full_fraction_median: optional_metric(
            rows,
            |row| row.get("measured_system_delta").is_some(),
            |row| {
                let micros =
                    nested_f64(row, &["measured_system_delta", "psi_io_full_us"]).unwrap_or(0.0);
                let accounting = nested_f64(row, &["measured_system_delta", "accounting_wall_ns"])
                    .unwrap_or(0.0)
                    / 1000.0;
                let fallback =
                    row.get("elapsed_s").and_then(Value::as_f64).unwrap_or(0.0) * 1_000_000.0;
                micros
                    / if accounting != 0.0 {
                        accounting
                    } else {
                        fallback
                    }
                    .max(1.0)
            },
        ),
        swap_activity_pages_median: optional_metric(
            rows,
            |row| row.get("measured_system_delta").is_some(),
            |row| {
                nested_u64(row, &["measured_system_delta", "pswpin"])
                    .unwrap_or(0)
                    .saturating_add(
                        nested_u64(row, &["measured_system_delta", "pswpout"]).unwrap_or(0),
                    ) as f64
            },
        ),
        settle_cpu_ns_median: filtered_metric(
            rows,
            |row| nonempty_object(row, "post_workload_settle"),
            |row| {
                nested_f64(row, &["post_workload_settle", "process", "cpu_runtime_ns"])
                    .unwrap_or(0.0)
            },
        ),
        settle_write_bytes_median: filtered_metric(
            rows,
            |row| nonempty_object(row, "post_workload_settle"),
            |row| {
                nested_f64(row, &["post_workload_settle", "process", "write_bytes"]).unwrap_or(0.0)
            },
        ),
        settle_db_delta_bytes_median: filtered_metric(
            rows,
            |row| nonempty_object(row, "post_workload_settle"),
            |row| {
                nested_f64(row, &["post_workload_settle", "db_bytes_after"]).unwrap_or(0.0)
                    - nested_f64(row, &["post_workload_settle", "db_bytes_before"]).unwrap_or(0.0)
            },
        ),
        durability_mapping,
        speedup_vs_c1: None,
        parallel_efficiency_vs_c1: None,
        p99_read_multiplier_vs_c1: None,
        p99_write_txn_multiplier_vs_c1: None,
    })
}

fn summarize_rows(
    rows: &[Value],
    expect_trials: Option<usize>,
) -> Result<(Vec<SummaryGroup>, Vec<String>)> {
    let mut grouped: BTreeMap<GroupKey, Vec<&Value>> = BTreeMap::new();
    for row in rows {
        grouped.entry(group_key(row)?).or_default().push(row);
    }

    let mut summary = Vec::with_capacity(grouped.len());
    let mut problems = Vec::new();
    for (key, group_rows) in grouped {
        let group = summarize_group(&key, &group_rows)?;
        if let Some(expected) = expect_trials
            && group.trials.len() != expected
        {
            problems.push(format!(
                "{}/{}/{}/{}: {} trials, expected {}",
                group.lane,
                group.engine,
                group.durability,
                group.workload,
                group.trials.len(),
                expected
            ));
        }
        summary.push(group);
    }

    let baselines = summary
        .iter()
        .filter(|group| {
            matches!(group.lane.as_str(), "kv-concurrency" | "record-concurrency")
                && group.clients == 1
        })
        .map(|group| (concurrency_key(group), group.clone()))
        .collect::<BTreeMap<_, _>>();

    for group in &mut summary {
        if !matches!(group.lane.as_str(), "kv-concurrency" | "record-concurrency") {
            continue;
        }
        let Some(base) = baselines.get(&concurrency_key(group)) else {
            continue;
        };
        if base.ops_per_s_median > 0.0 {
            let speedup = group.ops_per_s_median / base.ops_per_s_median;
            group.speedup_vs_c1 = Some(speedup);
            group.parallel_efficiency_vs_c1 = Some(speedup / group.clients.max(1) as f64);
        }

        let (base_read, current_read) = if group.lane == "record-concurrency" {
            (base.p99_operation_us_median, group.p99_operation_us_median)
        } else {
            (base.p99_read_us_median, group.p99_read_us_median)
        };
        if let (Some(base_read), Some(current_read)) = (base_read, current_read)
            && base_read != 0.0
        {
            group.p99_read_multiplier_vs_c1 = Some(current_read / base_read);
        }
        if let (Some(base_write), Some(current_write)) =
            (base.p99_write_txn_us_median, group.p99_write_txn_us_median)
            && base_write != 0.0
        {
            group.p99_write_txn_multiplier_vs_c1 = Some(current_write / base_write);
        }
    }

    Ok((summary, problems))
}

fn integer_with_commas(value: f64) -> String {
    let rounded = format!("{value:.0}");
    let (sign, digits) = rounded
        .strip_prefix('-')
        .map_or(("", rounded.as_str()), |digits| ("-", digits));
    let mut reversed = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().rev().enumerate() {
        if index > 0 && index.is_multiple_of(3) {
            reversed.push(',');
        }
        reversed.push(character);
    }
    format!("{sign}{}", reversed.chars().rev().collect::<String>())
}

fn fmt(value: f64) -> String {
    if value.is_nan() {
        "—".to_owned()
    } else if value.abs() >= 1000.0 {
        integer_with_commas(value)
    } else if value.abs() >= 100.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.2}")
    }
}

fn opt_fmt(value: Option<f64>) -> String {
    value.map_or_else(|| "—".to_owned(), fmt)
}

fn markdown(summary: &[SummaryGroup], problems: &[String]) -> String {
    let mut lines = Vec::new();
    if !problems.is_empty() {
        lines.extend(["# Completeness warnings".to_owned(), String::new()]);
        lines.extend(problems.iter().map(|problem| format!("- {problem}")));
        lines.push(String::new());
    }

    let lanes = summary
        .iter()
        .map(|group| group.lane.as_str())
        .collect::<BTreeSet<_>>();
    for lane in lanes {
        lines.extend([format!("# {lane} lane"), String::new()]);
        let durabilities = summary
            .iter()
            .filter(|group| group.lane == lane)
            .map(|group| group.durability.as_str())
            .collect::<BTreeSet<_>>();
        for durability in durabilities {
            lines.extend([
                format!("## {durability}"),
                String::new(),
                "| schema | scenario | workload | config | engine | trials | median ops/s | IQR ops/s | speedup vs c1 | efficiency | p99 read/op ×c1 | p99 write-txn ×c1 | CPU cores | client max/min | client setup ms | conflict retries/k write ops | median p99 read/op us | median p99 write-txn us | DB MiB | peak RSS MiB | open ms | prefill s | CPU ns/op | rq wait %wall | CPU PSI some %wall | read B/op | write B/op | IO PSI full %wall | swap pages |".to_owned(),
                "|---:|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|".to_owned(),
            ]);
            let mut block = summary
                .iter()
                .filter(|group| group.lane == lane && group.durability == durability)
                .collect::<Vec<_>>();
            block.sort_by(|left, right| {
                left.scenario
                    .cmp(&right.scenario)
                    .then(left.workload.cmp(&right.workload))
                    .then_with(|| right.ops_per_s_median.total_cmp(&left.ops_per_s_median))
            });
            for group in block {
                let p99 = group.p99_read_us_median.or(group.p99_operation_us_median);
                lines.push(format!(
                    "| {} | {} | {} | {} | {} {} | {} | {} | {}–{} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                    group.format_version,
                    group.scenario,
                    group.workload,
                    group.configuration,
                    group.engine,
                    group.engine_version,
                    group.trials.len(),
                    fmt(group.ops_per_s_median),
                    fmt(group.ops_per_s_q1),
                    fmt(group.ops_per_s_q3),
                    opt_fmt(group.speedup_vs_c1),
                    opt_fmt(group.parallel_efficiency_vs_c1),
                    opt_fmt(group.p99_read_multiplier_vs_c1),
                    opt_fmt(group.p99_write_txn_multiplier_vs_c1),
                    opt_fmt(group.cpu_cores_median),
                    opt_fmt(group.client_throughput_max_min_ratio_median),
                    opt_fmt(group.client_setup_s_median.map(|value| value * 1000.0)),
                    opt_fmt(group.write_conflict_retries_per_k_write_ops_median),
                    opt_fmt(p99),
                    opt_fmt(group.p99_write_txn_us_median),
                    fmt(group.db_bytes_median / (1024.0 * 1024.0)),
                    opt_fmt(group.peak_rss_kib_median.map(|value| value / 1024.0)),
                    opt_fmt(group.open_s_median.map(|value| value * 1000.0)),
                    opt_fmt(group.prefill_s_median),
                    opt_fmt(group.cpu_ns_per_op_median),
                    opt_fmt(group.runqueue_wait_fraction_median.map(|value| value * 100.0)),
                    opt_fmt(group.cpu_psi_some_fraction_median.map(|value| value * 100.0)),
                    opt_fmt(group.read_bytes_per_op_median),
                    opt_fmt(group.write_bytes_per_op_median),
                    opt_fmt(group.io_psi_full_fraction_median.map(|value| value * 100.0)),
                    opt_fmt(group.swap_activity_pages_median),
                ));
            }
            lines.push(String::new());
        }
    }
    lines.join("\n")
}

fn read_rows(input: &Path) -> Result<Vec<Value>> {
    let content =
        fs::read_to_string(input).with_context(|| format!("failed to read {}", input.display()))?;
    let mut rows = Vec::new();
    for (index, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        rows.push(
            serde_json::from_str(line)
                .with_context(|| format!("invalid JSON at {}:{}", input.display(), index + 1))?,
        );
    }
    if rows.is_empty() {
        bail!("no measurements");
    }
    Ok(rows)
}

pub(crate) fn summarize_file(
    input: &Path,
    json_out: Option<&Path>,
    markdown_out: Option<&Path>,
    expect_trials: Option<usize>,
) -> Result<()> {
    let rows = read_rows(input)?;
    let (summary, problems) = summarize_rows(&rows, expect_trials)?;
    let text = markdown(&summary, &problems);
    if let Some(path) = markdown_out {
        fs::write(path, format!("{text}\n"))
            .with_context(|| format!("failed to write {}", path.display()))?;
    } else {
        println!("{text}");
    }
    if let Some(path) = json_out {
        let payload = json!({
            "source": input.to_string_lossy(),
            "row_count": rows.len(),
            "group_count": summary.len(),
            "problems": problems,
            "groups": summary,
        });
        fs::write(path, serde_json::to_string_pretty(&payload)? + "\n")
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    if !problems.is_empty() {
        bail!("summary has {} completeness problem(s)", problems.len());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{group_key, summarize_rows};
    use anyhow::{Context, Result};
    use serde_json::{Value, json};

    fn kv_row(clients: u64, state: &str, slots: u64, trial: u64, rate: f64) -> Value {
        json!({
            "format_version": 6,
            "lane": "kv-concurrency",
            "scenario": "concurrency-primary",
            "engine": "redb",
            "engine_version": "4.3.0",
            "durability": "relaxed",
            "durability_mapping": "relaxed",
            "workload": "balanced",
            "records": 100_000,
            "ops_requested": 50_000,
            "ops_completed": 50_000,
            "clients": clients,
            "value_bytes": 256,
            "value_pattern": "pseudo-random",
            "key_bytes": 8,
            "key_shape": "sequential",
            "access_pattern": "auto",
            "miss_percent": 0,
            "write_pattern": "update-uniform",
            "state_evolution": state,
            "bounded_churn_slots": slots,
            "txn_size": 100,
            "scan_len": 100,
            "trial": trial,
            "ops_per_s": rate,
            "db_bytes": 1024,
            "elapsed_s": 1.0,
            "read_latency": {"count": 1, "p99_us": 10},
            "write_txn_latency": {"count": 1, "p99_us": 20}
        })
    }

    fn record_row(pattern: Option<&str>, read: Option<&str>, write: Option<&str>) -> Value {
        let mut row = json!({
            "format_version": 8,
            "lane": "record",
            "scenario": "baseline-core",
            "engine": "sqlite",
            "engine_version": "rusqlite / SQLite",
            "durability": "sync",
            "durability_mapping": "sync",
            "workload": "point-read",
            "records": 10_000,
            "ops_requested": 10_000,
            "ops_completed": 10_000,
            "payload_bytes": 512,
            "txn_size": 100,
            "trial": 1,
            "ops_per_s": 100.0,
            "db_bytes": 4096,
            "elapsed_s": 1.0
        });
        if let Some(pattern) = pattern {
            row["payload_pattern"] = json!(pattern);
        }
        if let Some(read) = read {
            row["read_materialization"] = json!(read);
        }
        if let Some(write) = write {
            row["write_materialization"] = json!(write);
        }
        row
    }

    #[test]
    fn state_and_pool_are_group_identity() -> Result<()> {
        let growth = kv_row(8, "growth", 0, 1, 100.0);
        let bounded = kv_row(8, "bounded", 8192, 1, 100.0);
        assert_ne!(group_key(&growth)?, group_key(&bounded)?);
        Ok(())
    }

    #[test]
    fn missing_state_defaults_to_growth_without_pool() -> Result<()> {
        let explicit = kv_row(8, "growth", 0, 1, 100.0);
        let mut legacy = explicit.clone();
        legacy
            .as_object_mut()
            .context("row object")?
            .remove("state_evolution");
        legacy
            .as_object_mut()
            .context("row object")?
            .remove("bounded_churn_slots");
        assert_eq!(group_key(&legacy)?, group_key(&explicit)?);
        Ok(())
    }

    #[test]
    fn record_payload_and_materialization_are_identity() -> Result<()> {
        let baseline = record_row(
            Some("hex-random"),
            Some("full-record-v1"),
            Some("no-return-v1"),
        );
        let shape = record_row(Some("zeros"), Some("full-record-v1"), Some("no-return-v1"));
        let read = record_row(
            Some("hex-random"),
            Some("legacy-read-v0"),
            Some("no-return-v1"),
        );
        let write = record_row(
            Some("hex-random"),
            Some("full-record-v1"),
            Some("legacy-return-v0"),
        );
        assert_ne!(group_key(&baseline)?, group_key(&shape)?);
        assert_ne!(group_key(&baseline)?, group_key(&read)?);
        assert_ne!(group_key(&baseline)?, group_key(&write)?);
        Ok(())
    }

    #[test]
    fn missing_record_markers_use_legacy_defaults() -> Result<()> {
        let missing = record_row(None, None, None);
        let explicit = record_row(
            Some("hex-random"),
            Some("legacy-read-v0"),
            Some("legacy-return-v0"),
        );
        assert_eq!(group_key(&missing)?, group_key(&explicit)?);
        Ok(())
    }

    #[test]
    fn concurrency_scaling_matches_c1_with_same_state() -> Result<()> {
        let rows = vec![
            kv_row(1, "growth", 0, 1, 100.0),
            kv_row(8, "growth", 0, 1, 400.0),
            kv_row(1, "bounded", 8192, 1, 200.0),
            kv_row(8, "bounded", 8192, 1, 600.0),
        ];
        let (groups, problems) = summarize_rows(&rows, Some(1))?;
        assert!(problems.is_empty());
        let growth = groups
            .iter()
            .find(|group| group.clients == 8 && group.state_evolution == "growth")
            .context("growth c8")?;
        let bounded = groups
            .iter()
            .find(|group| group.clients == 8 && group.state_evolution == "bounded")
            .context("bounded c8")?;
        assert_eq!(growth.speedup_vs_c1, Some(4.0));
        assert_eq!(growth.parallel_efficiency_vs_c1, Some(0.5));
        assert_eq!(bounded.speedup_vs_c1, Some(3.0));
        Ok(())
    }

    #[test]
    fn concurrency_baselines_never_cross_lanes() -> Result<()> {
        let kv = kv_row(1, "growth", 0, 1, 100.0);
        let mut record = kv.clone();
        record["lane"] = json!("record-concurrency");
        record["ops_per_s"] = json!(500.0);
        let mut record_c8 = record.clone();
        record_c8["clients"] = json!(8);
        record_c8["ops_per_s"] = json!(1000.0);
        let (groups, _) = summarize_rows(&[kv, record, record_c8], Some(1))?;
        let c8 = groups
            .iter()
            .find(|group| group.lane == "record-concurrency" && group.clients == 8)
            .context("record c8")?;
        assert_eq!(c8.speedup_vs_c1, Some(2.0));
        Ok(())
    }

    #[test]
    fn partially_present_optional_metrics_ignore_missing_trials() -> Result<()> {
        let mut first = kv_row(1, "growth", 0, 1, 100.0);
        first["open_s"] = json!(0.25);
        first["post_workload_settle"] = json!({
            "requested_ms": 0,
            "process": {"cpu_runtime_ns": 100, "write_bytes": 200},
            "db_bytes_before": 1_000,
            "db_bytes_after": 1_100
        });
        let second = kv_row(1, "growth", 0, 2, 110.0);
        let (groups, problems) = summarize_rows(&[first, second], Some(2))?;
        assert!(problems.is_empty());
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].open_s_median, Some(0.25));
        assert_eq!(groups[0].settle_cpu_ns_median, Some(100.0));
        assert_eq!(groups[0].settle_write_bytes_median, Some(200.0));
        assert_eq!(groups[0].settle_db_delta_bytes_median, Some(100.0));
        Ok(())
    }

    #[test]
    fn expected_trial_mismatch_is_reported() -> Result<()> {
        let (groups, problems) = summarize_rows(&[kv_row(1, "growth", 0, 1, 100.0)], Some(3))?;
        assert_eq!(groups.len(), 1);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("1 trials, expected 3"));
        Ok(())
    }
}
