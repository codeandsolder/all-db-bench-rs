use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, path::Path};

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct GroupKey {
    lane: String,
    engine: String,
    engine_version: String,
    engine_compression: String,
    workload: String,
    durability: String,
    records: u64,
    ops_requested: u64,
    bytes: u64,
    pattern: String,
    settle_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
struct Ratios {
    throughput_ratio: Option<f64>,
    cpu_ns_per_op_ratio: Option<f64>,
    apparent_bytes_ratio: Option<f64>,
    allocated_block_bytes_ratio: Option<f64>,
    device_write_bytes_ratio: Option<f64>,
    zfs_dataset_used_ratio: Option<f64>,
    zfs_dataset_logicalused_ratio: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
struct SummaryGroup {
    lane: String,
    engine: String,
    engine_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    engine_compression: Option<String>,
    workload: String,
    durability: String,
    records: u64,
    ops_requested: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    value_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    value_pattern: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload_pattern: Option<String>,
    settle_ms: u64,
    trials: Vec<u64>,
    ops_per_s_median: Option<f64>,
    cpu_ns_per_op_median: Option<f64>,
    db_apparent_bytes_median: Option<f64>,
    db_allocated_block_bytes_median: Option<f64>,
    db_allocated_block_bytes_reliable: bool,
    logical_live_bytes_median: Option<f64>,
    logical_live_per_allocated_byte_median: Option<f64>,
    lifecycle_device_read_bytes_median: Option<f64>,
    lifecycle_device_write_bytes_median: Option<f64>,
    zfs_dataset_used_delta_median: Option<f64>,
    zfs_dataset_logicalused_delta_median: Option<f64>,
    zfs_pool_direct_write_bytes_delta_median: Option<f64>,
    zfs_pool_arc_write_bytes_delta_median: Option<f64>,
    measured_process_read_bytes_per_op_median: Option<f64>,
    measured_process_write_bytes_per_op_median: Option<f64>,
    p99_read_us_median: Option<f64>,
    p99_write_txn_us_median: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vs_none: Option<Ratios>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vs_hex_random: Option<Ratios>,
}

fn req<'a>(value: &'a Value, key: &str) -> Result<&'a Value> {
    value
        .get(key)
        .with_context(|| format!("missing required field {key}"))
}

fn req_str(value: &Value, key: &str) -> Result<String> {
    req(value, key)?
        .as_str()
        .map(ToOwned::to_owned)
        .with_context(|| format!("field {key} is not a string"))
}

fn req_u64(value: &Value, key: &str) -> Result<u64> {
    req(value, key)?
        .as_u64()
        .with_context(|| format!("field {key} is not an unsigned integer"))
}

fn req_f64(value: &Value, key: &str) -> Result<f64> {
    req(value, key)?
        .as_f64()
        .filter(|number| number.is_finite())
        .with_context(|| format!("field {key} is not a finite number"))
}

fn nested<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter()
        .try_fold(value, |current, key| current.get(*key))
}

fn nested_u64(value: &Value, path: &[&str]) -> Option<u64> {
    nested(value, path).and_then(Value::as_u64)
}

