use crate::file_hash::sha256;
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Read},
    path::Path,
    process::Command,
};

pub(crate) const CONTINUOUS_ADMISSION: &str = "pre-io+pre/continuous/post-external-v3";
const EXPECTED_CONTINUOUS: [(&str, f64); 4] = [
    ("continuous_noise_sample_ms", 250.0),
    ("continuous_noise_max_cpu_percent", 50.0),
    ("continuous_noise_max_io_average_mib_s", 2.0),
    ("continuous_noise_max_io_rate_mib_s", 8.0),
];

fn required<'a>(support: &'a Value, key: &str, label: &str) -> Result<&'a Value> {
    let value = support
        .get(key)
        .with_context(|| format!("missing {key} in {label}"))?;
    if value.is_null() || value.as_str().is_some_and(str::is_empty) {
        bail!("missing {key} in {label}");
    }
    Ok(value)
}

fn scalar_string(value: &Value, key: &str, label: &str) -> Result<String> {
    if let Some(value) = value.as_str() {
        return Ok(value.to_owned());
    }
    if value.is_number() || value.is_boolean() {
        return Ok(value.to_string());
    }
    bail!("invalid {key} in {label}: expected scalar")
}

fn positive(value: &Value, key: &str, label: &str) -> Result<f64> {
    let parsed = if let Some(value) = value.as_f64() {
        value
    } else if let Some(value) = value.as_str() {
        value
            .parse::<f64>()
            .with_context(|| format!("invalid {key} in {label}: {value:?}"))?
    } else {
        bail!("invalid {key} in {label}: {value}");
    };
    if !parsed.is_finite() || parsed <= 0.0 {
        bail!("non-positive {key} in {label}: {value}");
    }
    Ok(parsed)
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn git_head(repo: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()
        .with_context(|| format!("failed to run git in {}", repo.display()))?;
    if !output.status.success() {
        bail!(
            "git rev-parse HEAD failed in {}: {}",
            repo.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8(output.stdout)
        .context("git HEAD was not UTF-8")?
        .trim()
        .to_owned())
}

fn insert_text(
    identity: &mut Map<String, Value>,
    support: &Value,
    key: &str,
    label: &str,
) -> Result<()> {
    identity.insert(
        key.to_owned(),
        json!(scalar_string(required(support, key, label)?, key, label)?),
    );
    Ok(())
}

fn insert_positive(
    identity: &mut Map<String, Value>,
    support: &Value,
    key: &str,
    label: &str,
) -> Result<()> {
    identity.insert(
        key.to_owned(),
        json!(positive(required(support, key, label)?, key, label)?),
    );
    Ok(())
}

pub(crate) fn support_identity(
    support: &Value,
    expected_lane: &str,
    repo: Option<&Path>,
    runner_name: Option<&str>,
    expected_admission: &str,
    label: &str,
) -> Result<Value> {
    if support.get("lane").and_then(Value::as_str) != Some(expected_lane) {
        bail!(
            "unexpected lane in {label}: {:?}",
            support.get("lane").unwrap_or(&Value::Null)
        );
    }
    if support.get("profile").and_then(Value::as_str) != Some("quick") {
        bail!(
            "unexpected profile in {label}: {:?}",
            support.get("profile").unwrap_or(&Value::Null)
        );
    }
    if support.get("admission_policy").and_then(Value::as_str) != Some(expected_admission) {
        bail!(
            "unexpected admission policy in {label}: {:?}; expected {expected_admission:?}",
            support.get("admission_policy").unwrap_or(&Value::Null)
        );
    }

    let mut identity = Map::new();
    identity.insert("lane".to_owned(), json!(expected_lane));
    identity.insert("profile".to_owned(), json!("quick"));
    identity.insert("admission_policy".to_owned(), json!(expected_admission));

    for key in [
        "runner_sha256",
        "concurrency_runner_common_sha256",
        "continuous_noise_common_sha256",
        "concurrency_policy_sha256",
        "noise_guard_sha256",
        "continuous_noise_guard_sha256",
        "benchmark_binary_sha256",
        "build_profile",
        "hostname",
        "machine_id_sha256",
        "filesystem",
        "source",
        "benchmark_source_commit",
        "harness_commit",
    ] {
        insert_text(&mut identity, support, key, label)?;
    }

    for key in [
        "runner_sha256",
        "concurrency_runner_common_sha256",
        "continuous_noise_common_sha256",
        "concurrency_policy_sha256",
        "noise_guard_sha256",
        "continuous_noise_guard_sha256",
        "benchmark_binary_sha256",
        "machine_id_sha256",
    ] {
        let value = identity[key]
            .as_str()
            .with_context(|| format!("internal non-string identity field {key}"))?;
        if !is_lower_hex(value, 64) {
            bail!("malformed {key} in {label}: {value:?}");
        }
    }
    for key in ["benchmark_source_commit", "harness_commit"] {
        let value = identity[key]
            .as_str()
            .with_context(|| format!("internal non-string identity field {key}"))?;
        if !is_lower_hex(value, 40) {
            bail!("malformed {key} in {label}: {value:?}");
        }
    }
    if identity["build_profile"].as_str() != Some("external") {
        bail!(
            "unexpected build_profile in {label}: {:?}",
            identity["build_profile"]
        );
    }

    for key in [
        "continuous_noise_sample_ms",
        "continuous_noise_max_cpu_percent",
        "continuous_noise_max_io_average_mib_s",
        "continuous_noise_max_io_rate_mib_s",
        "initial_min_free_gib",
        "case_min_free_gib",
        "case_timeout_s",
    ] {
        insert_positive(&mut identity, support, key, label)?;
    }
    for (key, expected) in EXPECTED_CONTINUOUS {
        if identity[key].as_f64() != Some(expected) {
            bail!(
                "unexpected {key} in {label}: {:?} != {expected}",
                identity[key]
            );
        }
    }
    if identity["initial_min_free_gib"].as_f64().unwrap_or(0.0) < 10.0
        || identity["case_min_free_gib"].as_f64().unwrap_or(0.0) < 10.0
    {
        bail!("concurrency free-space floor below 10 GiB in {label}");
    }

    match expected_lane {
        "kv-concurrency" => {
            insert_positive(&mut identity, support, "persy_lock_timeout_ms", label)?;
            if identity["persy_lock_timeout_ms"].as_f64() != Some(250.0) {
                bail!("unexpected persy_lock_timeout_ms in {label}");
            }
            let protocol = scalar_string(
                required(support, "prepared_db_protocol", label)?,
                "prepared_db_protocol",
                label,
            )?;
            if protocol != "case-private-clean-close-v1" {
                bail!("unexpected prepared_db_protocol in {label}: {protocol:?}");
            }
            identity.insert("prepared_db_protocol".to_owned(), json!(protocol));
        }
        "record-concurrency" => {
            for key in [
                "surrealdb_rocksdb_binary_sha256",
                "surrealdb_rocksdb_source_commit",
                "rocks_build_profile",
                "read_materialization",
                "write_materialization",
            ] {
                insert_text(&mut identity, support, key, label)?;
            }
            if !is_lower_hex(
                identity["surrealdb_rocksdb_binary_sha256"]
                    .as_str()
                    .context("internal rocks binary identity type")?,
                64,
            ) {
                bail!("malformed surrealdb_rocksdb_binary_sha256 in {label}");
            }
            if !is_lower_hex(
                identity["surrealdb_rocksdb_source_commit"]
                    .as_str()
                    .context("internal rocks source identity type")?,
                40,
            ) {
                bail!("malformed surrealdb_rocksdb_source_commit in {label}");
            }
            if identity["rocks_build_profile"].as_str() != Some("external") {
                bail!("unexpected rocks_build_profile in {label}");
            }
            if identity["read_materialization"].as_str() != Some("full-record-v1") {
                bail!("unexpected read_materialization in {label}");
            }
            if identity["write_materialization"].as_str() != Some("no-return-v1") {
                bail!("unexpected write_materialization in {label}");
            }
        }
        _ => bail!("unsupported concurrency lane: {expected_lane:?}"),
    }

    if let Some(repo) = repo {
        let runner_name = runner_name.context("runner_name is required when repo is provided")?;
        for (key, path) in [
            ("runner_sha256", repo.join("scripts").join(runner_name)),
            (
                "concurrency_runner_common_sha256",
                repo.join("scripts/concurrency-runner-common.sh"),
            ),
            (
                "continuous_noise_common_sha256",
                repo.join("scripts/continuous-noise-runner-common.sh"),
            ),
            (
                "concurrency_policy_sha256",
                repo.join("scripts/concurrency-matrix-policy.sh"),
            ),
            (
                "noise_guard_sha256",
                repo.join("tools/benchctl/src/noise.rs"),
            ),
            (
                "continuous_noise_guard_sha256",
                repo.join("tools/benchctl/src/noise.rs"),
            ),
        ] {
            let actual = sha256(&path)?;
            if identity[key].as_str() != Some(actual.as_str()) {
                bail!(
                    "runtime provenance mismatch for {key} in {label}: {:?} != {actual}",
                    identity[key]
                );
            }
        }
        let head = git_head(repo)?;
        if identity["harness_commit"].as_str() != Some(head.as_str()) {
            bail!(
                "runtime provenance mismatch for harness_commit in {label}: {:?} != {head}",
                identity["harness_commit"]
            );
        }
    }

    Ok(Value::Object(identity))
}

pub(crate) fn canonical_identity_sha256(identity: &Value) -> Result<String> {
    let encoded = serde_json::to_vec(identity)?;
    let mut digest = Sha256::new();
    digest.update(encoded);
    Ok(format!("{:x}", digest.finalize()))
}

fn read_json(path: &str) -> Result<Value> {
    if path != "-" {
        return crate::json_io::read_json(Path::new(path));
    }
    let mut text = String::new();
    io::stdin().read_to_string(&mut text)?;
    serde_json::from_str(&text).context("invalid JSON in -")
}

pub(crate) fn verify_plan_identity(
    plan: &Value,
    identity: &Value,
    actual_digest: &str,
) -> Result<()> {
    if let Some(expected) = plan.get("expected_measurement_identity")
        && !expected.is_null()
        && expected != identity
    {
        let expected_digest = canonical_identity_sha256(expected)?;
        bail!(
            "concurrency measurement identity differs from plan expectation: actual={actual_digest} expected={expected_digest}"
        );
    }
    Ok(())
}

pub(crate) fn run_support_identity(
    support_path: &str,
    expected_lane: &str,
    repo: Option<&Path>,
    runner_name: Option<&str>,
    expected_admission: &str,
    plan: Option<&Path>,
) -> Result<()> {
    let support = read_json(support_path)?;
    let identity = support_identity(
        &support,
        expected_lane,
        repo,
        runner_name,
        expected_admission,
        support_path,
    )?;
    let digest = canonical_identity_sha256(&identity)?;

    if let Some(plan) = plan {
        let plan_value: Value = serde_json::from_str(
            &fs::read_to_string(plan)
                .with_context(|| format!("failed to read {}", plan.display()))?,
        )
        .with_context(|| format!("invalid JSON in {}", plan.display()))?;
        verify_plan_identity(&plan_value, &identity, &digest)?;
    }

    println!(
        "{}",
        serde_json::to_string(&json!({
            "measurement_identity": identity,
            "measurement_identity_sha256": digest,
        }))?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        CONTINUOUS_ADMISSION, canonical_identity_sha256, git_head, sha256, support_identity,
        verify_plan_identity,
    };
    use anyhow::{Context, Result};
    use serde_json::{Value, json};
    use std::path::{Path, PathBuf};

    fn repo() -> Result<PathBuf> {
        Ok(Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()?)
    }

    fn kv_support() -> Result<Value> {
        let repo = repo()?;
        Ok(json!({
            "lane": "kv-concurrency",
            "profile": "quick",
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
            "harness_commit": git_head(&repo)?,
            "continuous_noise_sample_ms": 250,
            "continuous_noise_max_cpu_percent": 50,
            "continuous_noise_max_io_average_mib_s": 2,
            "continuous_noise_max_io_rate_mib_s": 8,
            "initial_min_free_gib": 10,
            "case_min_free_gib": "10",
            "case_timeout_s": 600,
            "persy_lock_timeout_ms": 250,
            "prepared_db_protocol": "case-private-clean-close-v1"
        }))
    }

    #[test]
    fn kv_support_accepts_exact_runtime_identity() -> Result<()> {
        let repo = repo()?;
        let identity = support_identity(
            &kv_support()?,
            "kv-concurrency",
            Some(&repo),
            Some("run-kv-concurrency-plan.sh"),
            CONTINUOUS_ADMISSION,
            "test",
        )?;
        assert_eq!(identity["hostname"], "test-host");
        assert_eq!(identity["case_min_free_gib"].as_f64(), Some(10.0));
        assert_eq!(canonical_identity_sha256(&identity)?.len(), 64);
        Ok(())
    }

    #[test]
    fn support_rejects_runtime_helper_hash_drift() -> Result<()> {
        let repo = repo()?;
        let mut support = kv_support()?;
        support["continuous_noise_common_sha256"] = json!("f".repeat(64));
        let error = support_identity(
            &support,
            "kv-concurrency",
            Some(&repo),
            Some("run-kv-concurrency-plan.sh"),
            CONTINUOUS_ADMISSION,
            "test",
        )
        .err()
        .context("hash drift must fail")?;
        assert!(error.to_string().contains("runtime provenance mismatch"));
        Ok(())
    }

    #[test]
    fn record_support_accepts_semantic_materialization_identity() -> Result<()> {
        let mut support = kv_support()?;
        support["lane"] = json!("record-concurrency");
        support
            .as_object_mut()
            .context("support object")?
            .remove("persy_lock_timeout_ms");
        support
            .as_object_mut()
            .context("support object")?
            .remove("prepared_db_protocol");
        support["surrealdb_rocksdb_binary_sha256"] = json!("d".repeat(64));
        support["surrealdb_rocksdb_source_commit"] = json!("e".repeat(40));
        support["rocks_build_profile"] = json!("external");
        support["read_materialization"] = json!("full-record-v1");
        support["write_materialization"] = json!("no-return-v1");

        let identity = support_identity(
            &support,
            "record-concurrency",
            None,
            None,
            CONTINUOUS_ADMISSION,
            "test",
        )?;
        assert_eq!(identity["read_materialization"], "full-record-v1");
        assert_eq!(identity["write_materialization"], "no-return-v1");
        assert_eq!(identity["initial_min_free_gib"].as_f64(), Some(10.0));
        Ok(())
    }

    #[test]
    fn plan_expected_identity_mismatch_is_rejected() -> Result<()> {
        let identity = support_identity(
            &kv_support()?,
            "kv-concurrency",
            None,
            None,
            CONTINUOUS_ADMISSION,
            "test",
        )?;
        let digest = canonical_identity_sha256(&identity)?;
        verify_plan_identity(
            &json!({"expected_measurement_identity": identity.clone()}),
            &identity,
            &digest,
        )?;

        let mut expected = identity.clone();
        expected["hostname"] = json!("different-host");
        let error = verify_plan_identity(
            &json!({"expected_measurement_identity": expected}),
            &identity,
            &digest,
        )
        .err()
        .context("identity mismatch must fail")?;
        assert!(error.to_string().contains("measurement identity differs"));
        Ok(())
    }

    #[test]
    fn support_rejects_wrong_lane_and_floor() -> Result<()> {
        let support = kv_support()?;
        assert!(
            support_identity(
                &support,
                "record-concurrency",
                None,
                None,
                CONTINUOUS_ADMISSION,
                "test"
            )
            .is_err()
        );
        let mut support = support;
        support["case_min_free_gib"] = json!(9);
        assert!(
            support_identity(
                &support,
                "kv-concurrency",
                None,
                None,
                CONTINUOUS_ADMISSION,
                "test"
            )
            .is_err()
        );
        Ok(())
    }
}
