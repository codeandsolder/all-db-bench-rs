use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

pub(crate) const CONTINUOUS_ADMISSION_POLICY: &str = "pre-io+pre/continuous/post-external-v3";

pub(crate) struct ReadyArgs<'a> {
    pub(crate) selected: &'a Path,
    pub(crate) binary_manifest: &'a Path,
    pub(crate) expected_commit: &'a str,
    pub(crate) repo: &'a Path,
    pub(crate) stock_run_id: &'a str,
    pub(crate) expected_admission_policy: &'a str,
    pub(crate) out: &'a Path,
}

fn read_json(path: &Path) -> Result<Value> {
    serde_json::from_str(
        &fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?,
    )
    .with_context(|| format!("invalid JSON in {}", path.display()))
}

fn read_rows(path: &Path) -> Result<Vec<Value>> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            serde_json::from_str(line)
                .with_context(|| format!("invalid JSON at {}:{}", path.display(), index + 1))
        })
        .collect()
}

fn value_i64(value: Option<&Value>) -> Result<i64> {
    match value.context("missing integer value")? {
        Value::Number(number) => number.as_i64().context("integer outside i64 range"),
        Value::String(raw) => raw
            .parse::<i64>()
            .with_context(|| format!("invalid integer {raw:?}")),
        other => bail!("expected integer-compatible value, got {other}"),
    }
}

fn value_f64(value: Option<&Value>) -> Result<f64> {
    match value.context("missing numeric value")? {
        Value::Number(number) => number.as_f64().context("number outside f64 range"),
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

fn sha256(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".tmp");
    PathBuf::from(name)
}

fn write_ready(path: &Path, ready: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let tmp = tmp_path(path);
    fs::write(&tmp, serde_json::to_string_pretty(ready)? + "\n")
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("failed to replace {}", path.display()))?;
    println!("{}", serde_json::to_string(ready)?);
    Ok(())
}

fn checked_runner_commit(repo: &Path) -> Result<String> {
    let status = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .current_dir(repo)
        .output()
        .with_context(|| format!("failed to inspect git status in {}", repo.display()))?;
    if !status.status.success() {
        bail!("git status failed in {}", repo.display());
    }
    if !String::from_utf8(status.stdout)?.trim().is_empty() {
        bail!("runner checkout has tracked modifications; refusing readiness certificate");
    }
    let commit = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()
        .with_context(|| format!("failed to inspect git commit in {}", repo.display()))?;
    if !commit.status.success() {
        bail!("git rev-parse failed in {}", repo.display());
    }
    Ok(String::from_utf8(commit.stdout)?.trim().to_owned())
}

fn selected_run_ids(manifest: &Value, stock_run_id: &str) -> Result<Vec<String>> {
    let selected_stock_groups = value_i64(manifest.get("selected_stock_groups")).unwrap_or(0);
    let mut selected = BTreeSet::new();
    if selected_stock_groups > 0 {
        selected.insert(stock_run_id.to_owned());
    }
    for item in manifest
        .get("sources")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let source = item.get("source").and_then(Value::as_str);
        if matches!(source, Some("resize" | "quality-repair"))
            && let Some(run_id) = item
                .get("run_id")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
        {
            selected.insert(run_id.to_owned());
        }
    }
    let expected = (i64::from(selected_stock_groups > 0))
        + value_i64(manifest.get("selected_resize_groups")).unwrap_or(0)
        + value_i64(manifest.get("selected_quality_repair_groups")).unwrap_or(0);
    if selected.len() as i64 != expected {
        bail!(
            "selected source-run provenance mismatch: manifest={expected} runs={}",
            selected.len()
        );
    }
    Ok(selected.into_iter().collect())
}

fn support_path(repo: &Path, run_id: &str) -> PathBuf {
    repo.join("results")
        .join("runs")
        .join(run_id)
        .join("support.json")
}