fn nested_f64(value: &Value, path: &[&str]) -> Option<f64> {
    nested(value, path)
        .and_then(Value::as_f64)
        .filter(|number| number.is_finite())
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

fn ratio(value: Option<f64>, baseline: Option<f64>) -> Option<f64> {
    match (value, baseline) {
        (Some(value), Some(baseline)) if baseline != 0.0 => Some(value / baseline),
        _ => None,
    }
}

fn device_bytes(row: &Value) -> (u64, u64) {
    let before = nested(
        row,
        &[
            "compression_storage",
            "storage_counters",
            "before",
            "devices",
        ],
    )
    .and_then(Value::as_array);
    let after = nested(
        row,
        &[
            "compression_storage",
            "storage_counters",
            "after",
            "devices",
        ],
    )
    .and_then(Value::as_array);
    let (Some(before), Some(after)) = (before, after) else {
        return (0, 0);
    };

    let mut before_by_path: BTreeMap<&str, &Value> = BTreeMap::new();
    for device in before {
        if let Some(path) = device.get("path").and_then(Value::as_str) {
            before_by_path.insert(path, device);
        }
    }

    let mut read_bytes = 0u64;
    let mut write_bytes = 0u64;
    for end in after {
        let Some(path) = end.get("path").and_then(Value::as_str) else {
            continue;
        };
        let Some(start) = before_by_path.get(path) else {
            continue;
        };
        let Some(start_stat) = start.get("stat").and_then(Value::as_array) else {
            continue;
        };
        let Some(end_stat) = end.get("stat").and_then(Value::as_array) else {
            continue;
        };
        if start_stat.len() < 7 || end_stat.len() < 7 {
            continue;
        }
        let start_read = start_stat[2].as_u64().unwrap_or(0);
        let end_read = end_stat[2].as_u64().unwrap_or(0);
        let start_write = start_stat[6].as_u64().unwrap_or(0);
        let end_write = end_stat[6].as_u64().unwrap_or(0);
        read_bytes =
            read_bytes.saturating_add(end_read.saturating_sub(start_read).saturating_mul(512));
        write_bytes =
            write_bytes.saturating_add(end_write.saturating_sub(start_write).saturating_mul(512));
    }
    (read_bytes, write_bytes)
}

fn nested_delta(row: &Value, section: &str, property: &str) -> Option<f64> {
    let tail = if section == "dataset_snapshot" {
        [
            "compression_storage",
            section,
            "before",
            "zfs_dataset",
            property,
        ]
    } else {
        [
            "compression_storage",
            section,
            "before",
            "zfs_pool_io",
            property,
        ]
    };
    let head = if section == "dataset_snapshot" {
        [
            "compression_storage",
            section,
            "after",
            "zfs_dataset",
            property,
        ]
    } else {
        [
            "compression_storage",
            section,
            "after",
            "zfs_pool_io",
            property,
        ]
    };
    let before = nested_f64(row, &tail)?;
    let after = nested_f64(row, &head)?;
    Some((after - before).max(0.0))
}

fn group_key(row: &Value) -> Result<GroupKey> {
    let lane = req_str(row, "lane")?;
    if lane != "kv-compression" && lane != "record-compression" {
        bail!("unsupported compression lane {lane}");
    }
    let (bytes, pattern, engine_compression) = if lane == "kv-compression" {
        (
            req_u64(row, "value_bytes")?,
            req_str(row, "value_pattern")?,
            req_str(row, "engine_compression")?,
        )
    } else {
        (
            req_u64(row, "payload_bytes")?,
            req_str(row, "payload_pattern")?,
            "product-default".to_owned(),
        )
    };
    Ok(GroupKey {
        lane,
        engine: req_str(row, "engine")?,
        engine_version: req_str(row, "engine_version")?,
        engine_compression,
        workload: req_str(row, "workload")?,
        durability: req_str(row, "durability")?,
        records: req_u64(row, "records")?,
        ops_requested: req_u64(row, "ops_requested")?,
        bytes,
        pattern,
        settle_ms: nested_u64(row, &["post_workload_settle", "requested_ms"]).unwrap_or(0),
    })
}

fn logical_live_bytes(row: &Value, key: &GroupKey) -> u64 {
    if key.lane == "record-compression" {
        let prefill = nested_u64(
            row,
            &["compression_storage", "logical", "prefill_record_bytes"],
        );
        let foreground = nested_u64(
            row,
            &["compression_storage", "logical", "foreground_record_bytes"],
        );
        if let (Some(prefill), Some(foreground)) = (prefill, foreground) {
            return prefill.saturating_add(foreground);
        }
        return key.records.saturating_mul(key.bytes.saturating_add(12));
    }

    let key_bytes = row.get("key_bytes").and_then(Value::as_u64).unwrap_or(8);
    let mut live_records = key.records;
    if key.workload == "write-burst"
        && row.get("write_pattern").and_then(Value::as_str) == Some("append")
    {
        live_records =
            live_records.saturating_add(row.get("writes").and_then(Value::as_u64).unwrap_or(0));
    }
    live_records.saturating_mul(key_bytes.saturating_add(key.bytes))
}

fn latency_p99(row: &Value, field: &str) -> Option<f64> {
    let count = nested_u64(row, &[field, "count"])?;
    if count == 0 {
        return None;
    }
    nested_f64(row, &[field, "p99_us"])
}

fn summarize_group(key: &GroupKey, rows: &[&Value]) -> Result<SummaryGroup> {
    let mut trials = Vec::with_capacity(rows.len());
    let mut ops_per_s = Vec::with_capacity(rows.len());
    let mut cpu_ns_per_op = Vec::with_capacity(rows.len());
    let mut apparent = Vec::with_capacity(rows.len());
    let mut allocated = Vec::with_capacity(rows.len());
    let mut logical = Vec::with_capacity(rows.len());
    let mut logical_per_allocated = Vec::with_capacity(rows.len());
    let mut device_reads = Vec::with_capacity(rows.len());
    let mut device_writes = Vec::with_capacity(rows.len());
    let mut process_reads_per_op = Vec::with_capacity(rows.len());
    let mut process_writes_per_op = Vec::with_capacity(rows.len());
    let mut zfs_used = Vec::new();
    let mut zfs_logical = Vec::new();
    let mut zfs_direct_write = Vec::new();
    let mut zfs_arc_write = Vec::new();
    let mut p99_read = Vec::new();
    let mut p99_write = Vec::new();

    let allocation_reliable = rows.iter().all(|row| {
        nested(
            row,
            &[
                "compression_storage",
                "footprint",
                "allocated_block_bytes_reliable",
            ],
        )
        .and_then(Value::as_bool)
        .unwrap_or(true)
    });

    for row in rows {
        trials.push(req_u64(row, "trial")?);
        ops_per_s.push(req_f64(row, "ops_per_s")?);
        let completed = req_u64(row, "ops_completed")?.max(1);
        let cpu_ns = nested_f64(row, &["measured_process", "cpu_runtime_ns"])
            .context("missing measured_process.cpu_runtime_ns")?;
        cpu_ns_per_op.push(cpu_ns / completed as f64);

        let apparent_bytes =
            nested_f64(row, &["compression_storage", "footprint", "apparent_bytes"])
                .context("missing compression_storage.footprint.apparent_bytes")?;
        apparent.push(apparent_bytes);
        let logical_bytes = logical_live_bytes(row, key) as f64;
        logical.push(logical_bytes);

        if allocation_reliable {
            let allocated_bytes = nested_f64(
                row,
                &["compression_storage", "footprint", "allocated_block_bytes"],
            )
            .context("missing compression_storage.footprint.allocated_block_bytes")?;
            allocated.push(allocated_bytes);
            if allocated_bytes != 0.0 {
                logical_per_allocated.push(logical_bytes / allocated_bytes);
            }
        }

        let (read_bytes, write_bytes) = device_bytes(row);
        device_reads.push(read_bytes as f64);
        device_writes.push(write_bytes as f64);
        let process_read = nested_f64(row, &["measured_process", "read_bytes"]).unwrap_or(0.0);
        let process_write = nested_f64(row, &["measured_process", "write_bytes"]).unwrap_or(0.0);
        process_reads_per_op.push(process_read / completed as f64);
        process_writes_per_op.push(process_write / completed as f64);

        if let Some(value) = nested_delta(row, "dataset_snapshot", "used") {
            zfs_used.push(value);
        }
        if let Some(value) = nested_delta(row, "dataset_snapshot", "logicalused") {
            zfs_logical.push(value);
        }
        if let Some(value) = nested_delta(row, "storage_counters", "direct_write_bytes") {
            zfs_direct_write.push(value);
        }
        if let Some(value) = nested_delta(row, "storage_counters", "arc_write_bytes") {
            zfs_arc_write.push(value);
        }
        if let Some(value) = latency_p99(row, "read_latency") {
            p99_read.push(value);
        }
        if let Some(value) = latency_p99(row, "write_txn_latency") {
            p99_write.push(value);
        }
    }
    trials.sort_unstable();

    Ok(SummaryGroup {
        lane: key.lane.clone(),
        engine: key.engine.clone(),
        engine_version: key.engine_version.clone(),
        engine_compression: (key.lane == "kv-compression").then(|| key.engine_compression.clone()),
        workload: key.workload.clone(),
        durability: key.durability.clone(),
        records: key.records,
        ops_requested: key.ops_requested,
        value_bytes: (key.lane == "kv-compression").then_some(key.bytes),
        value_pattern: (key.lane == "kv-compression").then(|| key.pattern.clone()),
        payload_bytes: (key.lane == "record-compression").then_some(key.bytes),
        payload_pattern: (key.lane == "record-compression").then(|| key.pattern.clone()),
        settle_ms: key.settle_ms,
        trials,
        ops_per_s_median: median(ops_per_s),
        cpu_ns_per_op_median: median(cpu_ns_per_op),
        db_apparent_bytes_median: median(apparent),
        db_allocated_block_bytes_median: if allocation_reliable {
            median(allocated)
        } else {
            None
        },
        db_allocated_block_bytes_reliable: allocation_reliable,
        logical_live_bytes_median: median(logical),
        logical_live_per_allocated_byte_median: if allocation_reliable {
            median(logical_per_allocated)
        } else {
            None
        },
        lifecycle_device_read_bytes_median: median(device_reads),
        lifecycle_device_write_bytes_median: median(device_writes),
        zfs_dataset_used_delta_median: median(zfs_used),
        zfs_dataset_logicalused_delta_median: median(zfs_logical),
        zfs_pool_direct_write_bytes_delta_median: median(zfs_direct_write),
        zfs_pool_arc_write_bytes_delta_median: median(zfs_arc_write),
        measured_process_read_bytes_per_op_median: median(process_reads_per_op),
        measured_process_write_bytes_per_op_median: median(process_writes_per_op),
        p99_read_us_median: median(p99_read),
        p99_write_txn_us_median: median(p99_write),
        vs_none: None,
        vs_hex_random: None,
    })
}

fn same_baseline_dimensions(left: &SummaryGroup, right: &SummaryGroup) -> bool {
    left.lane == right.lane
        && left.engine == right.engine
        && left.engine_version == right.engine_version
        && left.workload == right.workload
        && left.durability == right.durability
        && left.records == right.records
        && left.ops_requested == right.ops_requested
        && left.value_bytes == right.value_bytes
        && left.payload_bytes == right.payload_bytes
        && left.settle_ms == right.settle_ms
}

fn ratios(item: &SummaryGroup, baseline: &SummaryGroup) -> Ratios {
    Ratios {
        throughput_ratio: ratio(item.ops_per_s_median, baseline.ops_per_s_median),
        cpu_ns_per_op_ratio: ratio(item.cpu_ns_per_op_median, baseline.cpu_ns_per_op_median),
        apparent_bytes_ratio: ratio(
            item.db_apparent_bytes_median,
            baseline.db_apparent_bytes_median,
        ),
        allocated_block_bytes_ratio: if item.db_allocated_block_bytes_reliable
            && baseline.db_allocated_block_bytes_reliable
        {
            ratio(
                item.db_allocated_block_bytes_median,
                baseline.db_allocated_block_bytes_median,
            )
        } else {
            None
        },
        device_write_bytes_ratio: ratio(
            item.lifecycle_device_write_bytes_median,
            baseline.lifecycle_device_write_bytes_median,
        ),
        zfs_dataset_used_ratio: ratio(
            item.zfs_dataset_used_delta_median,
            baseline.zfs_dataset_used_delta_median,
        ),
        zfs_dataset_logicalused_ratio: ratio(
            item.zfs_dataset_logicalused_delta_median,
            baseline.zfs_dataset_logicalused_delta_median,
        ),
    }
}

fn apply_baselines(summary: &mut [SummaryGroup]) {
    let snapshot = summary.to_vec();
    for item in summary {
        if item.lane == "kv-compression" {
            let baseline = snapshot.iter().find(|candidate| {
                same_baseline_dimensions(item, candidate)
                    && candidate.engine_compression.as_deref() == Some("none")
                    && candidate.value_pattern == item.value_pattern
            });
            item.vs_none = baseline.map(|baseline| ratios(item, baseline));
        } else {
            let baseline = snapshot.iter().find(|candidate| {
                same_baseline_dimensions(item, candidate)
                    && candidate.payload_pattern.as_deref() == Some("hex-random")
            });
            item.vs_hex_random = baseline.map(|baseline| ratios(item, baseline));
        }
    }
}

fn summarize(rows: &[Value]) -> Result<Vec<SummaryGroup>> {
    let mut grouped: BTreeMap<GroupKey, Vec<&Value>> = BTreeMap::new();
    for row in rows {
        grouped.entry(group_key(row)?).or_default().push(row);
    }
    let mut summary = Vec::with_capacity(grouped.len());
    for (key, group_rows) in grouped {
        summary.push(summarize_group(&key, &group_rows)?);
    }
    apply_baselines(&mut summary);
    Ok(summary)
}

fn fnum(value: Option<f64>, digits: usize) -> String {
    match value {
        Some(value) if value.is_finite() => format!("{value:.digits$}"),
        _ => "—".to_owned(),
    }
}

fn mib(value: Option<f64>) -> Option<f64> {
    value.map(|bytes| bytes / (1024.0 * 1024.0))
}

fn markdown(summary: &[SummaryGroup], lane: &str) -> String {
    let mut lines = Vec::new();
    if lane == "kv-compression" {
        lines.extend([
            "# KV compression summary".to_owned(),
            String::new(),
            "Ratios are relative to the same engine/workload/value-shape case with engine compression `none`.".to_owned(),
            "Filesystem allocated-block bytes use stat.st_blocks and are suppressed on ZFS; ZFS used/logical-used deltas are captured after storage settle.".to_owned(),
            String::new(),
            "| Engine | Codec | Workload | Value | Shape | ops/s | CPU ns/op | alloc MiB | ZFS used Δ MiB | ZFS logical Δ MiB | device write MiB | thr/none | CPU/none | physical/none |".to_owned(),
            "|---|---|---|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|".to_owned(),
        ]);
        for item in summary {
            let ratios = item.vs_none.as_ref();
            let physical = ratios.and_then(|values| {
                if item.zfs_dataset_used_delta_median.is_some() {
                    values.zfs_dataset_used_ratio
                } else {
                    values.allocated_block_bytes_ratio
                }
            });
            lines.push(format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                item.engine,
                item.engine_compression.as_deref().unwrap_or("—"),
                item.workload,
                item.value_bytes.unwrap_or(0),
                item.value_pattern.as_deref().unwrap_or("—"),
                fnum(item.ops_per_s_median, 0),
                fnum(item.cpu_ns_per_op_median, 0),
                fnum(mib(item.db_allocated_block_bytes_median), 2),
                fnum(mib(item.zfs_dataset_used_delta_median), 2),
                fnum(mib(item.zfs_dataset_logicalused_delta_median), 2),
                fnum(mib(item.lifecycle_device_write_bytes_median), 2),
                fnum(ratios.and_then(|values| values.throughput_ratio), 3),
                fnum(ratios.and_then(|values| values.cpu_ns_per_op_ratio), 3),
                fnum(physical, 3),
            ));
        }
    } else {
        lines.extend([
            "# Record compression summary".to_owned(),
            String::new(),
            "Ratios are relative to the same engine/workload/payload-size case using the deterministic `hex-random` payload.".to_owned(),
            "Filesystem allocated-block bytes use stat.st_blocks and are suppressed on ZFS; ZFS used/logical-used deltas are captured after storage settle.".to_owned(),
            String::new(),
            "| Engine | Workload | Payload | Shape | ops/s | CPU ns/op | apparent MiB | alloc MiB | ZFS used Δ MiB | device write MiB | thr/hex | CPU/hex | physical/hex |".to_owned(),
            "|---|---|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|".to_owned(),
        ]);
        for item in summary {
            let ratios = item.vs_hex_random.as_ref();
            let physical = ratios.and_then(|values| {
                if item.zfs_dataset_used_delta_median.is_some() {
                    values.zfs_dataset_used_ratio
                } else {
                    values.allocated_block_bytes_ratio
                }
            });
            lines.push(format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                item.engine,
                item.workload,
                item.payload_bytes.unwrap_or(0),
                item.payload_pattern.as_deref().unwrap_or("—"),
                fnum(item.ops_per_s_median, 0),
                fnum(item.cpu_ns_per_op_median, 0),
                fnum(mib(item.db_apparent_bytes_median), 2),
                fnum(mib(item.db_allocated_block_bytes_median), 2),
                fnum(mib(item.zfs_dataset_used_delta_median), 2),
                fnum(mib(item.lifecycle_device_write_bytes_median), 2),
                fnum(ratios.and_then(|values| values.throughput_ratio), 3),
                fnum(ratios.and_then(|values| values.cpu_ns_per_op_ratio), 3),
                fnum(physical, 3),
            ));
        }
    }
    lines.push(String::new());
    lines.join("\n")
}

