use crate::file_hash::sha256;
use crate::json_io::read_json;
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, fs, path::Path};

const IMPORTER_SOURCE: &str = include_str!("import_concurrency.rs");
const POLICY: &str = "reuse only accepted non-Persy v1 cases; all Persy cases rerun under v2 lock-timeout/retry semantics";

#[derive(Debug)]
pub(crate) struct ImportArgs<'a> {
    pub(crate) source: Option<&'a Path>,
    pub(crate) dest: Option<&'a Path>,
    pub(crate) excluded_engines: &'a [String],
    pub(crate) verify_run: Option<&'a Path>,
}

fn importer_sha256() -> String {
    let mut digest = Sha256::new();
    digest.update(IMPORTER_SOURCE.as_bytes());
    format!("{:x}", digest.finalize())
}

fn require_quiet(path: &Path) -> Result<()> {
    let data = read_json(path)?;
    if data.get("quiet").and_then(Value::as_bool) != Some(true) {
        bail!("source noise evidence is not quiet: {}", path.display());
    }
    Ok(())
}

fn copy_verified(src: &Path, dst: &Path) -> Result<String> {
    let digest = sha256(src)?;
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    if dst.exists() {
        if sha256(dst)? != digest {
            bail!("destination conflict: {}", dst.display());
        }
    } else {
        fs::copy(src, dst)
            .with_context(|| format!("failed to copy {} to {}", src.display(), dst.display()))?;
    }
    if sha256(dst)? != digest {
        bail!("copy verification failed: {}", dst.display());
    }
    Ok(digest)
}

fn string_or_empty(value: Option<&Value>) -> String {
    value.and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn case_paths(source: &Path) -> Result<Vec<std::path::PathBuf>> {
    let root = source.join("cases");
    let mut paths = Vec::new();
    if root.is_dir() {
        for entry in
            fs::read_dir(&root).with_context(|| format!("failed to list {}", root.display()))?
        {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) == Some("json") {
                paths.push(path);
            }
        }
    }
    paths.sort();
    Ok(paths)
}

fn import_results(source: &Path, dest: &Path, excluded: &BTreeSet<String>) -> Result<Value> {
    let support_path = source.join("support.json");
    let jobs_path = source.join("jobs.txt");
    if !support_path.is_file() || !jobs_path.is_file() {
        bail!("source run must contain support.json and jobs.txt");
    }
    let support = read_json(&support_path)?;
    if support.get("lane").and_then(Value::as_str) != Some("kv-concurrency")
        || support.get("profile").and_then(Value::as_str) != Some("quick")
    {
        bail!("source run is not a quick kv-concurrency campaign");
    }
    if dest.join("support.json").exists() || dest.join("jobs.txt").exists() {
        bail!(
            "destination support/jobs already exist; import must precede v2 runner initialization"
        );
    }

    let mut entries = Vec::new();
    for case_path in case_paths(source)? {
        let row = read_json(&case_path)?;
        let engine = string_or_empty(row.get("engine"));
        if excluded.contains(&engine) {
            continue;
        }
        let case_id = case_path
            .file_stem()
            .and_then(|value| value.to_str())
            .with_context(|| format!("invalid case filename: {}", case_path.display()))?;
        let before = source.join("noise").join(format!("{case_id}.before.json"));
        let after = source.join("noise").join(format!("{case_id}.after.json"));
        if !before.is_file() || !after.is_file() {
            bail!("missing source noise evidence for {case_id}");
        }
        require_quiet(&before)?;
        require_quiet(&after)?;
        let result_sha = copy_verified(
            &case_path,
            &dest
                .join("cases")
                .join(case_path.file_name().context("missing case filename")?),
        )?;
        let before_sha = copy_verified(
            &before,
            &dest
                .join("noise")
                .join(before.file_name().context("missing before filename")?),
        )?;
        let after_sha = copy_verified(
            &after,
            &dest
                .join("noise")
                .join(after.file_name().context("missing after filename")?),
        )?;
        entries.push(json!({
            "case_id": case_id,
            "engine": engine,
            "trial": row.get("trial").cloned().unwrap_or(Value::Null),
            "workload": row.get("workload").cloned().unwrap_or(Value::Null),
            "clients": row.get("clients").cloned().unwrap_or(Value::Null),
            "elapsed_s": row.get("elapsed_s").cloned().unwrap_or(Value::Null),
            "result_sha256": result_sha,
            "noise_before_sha256": before_sha,
            "noise_after_sha256": after_sha,
        }));
    }

    let source_run_id = source
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let excluded_values = excluded
        .iter()
        .cloned()
        .map(Value::String)
        .collect::<Vec<_>>();
    let manifest = json!({
        "format_version": 1,
        "policy": POLICY,
        "source_run_id": source_run_id,
        "source_support_sha256": sha256(&support_path)?,
        "source_jobs_sha256": sha256(&jobs_path)?,
        "source_benchmark_binary_sha256": support.get("benchmark_binary_sha256").cloned().unwrap_or(Value::Null),
        "source_runner_sha256": support.get("runner_sha256").cloned().unwrap_or(Value::Null),
        "importer_sha256": importer_sha256(),
        "excluded_engines": excluded_values,
        "imported_case_count": entries.len(),
        "cases": entries,
    });
    fs::create_dir_all(dest).with_context(|| format!("failed to create {}", dest.display()))?;
    let out = dest.join("import-manifest.json");
    fs::write(&out, serde_json::to_string_pretty(&manifest)? + "\n")
        .with_context(|| format!("failed to write {}", out.display()))?;
    let mut returned = manifest;
    returned["manifest_sha256"] = Value::String(sha256(&out)?);
    Ok(returned)
}

