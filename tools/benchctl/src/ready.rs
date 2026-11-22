use crate::file_hash::{lower_hex, sha256};
use crate::json_value::json_truthy;
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

pub const CONTINUOUS_ADMISSION_POLICY: &str = "pre-io+pre/continuous/post-external-v3";

const CONTINUOUS_FIELDS: [(&str, &str, &str); 5] = [
    (
        "continuous_noise_guard_sha256",
        "guard_sha256",
        "selected_continuous_noise_guard_sha256",
    ),
    (
        "continuous_noise_sample_ms",
        "sample_ms",
        "continuous_noise_sample_ms",
    ),
    (
        "continuous_noise_max_cpu_percent",
        "max_cpu_percent",
        "continuous_noise_max_cpu_percent",
    ),
    (
        "continuous_noise_max_io_average_mib_s",
        "max_io_average_mib_s",
        "continuous_noise_max_io_average_mib_s",
    ),
    (
        "continuous_noise_max_io_rate_mib_s",
        "max_io_rate_mib_s",
        "continuous_noise_max_io_rate_mib_s",
    ),
];

fn has_provenance_value(value: Option<&Value>) -> bool {
    value.is_some_and(|value| !value.is_null() && value.as_str() != Some(""))
}

#[derive(Clone, Copy)]
pub struct ReadyArgs<'a> {
    pub(crate) selected: &'a Path,
    pub(crate) binary_manifest: &'a Path,
    pub(crate) expected_commit: &'a str,
    pub(crate) repo: &'a Path,
    pub(crate) stock_run_id: &'a str,
    pub(crate) expected_admission_policy: &'a str,
    pub(crate) out: &'a Path,
}

struct Hashed<T> {
    value: T,
    sha256: String,
}

fn read_hashed<T>(path: &Path, parse: impl FnOnce(&str) -> Result<T>) -> Result<Hashed<T>> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let value = parse(&text)?;
    let sha256 = lower_hex(&Sha256::digest(text.as_bytes()));
    Ok(Hashed { value, sha256 })
}

fn read_hashed_json(path: &Path) -> Result<Hashed<Value>> {
    read_hashed(path, |text| {
        serde_json::from_str(text).with_context(|| format!("invalid JSON in {}", path.display()))
    })
}

fn read_rows(path: &Path) -> Result<Hashed<Vec<Value>>> {
    read_hashed(path, |text| {
        text.lines()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty())
            .map(|(index, line)| {
                serde_json::from_str(line)
                    .with_context(|| format!("invalid JSON at {}:{}", path.display(), index + 1))
            })
            .collect()
    })
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
    if i64::try_from(selected.len()).unwrap_or(i64::MAX) != expected {
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

struct SelectedArtifacts {
    manifest: Hashed<Value>,
    summary_sha256: String,
    results: Hashed<Vec<Value>>,
    groups: i64,
}

impl SelectedArtifacts {
    fn load(args: &ReadyArgs<'_>, lane: &str, groups: i64) -> Result<Self> {
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
                bail!("missing {lane} readiness artifact: {}", path.display());
            }
        }
        let manifest = read_hashed_json(&manifest_path)?;
        let summary = read_hashed_json(&summary_path)?;
        let results = read_rows(&results_path)?;
        if manifest.value.get("complete").and_then(Value::as_bool) != Some(true)
            || value_i64(manifest.value.get("selected_groups"))? != groups
        {
            bail!("{lane} selection is not complete at {groups} groups");
        }
        if value_i64(summary.value.get("group_count"))? != groups
            || json_truthy(summary.value.get("problems"))
        {
            bail!("{lane} summary is not a clean {groups}-group corpus");
        }
        let count = i64::try_from(results.value.len())?;
        if value_i64(summary.value.get("row_count"))? != count
            || value_i64(manifest.value.get("selected_rows"))? != count
        {
            bail!("{lane} row-count provenance mismatch");
        }
        Ok(Self {
            manifest,
            summary_sha256: summary.sha256,
            results,
            groups,
        })
    }

    fn certificate(
        &self,
        args: &ReadyArgs<'_>,
        ready_version: u32,
        runner_repo_commit: &str,
        support_hashes: Map<String, Value>,
        binary_manifest_sha256: &str,
    ) -> Map<String, Value> {
        Map::from_iter([
            ("ready_version".to_owned(), json!(ready_version)),
            ("repo_commit".to_owned(), json!(args.expected_commit)),
            ("binary_repo_commit".to_owned(), json!(args.expected_commit)),
            ("runner_repo_commit".to_owned(), json!(runner_repo_commit)),
            (
                "admission_policy".to_owned(),
                json!(args.expected_admission_policy),
            ),
            ("group_count".to_owned(), json!(self.groups)),
            ("row_count".to_owned(), json!(self.results.value.len())),
            (
                "source_support_sha256".to_owned(),
                Value::Object(support_hashes),
            ),
            ("results_sha256".to_owned(), json!(self.results.sha256)),
            (
                "selection_manifest_sha256".to_owned(),
                json!(self.manifest.sha256),
            ),
            ("summary_sha256".to_owned(), json!(self.summary_sha256)),
            (
                "binary_manifest_sha256".to_owned(),
                json!(binary_manifest_sha256),
            ),
        ])
    }
}