fn kv_source_identity(repo: &Path, support: &Value) -> Result<Value> {
    let files = [
        ("runner_sha256", repo.join("scripts/run-kv-matrix.sh")),
        (
            "kv_matrix_policy_sha256",
            repo.join("scripts/kv-matrix-policy.sh"),
        ),
        (
            "performance_common_sha256",
            repo.join("scripts/performance-runner-common.sh"),
        ),
        (
            "continuous_noise_common_sha256",
            repo.join("scripts/continuous-noise-runner-common.sh"),
        ),
        (
            "noise_guard_sha256",
            repo.join("tools/benchctl/src/noise.rs"),
        ),
        (
            "continuous_noise_guard_sha256",
            repo.join("tools/benchctl/src/noise.rs"),
        ),
    ];
    let mut identity = Map::new();
    for (field, path) in files {
        let actual = sha256(&path)?;
        if support.get(field).and_then(Value::as_str) != Some(actual.as_str()) {
            bail!("KV runtime provenance mismatch: {field}");
        }
        identity.insert(field.to_owned(), json!(actual));
    }
    for field in [
        "admission_policy",
        "continuous_noise_sample_ms",
        "continuous_noise_max_cpu_percent",
        "continuous_noise_max_io_average_mib_s",
        "continuous_noise_max_io_rate_mib_s",
        "hostname",
        "machine_id_sha256",
        "filesystem",
        "source",
    ] {
        identity.insert(
            field.to_owned(),
            support.get(field).cloned().unwrap_or(Value::Null),
        );
    }
    Ok(Value::Object(identity))
}