fn verify_import(run_dir: &Path) -> Result<Value> {
    let manifest_path = run_dir.join("import-manifest.json");
    if !manifest_path.is_file() {
        bail!("missing import manifest: {}", manifest_path.display());
    }
    let manifest = read_json(&manifest_path)?;
    let expected = manifest.get("importer_sha256").and_then(Value::as_str);
    let actual = importer_sha256();
    if expected != Some(actual.as_str()) {
        bail!(
            "importer SHA-256 mismatch: manifest={} current={actual}",
            expected.unwrap_or("None")
        );
    }
    let entries = manifest
        .get("cases")
        .and_then(Value::as_array)
        .context("import manifest cases must be a list")?;
    if manifest.get("imported_case_count").and_then(Value::as_u64)
        != u64::try_from(entries.len()).ok()
    {
        bail!("imported_case_count does not match manifest case list");
    }
    let excluded = manifest
        .get("excluded_engines")
        .and_then(Value::as_array)
        .context("excluded_engines must be a list")?
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let mut seen = BTreeSet::new();
    for entry in entries {
        let case_id = entry.get("case_id").and_then(Value::as_str);
        let Some(case_id) = case_id.filter(|value| !value.is_empty()) else {
            bail!("invalid or duplicate imported case_id");
        };
        if !seen.insert(case_id.to_owned()) {
            bail!("invalid or duplicate imported case_id: {case_id}");
        }
        let engine = entry
            .get("engine")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if excluded.contains(engine) {
            bail!("excluded engine appears in imported cases: {engine}");
        }
        let artifacts = [
            (
                "result_sha256",
                run_dir.join("cases").join(format!("{case_id}.json")),
            ),
            (
                "noise_before_sha256",
                run_dir.join("noise").join(format!("{case_id}.before.json")),
            ),
            (
                "noise_after_sha256",
                run_dir.join("noise").join(format!("{case_id}.after.json")),
            ),
        ];
        for (field, path) in artifacts {
            if !path.is_file() {
                bail!("missing imported artifact: {}", path.display());
            }
            let actual = sha256(&path)?;
            let expected = entry.get(field).and_then(Value::as_str).unwrap_or_default();
            if actual != expected {
                bail!(
                    "imported artifact SHA-256 mismatch for {case_id} {field}: manifest={expected} actual={actual}"
                );
            }
        }
        require_quiet(&run_dir.join("noise").join(format!("{case_id}.before.json")))?;
        require_quiet(&run_dir.join("noise").join(format!("{case_id}.after.json")))?;
    }
    Ok(manifest)
}