pub(crate) fn summarize_file(input: &Path, json_out: &Path, markdown_out: &Path) -> Result<()> {
    let content =
        fs::read_to_string(input).with_context(|| format!("failed to read {}", input.display()))?;
    let mut rows = Vec::new();
    for (index, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        rows.push(
            serde_json::from_str::<Value>(line)
                .with_context(|| format!("invalid JSON at {}:{}", input.display(), index + 1))?,
        );
    }
    if rows.is_empty() {
        bail!("no compression measurements");
    }
    let lane = req_str(&rows[0], "lane")?;
    if rows
        .iter()
        .any(|row| row.get("lane").and_then(Value::as_str) != Some(lane.as_str()))
    {
        bail!("compression input mixes lanes");
    }
    let summary = summarize(&rows)?;
    let payload = json!({
        "format_version": 1,
        "lane": lane,
        "row_count": rows.len(),
        "group_count": summary.len(),
        "groups": summary,
    });
    fs::write(json_out, serde_json::to_string_pretty(&payload)? + "\n")
        .with_context(|| format!("failed to write {}", json_out.display()))?;
    fs::write(markdown_out, markdown(&summary, &lane))
        .with_context(|| format!("failed to write {}", markdown_out.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{device_bytes, logical_live_bytes, summarize};
    use anyhow::{Context, Result};
    use serde_json::{Value, json};

    fn kv_row(
        codec: &str,
        throughput: f64,
        cpu_ns: u64,
        allocated: u64,
        sectors_written: u64,
    ) -> Value {
        json!({
            "lane": "kv-compression",
            "engine": "turbokv",
            "engine_version": "0.6.0",
            "engine_compression": codec,
            "workload": "write-burst",
            "durability": "sync",
            "records": 100,
            "ops_requested": 20,
            "ops_completed": 20,
            "writes": 20,
            "value_bytes": 4096,
            "value_pattern": "structured-text",
            "key_bytes": 8,
            "write_pattern": "append",
            "trial": 1,
            "ops_per_s": throughput,
            "read_latency": {"count": 0, "p99_us": 0},
            "write_txn_latency": {"count": 1, "p99_us": 123},
            "measured_process": {"cpu_runtime_ns": cpu_ns, "read_bytes": 0, "write_bytes": 8192},
            "post_workload_settle": {"requested_ms": 1000},
            "compression_storage": {
                "footprint": {
                    "apparent_bytes": allocated * 2,
                    "allocated_block_bytes": allocated,
                    "allocated_block_bytes_reliable": true
                },
                "storage_counters": {
                    "before": {"devices": [{"path": "/dev/test", "stat": [0, 0, 10, 0, 0, 0, 20]}]},
                    "after": {"devices": [{"path": "/dev/test", "stat": [0, 0, 14, 0, 0, 0, 20 + sectors_written]}]}
                }
            }
        })
    }

    fn record_row(pattern: &str, apparent: u64) -> Value {
        json!({
            "lane": "record-compression",
            "engine": "sqlite",
            "engine_version": "3.54.0",
            "workload": "point-read",
            "durability": "sync",
            "records": 100,
            "ops_requested": 20,
            "ops_completed": 20,
            "payload_bytes": 512,
            "payload_pattern": pattern,
            "trial": 1,
            "ops_per_s": 100.0,
            "read_latency": {"count": 20, "p99_us": 10},
            "write_txn_latency": {"count": 0, "p99_us": 0},
            "measured_process": {"cpu_runtime_ns": 20000, "read_bytes": 0, "write_bytes": 0},
            "compression_storage": {
                "footprint": {
                    "apparent_bytes": apparent,
                    "allocated_block_bytes": apparent,
                    "allocated_block_bytes_reliable": true
                },
                "storage_counters": {"before": {"devices": []}, "after": {"devices": []}},
                "logical": {"prefill_record_bytes": 52_400, "foreground_record_bytes": 0}
            }
        })
    }

    #[test]
    fn kv_codec_ratios_use_matching_none_case() -> Result<()> {
        let groups = summarize(&[
            kv_row("none", 100.0, 20_000, 1_000_000, 100),
            kv_row("lz4", 125.0, 25_000, 500_000, 40),
        ])?;
        let lz4 = groups
            .iter()
            .find(|item| item.engine_compression.as_deref() == Some("lz4"))
            .context("missing lz4 group")?;
        let ratios = lz4.vs_none.as_ref().context("missing none baseline")?;
        assert_eq!(ratios.throughput_ratio, Some(1.25));
        assert_eq!(ratios.cpu_ns_per_op_ratio, Some(1.25));
        assert_eq!(ratios.allocated_block_bytes_ratio, Some(0.5));
        assert_eq!(ratios.device_write_bytes_ratio, Some(0.4));
        Ok(())
    }

    #[test]
    fn device_sector_accounting_uses_512_byte_units() {
        let row = kv_row("none", 1.0, 1, 1, 7);
        assert_eq!(device_bytes(&row), (4 * 512, 7 * 512));
    }

    #[test]
    fn kv_logical_live_bytes_counts_appends() -> Result<()> {
        let row = kv_row("none", 1.0, 1, 1, 0);
        let key = super::group_key(&row)?;
        assert_eq!(logical_live_bytes(&row, &key), 120 * (4096 + 8));
        Ok(())
    }

    #[test]
    fn kv_zfs_uses_dataset_deltas_not_st_blocks() -> Result<()> {
        let mut baseline = kv_row("none", 100.0, 20_000, 1536, 100);
        let mut compressed = kv_row("zstd", 90.0, 24_000, 512, 70);
        for (item, used_delta, logical_delta) in [
            (&mut baseline, 1_000_000_u64, 2_000_000_u64),
            (&mut compressed, 500_000_u64, 1_200_000_u64),
        ] {
            item["compression_storage"]["footprint"]["allocated_block_bytes_reliable"] =
                json!(false);
            item["compression_storage"]["dataset_snapshot"] = json!({
                "before": {"zfs_dataset": {"used": 10_000_000, "logicalused": 20_000_000}},
                "after": {"zfs_dataset": {
                    "used": 10_000_000 + used_delta,
                    "logicalused": 20_000_000 + logical_delta
                }}
            });
        }

        let groups = summarize(&[baseline, compressed])?;
        let compressed = groups
            .iter()
            .find(|item| item.engine_compression.as_deref() == Some("zstd"))
            .context("missing zstd group")?;
        let ratios = compressed
            .vs_none
            .as_ref()
            .context("missing none baseline")?;
        assert!(!compressed.db_allocated_block_bytes_reliable);
        assert_eq!(compressed.db_allocated_block_bytes_median, None);
        assert_eq!(compressed.logical_live_per_allocated_byte_median, None);
        assert_eq!(ratios.allocated_block_bytes_ratio, None);
        assert_eq!(ratios.zfs_dataset_used_ratio, Some(0.5));
        assert_eq!(ratios.zfs_dataset_logicalused_ratio, Some(0.6));
        Ok(())
    }

    #[test]
    fn record_logical_live_bytes_uses_runner_semantics() -> Result<()> {
        let mut row = record_row("zeros", 250_000);
        row["compression_storage"]["logical"]["foreground_record_bytes"] = json!(10_480);
        let key = super::group_key(&row)?;
        assert_eq!(logical_live_bytes(&row, &key), 62_880);
        Ok(())
    }

    #[test]
    fn kv_baselines_do_not_cross_engine_versions() -> Result<()> {
        let baseline = kv_row("none", 100.0, 20_000, 1_000_000, 100);
        let mut candidate = kv_row("lz4", 125.0, 25_000, 500_000, 40);
        candidate["engine_version"] = json!("0.7.0");
        let groups = summarize(&[baseline, candidate])?;
        let candidate = groups
            .iter()
            .find(|item| item.engine_compression.as_deref() == Some("lz4"))
            .context("missing lz4 group")?;
        assert!(candidate.vs_none.is_none());
        Ok(())
    }

    #[test]
    fn record_markdown_uses_allocated_bytes_for_non_zfs_physical_ratio() -> Result<()> {
        let mut baseline = record_row("hex-random", 1_000_000);
        baseline["compression_storage"]["footprint"]["allocated_block_bytes"] = json!(800_000);
        let mut compressed = record_row("zeros", 250_000);
        compressed["compression_storage"]["footprint"]["allocated_block_bytes"] = json!(400_000);
        let groups = summarize(&[baseline, compressed])?;
        let rendered = super::markdown(&groups, "record-compression");
        let zeros = rendered
            .lines()
            .find(|line| line.contains("| sqlite | point-read | 512 | zeros |"))
            .context("missing zeros markdown row")?;
        assert!(zeros.ends_with("| 1.000 | 1.000 | 0.500 |"));
        Ok(())
    }

    #[test]
    fn record_patterns_compare_to_hex_random() -> Result<()> {
        let groups = summarize(&[
            record_row("hex-random", 1_000_000),
            record_row("zeros", 250_000),
        ])?;
        let zeros = groups
            .iter()
            .find(|item| item.payload_pattern.as_deref() == Some("zeros"))
            .context("missing zeros group")?;
        let ratios = zeros
            .vs_hex_random
            .as_ref()
            .context("missing hex-random baseline")?;
        assert_eq!(ratios.apparent_bytes_ratio, Some(0.25));
        Ok(())
    }
}
