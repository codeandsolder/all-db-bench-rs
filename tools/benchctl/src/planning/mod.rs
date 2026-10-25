use crate::support::{canonical_identity_sha256, sha256, support_identity};
use anyhow::{Context, Result, bail};
use serde_json::{Map, Number, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};

mod audit;
mod kv;
mod kv_materialize;
mod record;
mod record_calibration;

pub(crate) use audit::{AuditArgs, run_concurrency_sizing_audit};
pub(crate) use kv::{KvCalibrationArgs, KvFinalArgs, run_kv_calibration, run_kv_final};
pub(crate) use kv_materialize::{KvMaterializeArgs, run_materialize as run_kv_materialize};
pub(crate) use record::{RecordFinalArgs, run_record_final};
pub(crate) use record_calibration::{
    MaterializeArgs, RecordCalibrationArgs, run_calibration as run_record_calibration,
    run_materialize as run_record_materialize,
};

const TARGET_SECONDS: f64 = 3.0;
const KV_PLAN_VERSION: i64 = 2;
const RECORD_PLAN_VERSION: i64 = 2;
const DEFAULT_BOUNDED_CHURN_SLOTS: i64 = 8192;
const RECORD_MAX_SLOW_SECONDS: f64 = 15.0;
const READ_MATERIALIZATION: &str = "full-record-v1";
const WRITE_MATERIALIZATION: &str = "no-return-v1";

fn read_json(path: &Path) -> Result<Value> {
    serde_json::from_str(
        &fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?,
    )
    .with_context(|| format!("invalid JSON in {}", path.display()))
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(path, serde_json::to_string_pretty(value)? + "\n")
        .with_context(|| format!("failed to write {}", path.display()))
}

fn get_str<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("missing or invalid {key}"))
}

fn value_i64(value: &Value) -> Option<i64> {
    if let Some(value) = value.as_i64() {
        return Some(value);
    }
    if let Some(value) = value.as_u64() {
        return i64::try_from(value).ok();
    }
    if let Some(value) = value.as_f64()
        && value.is_finite()
        && value.fract() == 0.0
        && value >= i64::MIN as f64
        && value <= i64::MAX as f64
    {
        return Some(value as i64);
    }
    value.as_str()?.parse::<i64>().ok()
}

fn get_i64(value: &Value, key: &str) -> Result<i64> {
    let raw = value.get(key).with_context(|| format!("missing {key}"))?;
    value_i64(raw).with_context(|| format!("invalid {key}: {raw}"))
}

fn get_f64(value: &Value, key: &str) -> Result<f64> {
    let raw = value.get(key).with_context(|| format!("missing {key}"))?;
    let parsed = if let Some(value) = raw.as_f64() {
        value
    } else if let Some(value) = raw.as_str() {
        value
            .parse::<f64>()
            .with_context(|| format!("invalid {key}: {raw}"))?
    } else {
        bail!("invalid {key}: {raw}");
    };
    Ok(parsed)
}

fn json_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(value)) => value.as_f64().is_some_and(|number| number != 0.0),
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Array(value)) => !value.is_empty(),
        Some(Value::Object(value)) => !value.is_empty(),
    }
}

fn object(value: &Value) -> Result<&Map<String, Value>> {
    value.as_object().context("expected JSON object")
}

fn clone_field(value: &Value, key: &str) -> Result<Value> {
    value
        .get(key)
        .cloned()
        .with_context(|| format!("missing {key}"))
}

fn number(value: f64) -> Result<Value> {
    Number::from_f64(value)
        .map(Value::Number)
        .with_context(|| format!("cannot encode non-finite float {value}"))
}

fn rounded_ops(value: f64) -> Result<i64> {
    if !value.is_finite() || value <= 0.0 {
        bail!("invalid operation recommendation: {value}");
    }
    let exponent = value.log10().floor() as i32;
    let scale = 10_f64.powi(exponent);
    let normalized = value / scale;
    for step in [1.0, 2.0, 5.0, 10.0] {
        if normalized <= step {
            let result = step * scale;
            if result > i64::MAX as f64 {
                bail!("operation recommendation too large: {result}");
            }
            return Ok((result as i64).max(1));
        }
    }
    bail!("unable to round operation recommendation: {value}")
}

fn median(values: &mut [f64]) -> Result<f64> {
    if values.is_empty() {
        bail!("median of empty sequence");
    }
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        Ok((values[middle - 1] + values[middle]) / 2.0)
    } else {
        Ok(values[middle])
    }
}

fn case_paths(run_dir: &Path) -> Result<Vec<PathBuf>> {
    let case_dir = run_dir.join("cases");
    let mut paths = Vec::new();
    if case_dir.is_dir() {
        for entry in fs::read_dir(&case_dir)
            .with_context(|| format!("failed to list {}", case_dir.display()))?
        {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) == Some("json") {
                paths.push(path);
            }
        }
    }
    paths.sort_by(|left, right| left.file_name().cmp(&right.file_name()));
    Ok(paths)
}