pub(crate) fn run(args: ImportArgs<'_>) -> Result<()> {
    if let Some(run_dir) = args.verify_run {
        if args.source.is_some() || args.dest.is_some() {
            bail!("--verify-run cannot be combined with source/dest");
        }
        let manifest = verify_import(run_dir)?;
        let count = manifest
            .get("imported_case_count")
            .and_then(Value::as_u64)
            .context("invalid imported_case_count")?;
        println!("{{\"imported_case_count\": {count}, \"verified\": true}}");
        return Ok(());
    }
    let source = args
        .source
        .context("source and dest are required unless --verify-run is used")?;
    let dest = args
        .dest
        .context("source and dest are required unless --verify-run is used")?;
    let mut excluded = args
        .excluded_engines
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    excluded.insert("persy".to_owned());
    let manifest = import_results(source, dest, &excluded)?;
    let mut summary = Map::new();
    if let Some(object) = manifest.as_object() {
        for (key, value) in object {
            if key != "cases" {
                summary.insert(key.clone(), value.clone());
            }
        }
    }
    println!("{}", serde_json::to_string_pretty(&Value::Object(summary))?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{import_results, importer_sha256, verify_import};
    use anyhow::Result;
    use serde_json::{Value, json};
    use std::{collections::BTreeSet, fs, path::Path};

    fn source(root: &Path) -> Result<std::path::PathBuf> {
        let source = root.join("v1");
        fs::create_dir(&source)?;
        fs::write(
            source.join("support.json"),
            serde_json::to_string(&json!({
                "lane": "kv-concurrency",
                "profile": "quick",
                "benchmark_binary_sha256": "old-bin",
                "runner_sha256": "old-runner",
            }))? + "\n",
        )?;
        fs::write(source.join("jobs.txt"), "job\n")?;
        Ok(source)
    }

    fn write_case(run: &Path, id: &str, engine: &str, quiet: bool) -> Result<()> {
        fs::create_dir_all(run.join("cases"))?;
        fs::create_dir_all(run.join("noise"))?;
        fs::write(
            run.join("cases").join(format!("{id}.json")),
            serde_json::to_string(&json!({
                "engine": engine,
                "trial": 1,
                "workload": "delete-burst",
                "clients": 8,
                "elapsed_s": 1.5,
            }))? + "\n",
        )?;
        let evidence = serde_json::to_string(&json!({"quiet": quiet}))? + "\n";
        fs::write(
            run.join("noise").join(format!("{id}.before.json")),
            &evidence,
        )?;
        fs::write(run.join("noise").join(format!("{id}.after.json")), evidence)?;
        Ok(())
    }

    fn temp_root(name: &str) -> Result<std::path::PathBuf> {
        let root = std::env::temp_dir().join(format!("all-db-bench-{name}-{}", std::process::id()));
        if root.exists() {
            fs::remove_dir_all(&root)?;
        }
        fs::create_dir_all(&root)?;
        Ok(root)
    }

    #[test]
    fn excludes_persy_and_hashes_quiet_cases() -> Result<()> {
        let root = temp_root("import-excludes")?;
        let src = source(&root)?;
        write_case(&src, "redb-case", "redb", true)?;
        write_case(&src, "persy-case", "persy", true)?;
        let dest = root.join("v2");
        let manifest = import_results(&src, &dest, &BTreeSet::from(["persy".to_owned()]))?;
        assert_eq!(manifest["imported_case_count"], 1);
        assert_eq!(manifest["cases"][0]["case_id"], "redb-case");
        assert!(dest.join("cases/redb-case.json").is_file());
        assert!(!dest.join("cases/persy-case.json").exists());
        assert_eq!(
            manifest["importer_sha256"],
            Value::String(importer_sha256())
        );
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn verify_detects_tampering() -> Result<()> {
        let root = temp_root("import-tamper")?;
        let src = source(&root)?;
        write_case(&src, "ok", "redb", true)?;
        let dest = root.join("v2");
        import_results(&src, &dest, &BTreeSet::from(["persy".to_owned()]))?;
        verify_import(&dest)?;
        fs::write(dest.join("cases/ok.json"), "{}\n")?;
        assert!(verify_import(&dest).is_err());
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn rejects_nonquiet_source() -> Result<()> {
        let root = temp_root("import-noisy")?;
        let src = source(&root)?;
        write_case(&src, "bad", "redb", false)?;
        assert!(import_results(&src, &root.join("v2"), &BTreeSet::new()).is_err());
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn refuses_initialized_destination() -> Result<()> {
        let root = temp_root("import-initialized")?;
        let src = source(&root)?;
        write_case(&src, "ok", "redb", true)?;
        let dest = root.join("v2");
        fs::create_dir(&dest)?;
        fs::write(dest.join("support.json"), "{}\n")?;
        assert!(import_results(&src, &dest, &BTreeSet::new()).is_err());
        fs::remove_dir_all(root)?;
        Ok(())
    }
}