fn pinned_binary_sha<'a>(binary: &'a Map<String, Value>, label: &str) -> Result<&'a str> {
    let path = PathBuf::from(binary.get("path").and_then(Value::as_str).unwrap_or(""));
    let expected = binary.get("sha256").and_then(Value::as_str).unwrap_or("");
    if !path.is_file() || expected.is_empty() || sha256(&path)? != expected {
        bail!("pinned binary verification failed: {label}");
    }
    Ok(expected)
}

fn load_binary_manifest(args: &ReadyArgs<'_>) -> Result<Hashed<Value>> {
    let manifest = read_hashed_json(args.binary_manifest)?;
    if manifest.value.get("repo_commit").and_then(Value::as_str) != Some(args.expected_commit) {
        bail!("binary manifest commit mismatch");
    }
    Ok(manifest)
}

pub fn verify_kv(args: &ReadyArgs<'_>) -> Result<Value> {
    let selected = SelectedArtifacts::load(args, "KV", 180)?;
    if value_i64(
        selected
            .manifest
            .value
            .get("final_status_counts")
            .and_then(|value| value.get("undersized")),
    )
    .unwrap_or(0)
        != 0
    {
        bail!("KV selection still contains undersized groups");
    }

    let binary_data = load_binary_manifest(args)?;
    let binary = binary_data
        .value
        .get("kv")
        .and_then(Value::as_object)
        .context("KV binary manifest is missing the kv binary")?;
    let binary_sha = pinned_binary_sha(binary, "KV")?;

    let run_ids = selected_run_ids(&selected.manifest.value, args.stock_run_id)?;
    if run_ids.is_empty() {
        bail!("KV selection has no source runs");
    }
    let SourceSupports { supports, hashes } =
        load_source_supports(args.repo, &run_ids, args.expected_admission_policy)?;
    let identity = kv_support_identity(args, binary_sha, &supports)?;
    let runner_repo_commit = checked_runner_commit(args.repo)?;
    if runner_repo_commit != args.expected_commit {
        bail!("KV runner checkout commit does not match pinned binary commit");
    }

    let mut readiness =
        selected.certificate(args, 7, &runner_repo_commit, hashes, &binary_data.sha256);
    readiness.insert("benchmark_binary_sha256".to_owned(), json!(binary_sha));
    readiness.extend(
        identity
            .as_object()
            .context("KV identity is not an object")?
            .clone(),
    );
    Ok(Value::Object(readiness))
}

