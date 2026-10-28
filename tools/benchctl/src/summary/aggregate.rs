use super::model::{
    GroupKey, SummaryGroup, concurrency_key, configuration, group_key, nested_f64, nested_u64,
    optional_str, required_f64, required_str, required_u64,
};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

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

pub(super) fn summarize_rows(
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