pub(crate) fn verify_kv(args: &ReadyArgs<'_>) -> Result<Value> {
    let manifest_path = args.selected.join("manifest.json");
    let summary_path = args.selected.join("summary.json");
    let results_path = args.selected.join("results.ndjson");
    for path in [
        &manifest_path,
        &summary_path,
        &results_path,
        args.binary_manifest,
    ] {
        if !path.is_file() {
            bail!("missing KV readiness artifact: {}", path.display());
        }
    }
    let manifest = read_json(&manifest_path)?;
    let summary = read_json(&summary_path)?;
    let rows = read_rows(&results_path)?;
    if !manifest
        .get("complete")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || value_i64(manifest.get("selected_groups"))? != 180
    {
        bail!("KV selection is not complete at 180 groups");
    }
    if value_i64(summary.get("group_count"))? != 180 || json_truthy(summary.get("problems")) {
        bail!("KV summary is not a clean 180-group corpus");
    }
    if value_i64(summary.get("row_count"))? != rows.len() as i64
        || value_i64(manifest.get("selected_rows"))? != rows.len() as i64
    {
        bail!("KV row-count provenance mismatch");
    }
    if value_i64(
        manifest
            .get("final_status_counts")
            .and_then(|value| value.get("undersized")),
    )
    .unwrap_or(0)
        != 0
    {
        bail!("KV selection still contains undersized groups");
    }

    let binary_data = read_json(args.binary_manifest)?;
    if binary_data.get("repo_commit").and_then(Value::as_str) != Some(args.expected_commit) {
        bail!("KV binary manifest commit mismatch");
    }
    let binary = binary_data
        .get("kv")
        .and_then(Value::as_object)
        .context("KV binary manifest is missing the kv binary")?;
    let binary_path = PathBuf::from(binary.get("path").and_then(Value::as_str).unwrap_or(""));
    let binary_sha = binary.get("sha256").and_then(Value::as_str).unwrap_or("");
    if !binary_path.is_file() || binary_sha.is_empty() || sha256(&binary_path)? != binary_sha {
        bail!("pinned KV binary verification failed");
    }

    let run_ids = selected_run_ids(&manifest, args.stock_run_id)?;
    if run_ids.is_empty() {
        bail!("KV selection has no source runs");
    }
    let mut support_hashes = Map::new();
    let mut identities = BTreeMap::<String, Value>::new();
    for run_id in &run_ids {
        let path = support_path(args.repo, run_id);
        if !path.is_file() {
            bail!("missing source support: {}", path.display());
        }
        let support = read_json(&path)?;
        if support.get("admission_policy").and_then(Value::as_str)
            != Some(args.expected_admission_policy)
        {
            bail!(
                "unexpected admission policy for {run_id}: {:?}",
                support.get("admission_policy").unwrap_or(&Value::Null)
            );
        }
        if support
            .get("benchmark_binary_sha256")
            .and_then(Value::as_str)
            != Some(binary_sha)
        {
            bail!("KV benchmark binary mismatch for {run_id}");
        }
        if support.get("build_profile").and_then(Value::as_str) != Some("external") {
            bail!("KV source was not run from a pinned external binary: {run_id}");
        }
        if value_f64(support.get("initial_min_free_gib")).unwrap_or(0.0) < 10.0
            || value_f64(support.get("case_min_free_gib")).unwrap_or(0.0) < 10.0
        {
            bail!("KV source free-space provenance is below quick-publication policy: {run_id}");
        }
        let identity = kv_source_identity(args.repo, &support)?;
        identities.insert(serde_json::to_string(&identity)?, identity);
        support_hashes.insert(run_id.clone(), json!(sha256(&path)?));
    }
    if identities.len() != 1 {
        bail!("selected KV sources have inconsistent runtime/admission identities");
    }
    let identity = identities
        .values()
        .next()
        .context("missing KV source identity")?;
    if identity.get("admission_policy").and_then(Value::as_str)
        != Some(args.expected_admission_policy)
    {
        bail!("KV selected source admission identity mismatch");
    }
    if args.expected_admission_policy == CONTINUOUS_ADMISSION_POLICY {
        for field in [
            "continuous_noise_guard_sha256",
            "continuous_noise_sample_ms",
            "continuous_noise_max_cpu_percent",
            "continuous_noise_max_io_average_mib_s",
            "continuous_noise_max_io_rate_mib_s",
        ] {
            let value = identity.get(field);
            if value.is_none()
                || matches!(value, Some(Value::Null))
                || value.and_then(Value::as_str) == Some("")
            {
                bail!("KV continuous admission provenance is incomplete");
            }
        }
    }
    let runner_repo_commit = checked_runner_commit(args.repo)?;
    if runner_repo_commit != args.expected_commit {
        bail!("KV runner checkout commit does not match pinned binary commit");
    }

    let mut ready = Map::new();
    ready.insert("ready_version".to_owned(), json!(7));
    ready.insert("repo_commit".to_owned(), json!(args.expected_commit));
    ready.insert("binary_repo_commit".to_owned(), json!(args.expected_commit));
    ready.insert("runner_repo_commit".to_owned(), json!(runner_repo_commit));
    ready.insert(
        "admission_policy".to_owned(),
        json!(args.expected_admission_policy),
    );
    ready.insert("group_count".to_owned(), json!(180));
    ready.insert("row_count".to_owned(), json!(rows.len()));
    ready.insert("benchmark_binary_sha256".to_owned(), json!(binary_sha));
    ready.insert(
        "source_support_sha256".to_owned(),
        Value::Object(support_hashes),
    );
    ready.insert("results_sha256".to_owned(), json!(sha256(&results_path)?));
    ready.insert(
        "selection_manifest_sha256".to_owned(),
        json!(sha256(&manifest_path)?),
    );
    ready.insert("summary_sha256".to_owned(), json!(sha256(&summary_path)?));
    ready.insert(
        "binary_manifest_sha256".to_owned(),
        json!(sha256(args.binary_manifest)?),
    );
    for (key, value) in identity
        .as_object()
        .context("KV identity is not an object")?
    {
        ready.insert(key.clone(), value.clone());
    }
    Ok(Value::Object(ready))
}

struct RecordSupports {
    hashes: Map<String, Value>,
    selected_noise_guard_sha256: String,
    continuous_identity: Option<Value>,
}