fn kv_support_identity(
    args: &ReadyArgs<'_>,
    binary_sha: &str,
    supports: &[(String, Value)],
) -> Result<Value> {
    let mut identities = BTreeMap::<String, Value>::new();
    for (run_id, support) in supports {
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
        let identity = kv_source_identity(args.repo, support)?;
        identities.insert(serde_json::to_string(&identity)?, identity);
    }
    if identities.len() != 1 {
        bail!("selected KV sources have inconsistent runtime/admission identities");
    }
    let identity = identities
        .into_values()
        .next()
        .context("missing KV source identity")?;
    if identity.get("admission_policy").and_then(Value::as_str)
        != Some(args.expected_admission_policy)
    {
        bail!("KV selected source admission identity mismatch");
    }
    if args.expected_admission_policy == CONTINUOUS_ADMISSION_POLICY {
        for (field, _, _) in CONTINUOUS_FIELDS {
            if !has_provenance_value(identity.get(field)) {
                bail!("KV continuous admission provenance is incomplete");
            }
        }
    }
    Ok(identity)
}

struct RecordSupports {
    hashes: Map<String, Value>,
    selected_noise_guard_sha256: String,
    continuous_identity: Option<Value>,
}

struct SourceSupports {
    supports: Vec<(String, Value)>,
    hashes: Map<String, Value>,
}

fn load_source_supports(
    repo: &Path,
    run_ids: &[String],
    expected_admission_policy: &str,
) -> Result<SourceSupports> {
    let mut supports = Vec::new();
    let mut hashes = Map::new();
    for run_id in run_ids {
        let path = support_path(repo, run_id);
        if !path.is_file() {
            bail!("missing source support: {}", path.display());
        }
        let support = read_hashed_json(&path)?;
        if support
            .value
            .get("admission_policy")
            .and_then(Value::as_str)
            != Some(expected_admission_policy)
        {
            bail!(
                "unexpected admission policy for {run_id}: {:?}",
                support
                    .value
                    .get("admission_policy")
                    .unwrap_or(&Value::Null)
            );
        }
        hashes.insert(run_id.clone(), json!(support.sha256));
        supports.push((run_id.clone(), support.value));
    }
    Ok(SourceSupports { supports, hashes })
}

