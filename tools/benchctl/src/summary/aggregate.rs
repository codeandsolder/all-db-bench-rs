use super::model::{
    GroupKey, ProcessMetrics, SettleMetrics, SummaryGroup, SystemMetrics, ThroughputMetrics,
    configuration, group_key, nested_f64, nested_u64, optional_str, required_f64, required_str,
    required_u64,
};
use crate::numeric::sample_quantile;
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
        Some(f64::midpoint(values[middle - 1], values[middle]))
    } else {
        Some(values[middle])
    }
}

fn required_median(values: Vec<f64>, label: &str) -> Result<f64> {
    median(values).with_context(|| format!("no finite values for {label}"))
}

fn quantile(mut values: Vec<f64>, q: f64) -> Result<f64> {
    values.retain(|value| value.is_finite());
    sample_quantile(&mut values, q).context("cannot compute quantile")
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

fn scalar_metric(rows: &[&Value], field: &str) -> Option<f64> {
    filtered_metric(
        rows,
        |row| row.get(field).is_some(),
        |row| row.get(field).and_then(Value::as_f64).unwrap_or(0.0),
    )
}

fn nonempty_object(row: &Value, key: &str) -> bool {
    row.get(key)
        .and_then(Value::as_object)
        .is_some_and(|object| !object.is_empty())
}

fn throughput_metrics(rows: &[&Value]) -> Result<ThroughputMetrics> {
    let rates = rows
        .iter()
        .map(|row| required_f64(row, "ops_per_s"))
        .collect::<Result<Vec<_>>>()?;
    Ok(ThroughputMetrics {
        median: required_median(rates.clone(), "ops_per_s")?,
        q1: quantile(rates.clone(), 0.25)?,
        q3: quantile(rates.clone(), 0.75)?,
        min: rates
            .iter()
            .copied()
            .reduce(f64::min)
            .context("missing ops_per_s")?,
        max: rates
            .iter()
            .copied()
            .reduce(f64::max)
            .context("missing ops_per_s")?,
    })
}

fn process_metrics(rows: &[&Value]) -> ProcessMetrics {
    let metric = |field, normalize: fn(&Value) -> f64| {
        optional_metric(
            rows,
            |row| row.get("measured_process").is_some(),
            |row| nested_f64(row, &["measured_process", field]).unwrap_or(0.0) / normalize(row),
        )
    };
    let completed =
        |row: &Value| int_as_f64!(required_u64(row, "ops_completed").unwrap_or(1).max(1));
    ProcessMetrics {
        cpu_ns_per_op: metric("cpu_runtime_ns", completed),
        cpu_cores: metric("cpu_runtime_fraction_of_wall", |_| 1.0),
        runqueue_wait_fraction: metric("runqueue_wait_fraction_of_wall", |_| 1.0),
        read_bytes_per_op: metric("read_bytes", completed),
        write_bytes_per_op: metric("write_bytes", completed),
        major_faults_per_kop: optional_metric(
            rows,
            |row| row.get("measured_process").is_some(),
            |row| {
                1000.0 * nested_f64(row, &["measured_process", "majflt"]).unwrap_or(0.0)
                    / completed(row)
            },
        ),
    }
}

fn psi_fraction(row: &Value, field: &str) -> f64 {
    let micros = nested_f64(row, &["measured_system_delta", field]).unwrap_or(0.0);
    let accounting =
        nested_f64(row, &["measured_system_delta", "accounting_wall_ns"]).unwrap_or(0.0) / 1000.0;
    let fallback = row.get("elapsed_s").and_then(Value::as_f64).unwrap_or(0.0) * 1_000_000.0;
    micros
        / if accounting == 0.0 {
            fallback
        } else {
            accounting
        }
        .max(1.0)
}

fn system_metrics(rows: &[&Value]) -> SystemMetrics {
    let present = |row: &Value| row.get("measured_system_delta").is_some();
    SystemMetrics {
        cpu_psi_some_fraction: optional_metric(rows, present, |row| {
            psi_fraction(row, "psi_cpu_some_us")
        }),
        io_psi_full_fraction: optional_metric(rows, present, |row| {
            psi_fraction(row, "psi_io_full_us")
        }),
        swap_activity_pages: optional_metric(rows, present, |row| {
            int_as_f64!(
                nested_u64(row, &["measured_system_delta", "pswpin"])
                    .unwrap_or(0)
                    .saturating_add(
                        nested_u64(row, &["measured_system_delta", "pswpout"]).unwrap_or(0)
                    )
            )
        }),
    }
}

fn settle_metrics(rows: &[&Value]) -> SettleMetrics {
    SettleMetrics {
        cpu_ns: filtered_metric(
            rows,
            |row| nonempty_object(row, "post_workload_settle"),
            |row| {
                nested_f64(row, &["post_workload_settle", "process", "cpu_runtime_ns"])
                    .unwrap_or(0.0)
            },
        ),
        write_bytes: filtered_metric(
            rows,
            |row| nonempty_object(row, "post_workload_settle"),
            |row| {
                nested_f64(row, &["post_workload_settle", "process", "write_bytes"]).unwrap_or(0.0)
            },
        ),
        db_delta_bytes: filtered_metric(
            rows,
            |row| nonempty_object(row, "post_workload_settle"),
            |row| {
                nested_f64(row, &["post_workload_settle", "db_bytes_after"]).unwrap_or(0.0)
                    - nested_f64(row, &["post_workload_settle", "db_bytes_before"]).unwrap_or(0.0)
            },
        ),
    }
}

fn summarize_group(key: &GroupKey, rows: &[&Value]) -> Result<SummaryGroup> {
    let trials = rows
        .iter()
        .map(|row| required_u64(row, "trial"))
        .collect::<Result<BTreeSet<_>>>()?
        .into_iter()
        .collect::<Vec<_>>();
    if trials.len() != rows.len() || trials.first() == Some(&0) {
        bail!("duplicate or zero trial ID for {key:?}");
    }
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
        key: key.clone(),
        engine_compression_mapping,
        configuration: configuration(key),
        trials,
        throughput: throughput_metrics(rows)?,
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
                .map(|row| required_u64(row, "db_bytes").map(|value| int_as_f64!(value)))
                .collect::<Result<Vec<_>>>()?,
            "db_bytes",
        )?,
        peak_rss_kib_median: filtered_metric(
            rows,
            |row| row.get("peak_rss_kib").is_some(),
            |row| int_as_f64!(row.get("peak_rss_kib").and_then(Value::as_u64).unwrap_or(0)),
        ),
        open_s_median: scalar_metric(rows, "open_s"),
        prefill_s_median: scalar_metric(rows, "prefill_s"),
        client_setup_s_median: scalar_metric(rows, "client_setup_s"),
        process: process_metrics(rows),
        client_throughput_max_min_ratio_median: scalar_metric(
            rows,
            "client_throughput_max_min_ratio",
        ),
        write_conflict_retries_median: optional_metric(
            rows,
            |row| row.get("write_conflict_retries").is_some(),
            |row| {
                int_as_f64!(
                    row.get("write_conflict_retries")
                        .and_then(Value::as_u64)
                        .unwrap_or(0)
                )
            },
        ),
        write_conflict_retries_per_k_write_ops_median: optional_metric(
            rows,
            |row| row.get("write_conflict_retries").is_some(),
            |row| {
                let retries = int_as_f64!(
                    row.get("write_conflict_retries")
                        .and_then(Value::as_u64)
                        .unwrap_or(0)
                );
                let writes = row.get("writes").and_then(Value::as_u64).unwrap_or(0);
                let deletes = row.get("deletes").and_then(Value::as_u64).unwrap_or(0);
                1000.0 * retries / int_as_f64!(writes.saturating_add(deletes).max(1))
            },
        ),
        system: system_metrics(rows),
        settle: settle_metrics(rows),
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
        let key = group_key(row)?;
        if key.ops_requested == 0 || required_u64(row, "ops_completed")? != key.ops_requested {
            bail!("incomplete measurement for {key:?}");
        }
        if required_f64(row, "elapsed_s")? <= 0.0 || required_f64(row, "ops_per_s")? <= 0.0 {
            bail!("nonpositive measurement timing for {key:?}");
        }
        grouped.entry(key).or_default().push(row);
    }

    let mut summary = Vec::with_capacity(grouped.len());
    let mut problems = Vec::new();
    for (key, group_rows) in grouped {
        let group = summarize_group(&key, &group_rows)?;
        if let Some(expected) = expect_trials
            && (group.trials.len() != expected
                || group
                    .trials
                    .iter()
                    .copied()
                    .ne(1..=u64::try_from(expected)?))
        {
            let lane = &group.key.lane;
            let engine = &group.key.engine;
            let durability = &group.key.durability;
            let workload = &group.key.workload;
            let observed = group.trials.len();
            problems.push(format!(
                "{lane}/{engine}/{durability}/{workload}: {observed} trials, expected {expected}; IDs: {:?}", group.trials
            ));
        }
        summary.push(group);
    }

    let baselines = summary
        .iter()
        .filter(|group| {
            matches!(
                group.key.lane.as_str(),
                "kv-concurrency" | "record-concurrency"
            ) && group.key.clients == 1
        })
        .map(|group| (group.key.clone(), group.clone()))
        .collect::<BTreeMap<_, _>>();

    for group in &mut summary {
        if !matches!(
            group.key.lane.as_str(),
            "kv-concurrency" | "record-concurrency"
        ) {
            continue;
        }
        let Some(base) = baselines.get(&group.key.single_client()) else {
            continue;
        };
        if base.throughput.median > 0.0 {
            let speedup = group.throughput.median / base.throughput.median;
            group.speedup_vs_c1 = Some(speedup);
            group.parallel_efficiency_vs_c1 = Some(speedup / int_as_f64!(group.key.clients.max(1)));
        }

        let (base_read, current_read) = if group.key.lane == "record-concurrency" {
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