fn load_record_supports(
    repo: &Path,
    run_ids: &[String],
    expected_admission_policy: &str,
) -> Result<RecordSupports> {
    let mut supports = Vec::new();
    let mut hashes = Map::new();
    for run_id in run_ids {
        let path = support_path(repo, run_id);
        if !path.is_file() {
            bail!("missing source support: {}", path.display());
        }
        let support = read_json(&path)?;
        if support.get("admission_policy").and_then(Value::as_str)
            != Some(expected_admission_policy)
        {
            bail!(
                "unexpected admission policy for {run_id}: {:?}",
                support.get("admission_policy").unwrap_or(&Value::Null)
            );
        }
        hashes.insert(run_id.clone(), json!(sha256(&path)?));
        supports.push((run_id.clone(), support));
    }
    let noise_guards = supports
        .iter()
        .map(|(_, support)| {
            support
                .get("noise_guard_sha256")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect::<BTreeSet<_>>();
    if noise_guards.len() != 1
        || noise_guards.contains(&None)
        || noise_guards.contains(&Some(String::new()))
    {
        let rendered = noise_guards
            .iter()
            .map(|value| value.clone().unwrap_or_else(|| "None".to_owned()))
            .collect::<Vec<_>>();
        bail!("selected performance sources have inconsistent noise guards: {rendered:?}");
    }
    let selected_noise_guard_sha256 = noise_guards
        .into_iter()
        .next()
        .flatten()
        .context("missing selected noise guard")?;

    let continuous_identity = if expected_admission_policy == CONTINUOUS_ADMISSION_POLICY {
        let mut identities = BTreeMap::<String, Value>::new();
        for (run_id, support) in &supports {
            let identity = json!({
                "guard_sha256": support.get("continuous_noise_guard_sha256").cloned().unwrap_or(Value::Null),
                "sample_ms": support.get("continuous_noise_sample_ms").cloned().unwrap_or(Value::Null),
                "max_cpu_percent": support.get("continuous_noise_max_cpu_percent").cloned().unwrap_or(Value::Null),
                "max_io_average_mib_s": support.get("continuous_noise_max_io_average_mib_s").cloned().unwrap_or(Value::Null),
                "max_io_rate_mib_s": support.get("continuous_noise_max_io_rate_mib_s").cloned().unwrap_or(Value::Null),
            });
            if identity
                .as_object()
                .context("continuous identity is not an object")?
                .values()
                .any(|value| value.is_null() || value.as_str() == Some(""))
            {
                bail!("missing continuous admission provenance for {run_id}");
            }
            identities.insert(serde_json::to_string(&identity)?, identity);
        }
        if identities.len() != 1 {
            bail!("selected performance sources have inconsistent continuous admission provenance");
        }
        Some(
            identities
                .into_values()
                .next()
                .context("missing continuous admission identity")?,
        )
    } else {
        None
    };
    Ok(RecordSupports {
        hashes,
        selected_noise_guard_sha256,
        continuous_identity,
    })
}

pub(crate) fn verify_record(args: &ReadyArgs<'_>) -> Result<Value> {
    let runner_repo_commit = checked_runner_commit(args.repo)?;
    let manifest_path = args.selected.join("manifest.json");
    let summary_path = args.selected.join("summary.json");
    let results_path = args.selected.join("results.ndjson");
    for path in [
        &manifest_path,
        &summary_path,
        &results_path,
        args.binary_manifest,
    ] {
        if !path.is_file() {
            bail!("missing record readiness artifact: {}", path.display());
        }
    }
    let manifest = read_json(&manifest_path)?;
    let summary = read_json(&summary_path)?;
    let binary_manifest = read_json(args.binary_manifest)?;
    let rows = read_rows(&results_path)?;
    if !manifest
        .get("complete")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || value_i64(manifest.get("selected_groups"))? != 40
    {
        bail!("record selection is not complete at 40 groups");
    }
    if value_i64(summary.get("group_count"))? != 40 || json_truthy(summary.get("problems")) {
        bail!("record summary is not a clean 40-group corpus");
    }
    if value_i64(summary.get("row_count"))? != rows.len() as i64
        || value_i64(manifest.get("selected_rows"))? != rows.len() as i64
    {
        bail!("record row-count provenance mismatch");
    }
    let reads = rows
        .iter()
        .map(|row| row.get("read_materialization").and_then(Value::as_str))
        .collect::<BTreeSet<_>>();
    let writes = rows
        .iter()
        .map(|row| row.get("write_materialization").and_then(Value::as_str))
        .collect::<BTreeSet<_>>();
    if reads != BTreeSet::from([Some("full-record-v1")]) {
        bail!("unexpected read semantics: {reads:?}");
    }
    if writes != BTreeSet::from([Some("no-return-v1")]) {
        bail!("unexpected write semantics: {writes:?}");
    }

    let run_ids = selected_run_ids(&manifest, args.stock_run_id)?;
    let supports = load_record_supports(args.repo, &run_ids, args.expected_admission_policy)?;
    if binary_manifest.get("repo_commit").and_then(Value::as_str) != Some(args.expected_commit) {
        bail!("binary manifest commit mismatch");
    }
    if binary_manifest
        .get("read_materialization")
        .and_then(Value::as_str)
        != Some("full-record-v1")
        || binary_manifest
            .get("write_materialization")
            .and_then(Value::as_str)
            != Some("no-return-v1")
    {
        bail!("binary manifest semantic mismatch");
    }
    let binaries = binary_manifest
        .get("binaries")
        .and_then(Value::as_object)
        .context("binary manifest does not describe exactly six record binaries")?;
    if binaries.len() != 6 {
        bail!("binary manifest does not describe exactly six record binaries");
    }
    for (name, item) in binaries {
        let path = PathBuf::from(item.get("path").and_then(Value::as_str).unwrap_or(""));
        let expected = item.get("sha256").and_then(Value::as_str).unwrap_or("");
        if !path.is_file() || expected.is_empty() || sha256(&path)? != expected {
            bail!("pinned binary verification failed: {name}");
        }
    }

    let mut ready = Map::new();
    ready.insert(
        "ready_version".to_owned(),
        json!(if supports.continuous_identity.is_some() {
            5
        } else {
            4
        }),
    );
    ready.insert("repo_commit".to_owned(), json!(args.expected_commit));
    ready.insert("binary_repo_commit".to_owned(), json!(args.expected_commit));
    ready.insert("runner_repo_commit".to_owned(), json!(runner_repo_commit));
    ready.insert(
        "admission_policy".to_owned(),
        json!(args.expected_admission_policy),
    );
    ready.insert(
        "source_support_sha256".to_owned(),
        Value::Object(supports.hashes),
    );
    ready.insert(
        "selected_noise_guard_sha256".to_owned(),
        json!(supports.selected_noise_guard_sha256),
    );
    ready.insert("group_count".to_owned(), json!(40));
    ready.insert("row_count".to_owned(), json!(rows.len()));
    ready.insert("read_materialization".to_owned(), json!("full-record-v1"));
    ready.insert("write_materialization".to_owned(), json!("no-return-v1"));
    ready.insert("results_sha256".to_owned(), json!(sha256(&results_path)?));
    ready.insert(
        "selection_manifest_sha256".to_owned(),
        json!(sha256(&manifest_path)?),
    );
    ready.insert("summary_sha256".to_owned(), json!(sha256(&summary_path)?));
    ready.insert(
        "binary_manifest_sha256".to_owned(),
        json!(sha256(args.binary_manifest)?),
    );
    if let Some(identity) = supports.continuous_identity {
        ready.insert(
            "selected_continuous_noise_guard_sha256".to_owned(),
            identity.get("guard_sha256").cloned().unwrap_or(Value::Null),
        );
        ready.insert(
            "continuous_noise_sample_ms".to_owned(),
            identity.get("sample_ms").cloned().unwrap_or(Value::Null),
        );
        ready.insert(
            "continuous_noise_max_cpu_percent".to_owned(),
            identity
                .get("max_cpu_percent")
                .cloned()
                .unwrap_or(Value::Null),
        );
        ready.insert(
            "continuous_noise_max_io_average_mib_s".to_owned(),
            identity
                .get("max_io_average_mib_s")
                .cloned()
                .unwrap_or(Value::Null),
        );
        ready.insert(
            "continuous_noise_max_io_rate_mib_s".to_owned(),
            identity
                .get("max_io_rate_mib_s")
                .cloned()
                .unwrap_or(Value::Null),
        );
    }
    Ok(Value::Object(ready))
}

pub(crate) fn run_kv(args: ReadyArgs<'_>) -> Result<()> {
    let ready = verify_kv(&args)?;
    write_ready(args.out, &ready)
}

pub(crate) fn run_record(args: ReadyArgs<'_>) -> Result<()> {
    let ready = verify_record(&args)?;
    write_ready(args.out, &ready)
}

#[cfg(test)]
mod tests {
    use super::{CONTINUOUS_ADMISSION_POLICY, load_record_supports, selected_run_ids};
    use anyhow::Result;
    use serde_json::json;
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    fn temp_dir(label: &str) -> Result<PathBuf> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "all-db-bench-ready-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path)?;
        Ok(path)
    }

    #[test]
    fn selected_runs_include_stock_only_when_selected() -> Result<()> {
        let with_stock = json!({
            "selected_stock_groups": 179,
            "selected_resize_groups": 1,
            "selected_quality_repair_groups": 0,
            "sources": [{"source":"resize","run_id":"resize"}]
        });
        assert_eq!(selected_run_ids(&with_stock, "stock")?, ["resize", "stock"]);
        let repairs_only = json!({
            "selected_stock_groups": 0,
            "selected_resize_groups": 0,
            "selected_quality_repair_groups": 2,
            "sources": [
                {"source":"quality-repair","run_id":"fresh-b"},
                {"source":"quality-repair","run_id":"fresh-a"}
            ]
        });
        assert_eq!(
            selected_run_ids(&repairs_only, "old-stock")?,
            ["fresh-a", "fresh-b"]
        );
        Ok(())
    }

    #[test]
    fn selected_run_count_mismatch_is_rejected() {
        let manifest = json!({
            "selected_stock_groups": 1,
            "selected_resize_groups": 1,
            "selected_quality_repair_groups": 0,
            "sources": []
        });
        assert!(selected_run_ids(&manifest, "stock").is_err());
    }

    #[test]
    fn record_continuous_support_must_be_homogeneous() -> Result<()> {
        let repo = temp_dir("supports")?;
        for run_id in ["a", "b"] {
            let run = repo.join("results/runs").join(run_id);
            fs::create_dir_all(&run)?;
            fs::write(
                run.join("support.json"),
                serde_json::to_string(&json!({
                    "admission_policy": CONTINUOUS_ADMISSION_POLICY,
                    "noise_guard_sha256": "1".repeat(64),
                    "continuous_noise_guard_sha256": "2".repeat(64),
                    "continuous_noise_sample_ms": 250,
                    "continuous_noise_max_cpu_percent": 50,
                    "continuous_noise_max_io_average_mib_s": 2,
                    "continuous_noise_max_io_rate_mib_s": 8
                }))?,
            )?;
        }
        let loaded = load_record_supports(
            &repo,
            &["a".into(), "b".into()],
            CONTINUOUS_ADMISSION_POLICY,
        )?;
        assert_eq!(loaded.selected_noise_guard_sha256, "1".repeat(64));
        assert_eq!(
            loaded
                .continuous_identity
                .as_ref()
                .and_then(|value| value.get("sample_ms")),
            Some(&json!(250))
        );
        let path = repo.join("results/runs/b/support.json");
        let mut changed: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
        changed["continuous_noise_sample_ms"] = json!(500);
        fs::write(&path, serde_json::to_string(&changed)?)?;
        assert!(
            load_record_supports(
                &repo,
                &["a".into(), "b".into()],
                CONTINUOUS_ADMISSION_POLICY
            )
            .is_err()
        );
        fs::remove_dir_all(repo)?;
        Ok(())
    }
}
