use crate::file_hash::{lower_hex, sha256};
use crate::json_io::read_json;
use crate::json_value::json_truthy;
use crate::support::{canonical_identity_sha256, support_identity};
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};

mod audit;
mod kv;
mod kv_identity;
mod kv_materialize;
mod record;
mod record_calibration;

pub use audit::{AuditArgs, run_concurrency_sizing_audit};
pub use kv::{KvCalibrationArgs, KvFinalArgs, run_kv_calibration, run_kv_final};
pub use kv_materialize::{KvMaterializeArgs, run_materialize as run_kv_materialize};
pub use record::{RecordFinalArgs, run_record_final};
pub use record_calibration::{
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
        && value >= int_as_f64!(i64::MIN)
        && value <= int_as_f64!(i64::MAX)
    {
        return Some(f64_to_i64!(value));
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

fn object(value: &Value) -> Result<&Map<String, Value>> {
    value.as_object().context("expected JSON object")
}

fn clone_field(value: &Value, key: &str) -> Result<Value> {
    value
        .get(key)
        .cloned()
        .with_context(|| format!("missing {key}"))
}

fn rounded_ops(value: f64) -> Result<i64> {
    if !value.is_finite() || value <= 0.0 {
        bail!("invalid operation recommendation: {value}");
    }
    let exponent = f64_to_i32!(value.log10().floor());
    let scale = 10_f64.powi(exponent);
    let normalized = value / scale;
    for step in [1.0, 2.0, 5.0, 10.0] {
        if normalized <= step {
            let result = step * scale;
            if result > int_as_f64!(i64::MAX) {
                bail!("operation recommendation too large: {result}");
            }
            return Ok(f64_to_i64!(result).max(1));
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
        Ok(f64::midpoint(values[middle - 1], values[middle]))
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

fn load_calibration_cases<K: Ord + std::fmt::Debug>(
    plan: &Value,
    paths: &[PathBuf],
    key: impl Fn(&Value) -> Result<K>,
    validate: impl Fn(&Value, &Path) -> Result<()>,
) -> Result<BTreeMap<K, Value>> {
    let cases = plan
        .get("cases")
        .and_then(Value::as_array)
        .context("calibration plan cases must be an array")?;
    let mut planned = BTreeSet::new();
    for item in cases {
        if !planned.insert(key(item)?) {
            bail!("duplicate calibration plan identity");
        }
    }
    if i64::try_from(planned.len()).context("planned count overflow")?
        != get_i64(plan, "case_count")?
    {
        bail!("calibration plan case count mismatch");
    }
    let mut results = BTreeMap::new();
    for path in paths {
        let row = read_json(path)?;
        validate(&row, path)?;
        let identity = key(&row)?;
        if !planned.contains(&identity) {
            bail!(
                "calibration result outside plan: {}: {identity:?}",
                path.display()
            );
        }
        if get_i64(&row, "ops_completed")? != get_i64(&row, "ops_requested")? {
            bail!("partial calibration result: {}", path.display());
        }
        let elapsed = get_f64(&row, "elapsed_s")?;
        let rate = get_f64(&row, "ops_per_s")?;
        if !elapsed.is_finite() || elapsed <= 0.0 || !rate.is_finite() || rate <= 0.0 {
            bail!("invalid calibration timing: {}", path.display());
        }
        match results.entry(identity) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(row);
            }
            std::collections::btree_map::Entry::Occupied(entry) => {
                bail!("duplicate calibration result: {:?}", entry.key());
            }
        }
    }
    if planned.iter().ne(results.keys()) {
        bail!("calibration results do not exactly cover plan");
    }
    Ok(results)
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
    Ok(lower_hex(&digest.finalize()))
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
        "noise_guard_sha256": sha256(&repo.join("tools/benchctl/src/noise.rs"))?,
        "continuous_noise_guard_sha256": sha256(&repo.join("tools/benchctl/src/noise.rs"))?,
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
        "noise_guard_sha256": sha256(&repo.join("tools/benchctl/src/noise.rs"))?,
        "continuous_noise_guard_sha256": sha256(&repo.join("tools/benchctl/src/noise.rs"))?,
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

#[cfg(test)]
mod calibration_case_tests {
    use super::{TestDir, get_i64, load_calibration_cases, write_json};
    use anyhow::Result;
    use serde_json::json;

    #[test]
    fn calibration_requires_exact_complete_positive_results() -> Result<()> {
        let dir = TestDir::new("case-coverage")?;
        let first = dir.0.join("first.json");
        let second = dir.0.join("second.json");
        let plan = json!({"case_count": 2, "cases": [{"id": 1}, {"id": 2}]});
        let valid = json!({"id": 1, "ops_requested": 10, "ops_completed": 10,
                           "elapsed_s": 1.0, "ops_per_s": 10.0});
        write_json(&first, &valid)?;
        let load = |plan, paths: &[std::path::PathBuf]| {
            load_calibration_cases(plan, paths, |row| get_i64(row, "id"), |_, _| Ok(()))
        };
        assert!(load(&plan, std::slice::from_ref(&first)).is_err());
        assert!(load(&plan, &[first.clone(), first.clone()]).is_err());
        let paths = [first, second.clone()];
        for (key, value) in [
            ("id", json!(3)),
            ("ops_completed", json!(9)),
            ("elapsed_s", json!(0.0)),
            ("ops_per_s", json!(-1.0)),
        ] {
            let mut invalid = valid.clone();
            invalid["id"] = json!(2);
            invalid[key] = value;
            write_json(&second, &invalid)?;
            assert!(load(&plan, &paths).is_err());
        }
        let mut valid_second = valid;
        valid_second["id"] = json!(2);
        write_json(&second, &valid_second)?;
        assert_eq!(load(&plan, &paths)?.len(), 2);
        let duplicate = json!({"case_count": 2, "cases": [{"id": 1}, {"id": 1}]});
        assert!(load(&duplicate, &paths).is_err());
        let wrong_count = json!({"case_count": 3, "cases": [{"id": 1}, {"id": 2}]});
        assert!(load(&wrong_count, &paths).is_err());
        Ok(())
    }
}
