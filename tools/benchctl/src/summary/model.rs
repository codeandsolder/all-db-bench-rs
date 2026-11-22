use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(super) struct GroupKey {
    pub(super) format_version: u64,
    pub(super) lane: String,
    pub(super) scenario: String,
    pub(super) engine: String,
    pub(super) engine_version: String,
    pub(super) durability: String,
    pub(super) workload: String,
    pub(super) records: u64,
    pub(super) ops_requested: u64,
    pub(super) clients: u64,
    pub(super) value_bytes: Option<u64>,
    pub(super) value_pattern: String,
    pub(super) payload_pattern: String,
    pub(super) engine_compression: String,
    pub(super) key_bytes: u64,
    pub(super) key_shape: String,
    pub(super) access_pattern: String,
    pub(super) miss_percent: u64,
    pub(super) write_pattern: String,
    pub(super) state_evolution: String,
    pub(super) bounded_churn_slots: u64,
    pub(super) txn_size: u64,
    pub(super) read_materialization: String,
    pub(super) write_materialization: String,
    pub(super) scan_len: Option<u64>,
    pub(super) settle_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ThroughputMetrics {
    #[serde(rename = "ops_per_s_median")]
    pub(super) median: f64,
    #[serde(rename = "ops_per_s_q1")]
    pub(super) q1: f64,
    #[serde(rename = "ops_per_s_q3")]
    pub(super) q3: f64,
    #[serde(rename = "ops_per_s_min")]
    pub(super) min: f64,
    #[serde(rename = "ops_per_s_max")]
    pub(super) max: f64,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ProcessMetrics {
    #[serde(rename = "cpu_ns_per_op_median")]
    pub(super) cpu_ns_per_op: Option<f64>,
    #[serde(rename = "cpu_cores_median")]
    pub(super) cpu_cores: Option<f64>,
    #[serde(rename = "runqueue_wait_fraction_median")]
    pub(super) runqueue_wait_fraction: Option<f64>,
    #[serde(rename = "read_bytes_per_op_median")]
    pub(super) read_bytes_per_op: Option<f64>,
    #[serde(rename = "write_bytes_per_op_median")]
    pub(super) write_bytes_per_op: Option<f64>,
    #[serde(rename = "major_faults_per_kop_median")]
    pub(super) major_faults_per_kop: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct SystemMetrics {
    #[serde(rename = "cpu_psi_some_fraction_median")]
    pub(super) cpu_psi_some_fraction: Option<f64>,
    #[serde(rename = "io_psi_full_fraction_median")]
    pub(super) io_psi_full_fraction: Option<f64>,
    #[serde(rename = "swap_activity_pages_median")]
    pub(super) swap_activity_pages: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct SettleMetrics {
    #[serde(rename = "settle_cpu_ns_median")]
    pub(super) cpu_ns: Option<f64>,
    #[serde(rename = "settle_write_bytes_median")]
    pub(super) write_bytes: Option<f64>,
    #[serde(rename = "settle_db_delta_bytes_median")]
    pub(super) db_delta_bytes: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct SummaryGroup {
    #[serde(flatten)]
    pub(super) key: GroupKey,
    pub(super) engine_compression_mapping: Vec<String>,
    pub(super) configuration: String,
    pub(super) trials: Vec<u64>,
    #[serde(flatten)]
    pub(super) throughput: ThroughputMetrics,
    pub(super) p99_read_us_median: Option<f64>,
    pub(super) p99_operation_us_median: Option<f64>,
    pub(super) p99_write_txn_us_median: Option<f64>,
    pub(super) db_bytes_median: f64,
    pub(super) peak_rss_kib_median: Option<f64>,
    pub(super) open_s_median: Option<f64>,
    pub(super) prefill_s_median: Option<f64>,
    pub(super) client_setup_s_median: Option<f64>,
    #[serde(flatten)]
    pub(super) process: ProcessMetrics,
    pub(super) client_throughput_max_min_ratio_median: Option<f64>,
    pub(super) write_conflict_retries_median: Option<f64>,
    pub(super) write_conflict_retries_per_k_write_ops_median: Option<f64>,
    #[serde(flatten)]
    pub(super) system: SystemMetrics,
    #[serde(flatten)]
    pub(super) settle: SettleMetrics,
    pub(super) durability_mapping: Vec<String>,
    pub(super) speedup_vs_c1: Option<f64>,
    pub(super) parallel_efficiency_vs_c1: Option<f64>,
    pub(super) p99_read_multiplier_vs_c1: Option<f64>,
    pub(super) p99_write_txn_multiplier_vs_c1: Option<f64>,
}

fn required<'a>(row: &'a Value, key: &str) -> Result<&'a Value> {
    row.get(key)
        .with_context(|| format!("missing required field {key}"))
}

pub(super) fn required_str(row: &Value, key: &str) -> Result<String> {
    required(row, key)?
        .as_str()
        .map(ToOwned::to_owned)
        .with_context(|| format!("field {key} is not a string"))
}

pub(super) fn required_u64(row: &Value, key: &str) -> Result<u64> {
    required(row, key)?
        .as_u64()
        .with_context(|| format!("field {key} is not an unsigned integer"))
}

pub(super) fn required_f64(row: &Value, key: &str) -> Result<f64> {
    required(row, key)?
        .as_f64()
        .filter(|value| value.is_finite())
        .with_context(|| format!("field {key} is not a finite number"))
}

pub(super) fn optional_str(row: &Value, key: &str, default: &str) -> String {
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

pub(super) fn nested_u64(row: &Value, path: &[&str]) -> Option<u64> {
    nested(row, path).and_then(Value::as_u64)
}

pub(super) fn nested_f64(row: &Value, path: &[&str]) -> Option<f64> {
    nested(row, path)
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
}

fn value_bytes(row: &Value) -> Option<u64> {
    row.get("value_bytes")
        .and_then(Value::as_u64)
        .or_else(|| row.get("payload_bytes").and_then(Value::as_u64))
}

pub(super) fn group_key(row: &Value) -> Result<GroupKey> {
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

impl GroupKey {
    pub(super) fn single_client(&self) -> Self {
        let mut key = self.clone();
        key.clients = 1;
        key
    }
}

pub(super) fn configuration(key: &GroupKey) -> String {
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