fn case_set_sha256(paths: &[PathBuf]) -> Result<String> {
    let mut sorted = paths.to_vec();
    sorted.sort_by(|left, right| left.file_name().cmp(&right.file_name()));
    let mut digest = Sha256::new();
    for path in sorted {
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .with_context(|| format!("non-UTF-8 case filename: {}", path.display()))?;
        digest.update(name.as_bytes());
        digest.update([0]);
        let mut file =
            File::open(&path).with_context(|| format!("failed to open case {}", path.display()))?;
        let mut file_digest = Sha256::new();
        let mut buffer = vec![0_u8; 1024 * 1024];
        loop {
            let count = file
                .read(&mut buffer)
                .with_context(|| format!("failed to read case {}", path.display()))?;
            if count == 0 {
                break;
            }
            file_digest.update(&buffer[..count]);
        }
        digest.update(file_digest.finalize());
    }
    Ok(format!("{:x}", digest.finalize()))
}

#[cfg(test)]
fn repo() -> Result<PathBuf> {
    Ok(Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?)
}

#[cfg(test)]
fn kv_support(repo: &Path) -> Result<Value> {
    use crate::support::{CONTINUOUS_ADMISSION, git_head};
    use serde_json::json;

    Ok(json!({
        "lane": "kv-concurrency",
        "profile": "quick",
        "trials": 3,
        "records": 100_000,
        "default_ops": 50_000,
        "admission_policy": CONTINUOUS_ADMISSION,
        "runner_sha256": sha256(&repo.join("scripts/run-kv-concurrency-plan.sh"))?,
        "concurrency_runner_common_sha256": sha256(&repo.join("scripts/concurrency-runner-common.sh"))?,
        "continuous_noise_common_sha256": sha256(&repo.join("scripts/continuous-noise-runner-common.sh"))?,
        "concurrency_policy_sha256": sha256(&repo.join("scripts/concurrency-matrix-policy.sh"))?,
        "noise_guard_sha256": sha256(&repo.join("scripts/check-external-noise.py"))?,
        "continuous_noise_guard_sha256": sha256(&repo.join("scripts/run-with-continuous-noise.py"))?,
        "benchmark_binary_sha256": "a".repeat(64),
        "build_profile": "external",
        "hostname": "test-host",
        "machine_id_sha256": "b".repeat(64),
        "filesystem": "zfs",
        "source": "testpool/scratch",
        "benchmark_source_commit": "c".repeat(40),
        "harness_commit": git_head(repo)?,
        "continuous_noise_sample_ms": 250,
        "continuous_noise_max_cpu_percent": 50,
        "continuous_noise_max_io_average_mib_s": 2,
        "continuous_noise_max_io_rate_mib_s": 8,
        "initial_min_free_gib": 10,
        "case_min_free_gib": "10",
        "case_timeout_s": 600,
        "persy_lock_timeout_ms": 250,
        "prepared_db_protocol": "case-private-clean-close-v1",
    }))
}

#[cfg(test)]
fn record_support(repo: &Path) -> Result<Value> {
    use crate::support::{CONTINUOUS_ADMISSION, git_head};
    use serde_json::json;

    Ok(json!({
        "lane": "record-concurrency",
        "profile": "quick",
        "admission_policy": CONTINUOUS_ADMISSION,
        "runner_sha256": sha256(&repo.join("scripts/run-record-concurrency-plan.sh"))?,
        "concurrency_runner_common_sha256": sha256(&repo.join("scripts/concurrency-runner-common.sh"))?,
        "continuous_noise_common_sha256": sha256(&repo.join("scripts/continuous-noise-runner-common.sh"))?,
        "concurrency_policy_sha256": sha256(&repo.join("scripts/concurrency-matrix-policy.sh"))?,
        "noise_guard_sha256": sha256(&repo.join("scripts/check-external-noise.py"))?,
        "continuous_noise_guard_sha256": sha256(&repo.join("scripts/run-with-continuous-noise.py"))?,
        "benchmark_binary_sha256": "a".repeat(64),
        "build_profile": "external",
        "surrealdb_rocksdb_binary_sha256": "b".repeat(64),
        "rocks_build_profile": "external",
        "hostname": "test-host",
        "machine_id_sha256": "c".repeat(64),
        "filesystem": "zfs",
        "source": "testpool/scratch",
        "benchmark_source_commit": "d".repeat(40),
        "surrealdb_rocksdb_source_commit": "d".repeat(40),
        "harness_commit": git_head(repo)?,
        "continuous_noise_sample_ms": 250,
        "continuous_noise_max_cpu_percent": 50,
        "continuous_noise_max_io_average_mib_s": 2,
        "continuous_noise_max_io_rate_mib_s": 8,
        "initial_min_free_gib": 10,
        "case_min_free_gib": "10",
        "case_timeout_s": 600,
        "read_materialization": READ_MATERIALIZATION,
        "write_materialization": WRITE_MATERIALIZATION,
    }))
}

#[cfg(test)]
struct TestDir(PathBuf);

#[cfg(test)]
impl TestDir {
    fn new(label: &str) -> Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "all-db-benchctl-planning-{label}-{}-{counter}",
            std::process::id()
        ));
        if path.exists() {
            fs::remove_dir_all(&path)?;
        }
        fs::create_dir_all(&path)?;
        Ok(Self(path))
    }
}

#[cfg(test)]
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