fn load_record_supports(
    repo: &Path,
    run_ids: &[String],
    expected_admission_policy: &str,
) -> Result<RecordSupports> {
    let SourceSupports { supports, hashes } =
        load_source_supports(repo, run_ids, expected_admission_policy)?;
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
            let mut identity = Map::new();
            for (source, field, _) in CONTINUOUS_FIELDS {
                let value = support.get(source);
                if !has_provenance_value(value) {
                    bail!("missing continuous admission provenance for {run_id}");
                }
                identity.insert(field.to_owned(), value.cloned().unwrap_or(Value::Null));
            }
            let identity = Value::Object(identity);
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

pub fn verify_record(args: &ReadyArgs<'_>) -> Result<Value> {
    let runner_repo_commit = checked_runner_commit(args.repo)?;
    let selected = SelectedArtifacts::load(args, "record", 40)?;
    let binary_manifest = load_binary_manifest(args)?;
    let reads = selected
        .results
        .value
        .iter()
        .map(|row| row.get("read_materialization").and_then(Value::as_str))
        .collect::<BTreeSet<_>>();
    let writes = selected
        .results
        .value
        .iter()
        .map(|row| row.get("write_materialization").and_then(Value::as_str))
        .collect::<BTreeSet<_>>();
    if reads != BTreeSet::from([Some("full-record-v1")]) {
        bail!("unexpected read semantics: {reads:?}");
    }
    if writes != BTreeSet::from([Some("no-return-v1")]) {
        bail!("unexpected write semantics: {writes:?}");
    }

    let run_ids = selected_run_ids(&selected.manifest.value, args.stock_run_id)?;
    let supports = load_record_supports(args.repo, &run_ids, args.expected_admission_policy)?;
    if binary_manifest
        .value
        .get("read_materialization")
        .and_then(Value::as_str)
        != Some("full-record-v1")
        || binary_manifest
            .value
            .get("write_materialization")
            .and_then(Value::as_str)
            != Some("no-return-v1")
    {
        bail!("binary manifest semantic mismatch");
    }
    let binaries = binary_manifest
        .value
        .get("binaries")
        .and_then(Value::as_object)
        .context("binary manifest does not describe exactly six record binaries")?;
    if binaries.len() != 6 {
        bail!("binary manifest does not describe exactly six record binaries");
    }
    for (name, item) in binaries {
        pinned_binary_sha(
            item.as_object().context("binary entry is not an object")?,
            name,
        )?;
    }

    let ready_version = if supports.continuous_identity.is_some() {
        5
    } else {
        4
    };
    let mut readiness = selected.certificate(
        args,
        ready_version,
        &runner_repo_commit,
        supports.hashes,
        &binary_manifest.sha256,
    );
    readiness.insert(
        "selected_noise_guard_sha256".to_owned(),
        json!(supports.selected_noise_guard_sha256),
    );
    readiness.insert("read_materialization".to_owned(), json!("full-record-v1"));
    readiness.insert("write_materialization".to_owned(), json!("no-return-v1"));
    if let Some(identity) = supports.continuous_identity {
        for (_, field, certificate_field) in CONTINUOUS_FIELDS {
            readiness.insert(
                certificate_field.to_owned(),
                identity.get(field).cloned().unwrap_or(Value::Null),
            );
        }
    }
    Ok(Value::Object(readiness))
}

pub fn run_kv(args: ReadyArgs<'_>) -> Result<()> {
    let ready = verify_kv(&args)?;
    write_ready(args.out, &ready)
}

pub fn run_record(args: ReadyArgs<'_>) -> Result<()> {
    let ready = verify_record(&args)?;
    write_ready(args.out, &ready)
}

#[cfg(test)]
mod tests {
    use super::{
        CONTINUOUS_ADMISSION_POLICY, ReadyArgs, SelectedArtifacts, load_binary_manifest,
        load_record_supports, selected_run_ids,
    };
    use crate::file_hash::sha256;
    use anyhow::Result;
    use serde_json::Map;
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
    fn certificate_hashes_bind_the_exact_validated_artifacts() -> Result<()> {
        let repo = temp_dir("artifacts")?;
        let manifest = repo.join("manifest.json");
        let summary = repo.join("summary.json");
        let results = repo.join("results.ndjson");
        let binary = repo.join("binary.json");
        fs::write(
            &manifest,
            r#"{"complete":true,"selected_groups":2,"selected_rows":2}"#,
        )?;
        fs::write(&summary, r#"{"group_count":2,"row_count":2,"problems":[]}"#)?;
        fs::write(&results, "{\"trial\":1}\n{\"trial\":2}\n")?;
        fs::write(&binary, r#"{"repo_commit":"abc"}"#)?;
        let args = ReadyArgs {
            selected: &repo,
            binary_manifest: &binary,
            expected_commit: "abc",
            repo: &repo,
            stock_run_id: "stock",
            expected_admission_policy: CONTINUOUS_ADMISSION_POLICY,
            out: &repo.join("ready.json"),
        };
        let artifacts = SelectedArtifacts::load(&args, "KV", 2)?;
        let binary_manifest = load_binary_manifest(&args)?;
        let original_result_hash = sha256(&results)?;
        let original_manifest_hash = sha256(&manifest)?;
        let original_summary_hash = sha256(&summary)?;
        let original_binary_hash = sha256(&binary)?;
        fs::write(&results, "{}\n")?;
        fs::write(&manifest, "{}")?;
        fs::write(&summary, "{}")?;
        fs::write(&binary, "{}")?;
        let certificate =
            artifacts.certificate(&args, 7, "abc", Map::new(), &binary_manifest.sha256);
        assert_eq!(certificate["group_count"], 2);
        assert_eq!(certificate["row_count"], 2);
        assert_eq!(certificate["results_sha256"], original_result_hash);
        assert_eq!(
            certificate["selection_manifest_sha256"],
            original_manifest_hash
        );
        assert_eq!(certificate["summary_sha256"], original_summary_hash);
        assert_eq!(certificate["binary_manifest_sha256"], original_binary_hash);
        assert!(SelectedArtifacts::load(&args, "record", 2).is_err());
        assert!(load_binary_manifest(&args).is_err());
        fs::remove_dir_all(repo)?;
        Ok(())
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
