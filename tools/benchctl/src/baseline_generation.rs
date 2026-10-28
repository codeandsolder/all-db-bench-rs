use crate::idle;
use crate::json_io::read_json;
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

const ADMISSION_POLICY: &str = "pre-io+pre/continuous/post-external-v3";

#[derive(Clone, Debug)]
pub(crate) struct GenerationArgs<'a> {
    pub(crate) repo: &'a Path,
    pub(crate) generation_root: &'a Path,
    pub(crate) generation_id: &'a str,
    pub(crate) kv_binary_manifest: &'a Path,
    pub(crate) record_binary_manifest: &'a Path,
    pub(crate) lock_file: &'a Path,
    pub(crate) watch: bool,
    pub(crate) busy_sleep: f64,
    pub(crate) dry_run: bool,
}

#[derive(Clone, Debug)]
struct Pins {
    commit: String,
    kvbench: PathBuf,
    kvbench_sha256: String,
    recordbench: PathBuf,
    recordbench_sha256: String,
    rocks_recordbench: PathBuf,
    rocks_recordbench_sha256: String,
}

#[derive(Clone, Debug)]
struct GenerationPaths {
    plans: PathBuf,
    status: PathBuf,
    selected: PathBuf,
    ready: PathBuf,
    kv_stock_id: String,
    record_stock_id: String,
    kv_resize_prefix: String,
    record_resize_prefix: String,
    kv_audit: PathBuf,
    record_audit: PathBuf,
    kv_quality_initial: PathBuf,
    record_quality_initial: PathBuf,
    kv_quality_refined: PathBuf,
    record_quality_refined: PathBuf,
}

#[derive(Clone, Debug, Serialize)]
struct StageDump {
    name: String,
    argv: Vec<String>,
}
#[derive(Clone, Debug, Serialize)]
struct DryRun<'a> {
    commit: &'a str,
    generation_id: &'a str,
    stages: &'a [StageDump],
}

fn executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}
fn checked_repo_commit(repo: &Path) -> Result<String> {
    let out = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .current_dir(repo)
        .output()
        .context("failed to inspect generation checkout")?;
    if !out.status.success() {
        bail!("git status failed for generation checkout")
    };
    if !String::from_utf8(out.stdout)?.trim().is_empty() {
        bail!("generation runner checkout has tracked modifications")
    }
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()
        .context("failed to inspect generation commit")?;
    if !out.status.success() {
        bail!("git rev-parse failed for generation checkout")
    };
    Ok(String::from_utf8(out.stdout)?.trim().to_owned())
}
fn checked_sha(value: Option<&Value>, label: &str) -> Result<String> {
    let sha = value.and_then(Value::as_str).unwrap_or("");
    if sha.len() != 64
        || !sha
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
    {
        bail!("{label} manifest SHA-256 is invalid")
    };
    Ok(sha.to_owned())
}
fn checked_binary(path: PathBuf, label: &str) -> Result<PathBuf> {
    if !executable(&path) {
        bail!("{label} binary is not executable: {}", path.display())
    };
    Ok(path)
}

fn load_pins(repo: &Path, kv_manifest: &Path, record_manifest: &Path) -> Result<Pins> {
    let commit = checked_repo_commit(repo)?;
    let kv = read_json(kv_manifest)?;
    let record = read_json(record_manifest)?;
    if kv.get("repo_commit").and_then(Value::as_str) != Some(&commit)
        || record.get("repo_commit").and_then(Value::as_str) != Some(&commit)
    {
        bail!("binary manifests and generation runner must share one exact repo commit")
    }
    let kv_item = kv
        .get("kv")
        .and_then(Value::as_object)
        .context("unexpected binary manifest schema")?;
    let binaries = record
        .get("binaries")
        .and_then(Value::as_object)
        .context("unexpected binary manifest schema")?;
    let rec = binaries
        .get("recordbench")
        .and_then(Value::as_object)
        .context("record manifest is missing baseline binaries")?;
    let rocks = binaries
        .get("surrealdb-rocksdb-recordbench")
        .and_then(Value::as_object)
        .context("record manifest is missing baseline binaries")?;
    Ok(Pins {
        commit,
        kvbench: checked_binary(
            PathBuf::from(kv_item.get("path").and_then(Value::as_str).unwrap_or("")),
            "kvbench",
        )?,
        kvbench_sha256: checked_sha(kv_item.get("sha256"), "kvbench")?,
        recordbench: checked_binary(
            PathBuf::from(rec.get("path").and_then(Value::as_str).unwrap_or("")),
            "recordbench",
        )?,
        recordbench_sha256: checked_sha(rec.get("sha256"), "recordbench")?,
        rocks_recordbench: checked_binary(
            PathBuf::from(rocks.get("path").and_then(Value::as_str).unwrap_or("")),
            "surrealdb-rocksdb-recordbench",
        )?,
        rocks_recordbench_sha256: checked_sha(
            rocks.get("sha256"),
            "surrealdb-rocksdb-recordbench",
        )?,
    })
}

fn generation_paths(root: &Path, id: &str) -> GenerationPaths {
    let plans = root.join("plans");
    GenerationPaths {
        plans: plans.clone(),
        status: root.join("status"),
        selected: root.join("selected"),
        ready: root.join("ready"),
        kv_stock_id: format!("{id}-kv-stock"),
        record_stock_id: format!("{id}-record-stock"),
        kv_resize_prefix: format!("{id}-kv-resize"),
        record_resize_prefix: format!("{id}-record-resize"),
        kv_audit: plans.join(format!("{id}-kv-sizing.json")),
        record_audit: plans.join(format!("{id}-record-sizing.json")),
        kv_quality_initial: plans.join(format!("{id}-kv-quality-initial.json")),
        record_quality_initial: plans.join(format!("{id}-record-quality-initial.json")),
        kv_quality_refined: plans.join(format!("{id}-kv-quality-refined.json")),
        record_quality_refined: plans.join(format!("{id}-record-quality-refined.json")),
    }
}
fn script(repo: &Path) -> Result<String> {
    let p = repo.join("scripts/benchctl.sh");
    if !executable(&p) {
        bail!("generation script is not executable: {}", p.display())
    };
    Ok(p.to_string_lossy().into_owned())
}
fn py_float(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{v:.1}")
    } else {
        v.to_string()
    }
}
fn p(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
struct StagePlanArgs<'a> {
    repo: &'a Path,
    paths: &'a GenerationPaths,
    pins: &'a Pins,
    kv_manifest: &'a Path,
    record_manifest: &'a Path,
    lock_file: &'a Path,
    watch: bool,
    busy_sleep: f64,
}
fn stages(input: StagePlanArgs<'_>) -> Result<Vec<StageDump>> {
    let StagePlanArgs {
        repo,
        paths,
        pins,
        kv_manifest,
        record_manifest,
        lock_file,
        watch,
        busy_sleep,
    } = input;
    let b = script(repo)?;
    let run_root = repo.join("results/runs");
    let kv_stock = run_root.join(&paths.kv_stock_id);
    let record_stock = run_root.join(&paths.record_stock_id);
    let kv_selected = paths.selected.join("kv");
    let record_selected = paths.selected.join("record");
    let watch_args = if watch {
        vec![
            "--watch".into(),
            "--busy-sleep".into(),
            py_float(busy_sleep),
        ]
    } else {
        Vec::new()
    };
    let mut out = Vec::new();
    let mut add = |name: &str, mut argv: Vec<String>| {
        argv.extend(watch_args.clone());
        out.push(StageDump {
            name: name.into(),
            argv,
        });
    };
    add(
        "kv-stock",
        vec![
            b.clone(),
            "baseline-quick-idle".into(),
            "--lane".into(),
            "kv".into(),
            "--repo".into(),
            p(repo),
            "--bench-bin".into(),
            p(&pins.kvbench),
            "--expected-bench-sha256".into(),
            pins.kvbench_sha256.clone(),
            "--run-id".into(),
            paths.kv_stock_id.clone(),
            "--lock-file".into(),
            p(lock_file),
            "--status-file".into(),
            p(&paths.status.join("kv-stock.json")),
            "--audit-json".into(),
            p(&paths.kv_audit),
            "--audit-markdown".into(),
            p(&paths.kv_audit.with_extension("md")),
        ],
    );
    add(
        "record-stock",
        vec![
            b.clone(),
            "baseline-quick-idle".into(),
            "--lane".into(),
            "record".into(),
            "--repo".into(),
            p(repo),
            "--bench-bin".into(),
            p(&pins.recordbench),
            "--rocks-bench-bin".into(),
            p(&pins.rocks_recordbench),
            "--expected-bench-sha256".into(),
            pins.recordbench_sha256.clone(),
            "--expected-rocks-bench-sha256".into(),
            pins.rocks_recordbench_sha256.clone(),
            "--run-id".into(),
            paths.record_stock_id.clone(),
            "--lock-file".into(),
            p(lock_file),
            "--status-file".into(),
            p(&paths.status.join("record-stock.json")),
            "--audit-json".into(),
            p(&paths.record_audit),
            "--audit-markdown".into(),
            p(&paths.record_audit.with_extension("md")),
        ],
    );
    let plain = |name: &str, argv: Vec<String>| StageDump {
        name: name.into(),
        argv,
    };
    out.push(plain(
        "kv-quality-plan",
        vec![
            b.clone(),
            "audit-stock-short-pressure".into(),
            p(&kv_stock),
            p(&paths.kv_audit),
            "--lane".into(),
            "kv".into(),
            "--json-out".into(),
            p(&paths.kv_quality_initial),
            "--markdown-out".into(),
            p(&paths.kv_quality_initial.with_extension("md")),
        ],
    ));
    out.push(plain(
        "record-quality-plan",
        vec![
            b.clone(),
            "audit-stock-short-pressure".into(),
            p(&record_stock),
            p(&paths.record_audit),
            "--lane".into(),
            "record".into(),
            "--json-out".into(),
            p(&paths.record_quality_initial),
            "--markdown-out".into(),
            p(&paths.record_quality_initial.with_extension("md")),
        ],
    ));
    let kv_follow = |name: &str, quality: &Path, status: &str| {
        let mut argv = vec![
            b.clone(),
            "baseline-sizing-followups".into(),
            "--lane".into(),
            "kv".into(),
            "--repo".into(),
            p(repo),
            "--plan".into(),
            p(&paths.kv_audit),
            "--quality-plan".into(),
            p(quality),
            "--run-prefix".into(),
            paths.kv_resize_prefix.clone(),
            "--bench-bin".into(),
            p(&pins.kvbench),
            "--expected-bench-sha256".into(),
            pins.kvbench_sha256.clone(),
            "--lock-file".into(),
            p(lock_file),
            "--status-file".into(),
            p(&paths.status.join(status)),
        ];
        argv.extend(watch_args.clone());
        StageDump {
            name: name.into(),
            argv,
        }
    };
    out.push(kv_follow(
        "kv-followups-initial",
        &paths.kv_quality_initial,
        "kv-followups-initial.json",
    ));
    let record_follow = |name: &str, quality: &Path, status: &str| {
        let mut argv = vec![
            b.clone(),
            "baseline-sizing-followups".into(),
            "--lane".into(),
            "record".into(),
            "--repo".into(),
            p(repo),
            "--plan".into(),
            p(&paths.record_audit),
            "--quality-plan".into(),
            p(quality),
            "--run-prefix".into(),
            paths.record_resize_prefix.clone(),
            "--bench-bin".into(),
            p(&pins.recordbench),
            "--rocks-bench-bin".into(),
            p(&pins.rocks_recordbench),
            "--expected-bench-sha256".into(),
            pins.recordbench_sha256.clone(),
            "--expected-rocks-bench-sha256".into(),
            pins.rocks_recordbench_sha256.clone(),
            "--lock-file".into(),
            p(lock_file),
            "--status-file".into(),
            p(&paths.status.join(status)),
        ];
        argv.extend(watch_args.clone());
        StageDump {
            name: name.into(),
            argv,
        }
    };
    out.push(record_follow(
        "record-followups-initial",
        &paths.record_quality_initial,
        "record-followups-initial.json",
    ));
    out.push(plain(
        "kv-refine-quality",
        vec![
            b.clone(),
            "refine-stock-quality-repairs".into(),
            p(&paths.kv_quality_initial),
            p(&paths.kv_audit),
            p(&run_root),
            "--resize-prefix".into(),
            paths.kv_resize_prefix.clone(),
            "--json-out".into(),
            p(&paths.kv_quality_refined),
            "--markdown-out".into(),
            p(&paths.kv_quality_refined.with_extension("md")),
        ],
    ));
    out.push(plain(
        "record-refine-quality",
        vec![
            b.clone(),
            "refine-stock-quality-repairs".into(),
            p(&paths.record_quality_initial),
            p(&paths.record_audit),
            p(&run_root),
            "--resize-prefix".into(),
            paths.record_resize_prefix.clone(),
            "--json-out".into(),
            p(&paths.record_quality_refined),
            "--markdown-out".into(),
            p(&paths.record_quality_refined.with_extension("md")),
        ],
    ));
    out.push(kv_follow(
        "kv-followups-refined",
        &paths.kv_quality_refined,
        "kv-followups-refined.json",
    ));
    out.push(record_follow(
        "record-followups-refined",
        &paths.record_quality_refined,
        "record-followups-refined.json",
    ));
    out.push(plain(
        "finalize",
        vec![
            b.clone(),
            "finalize-baseline-sizing".into(),
            "--repo".into(),
            p(repo),
            "--kv-stock-results".into(),
            p(&kv_stock.join("results.ndjson")),
            "--kv-audit".into(),
            p(&paths.kv_audit),
            "--kv-resize-prefix".into(),
            paths.kv_resize_prefix.clone(),
            "--kv-out-dir".into(),
            p(&kv_selected),
            "--kv-quality-repair-plan".into(),
            p(&paths.kv_quality_refined),
            "--record-stock-results".into(),
            p(&record_stock.join("results.ndjson")),
            "--record-audit".into(),
            p(&paths.record_audit),
            "--record-resize-prefix".into(),
            paths.record_resize_prefix.clone(),
            "--record-out-dir".into(),
            p(&record_selected),
            "--record-quality-repair-plan".into(),
            p(&paths.record_quality_refined),
        ],
    ));
    out.push(plain(
        "verify-kv",
        vec![
            b.clone(),
            "verify-kv-ready".into(),
            "--selected".into(),
            p(&kv_selected),
            "--binary-manifest".into(),
            p(kv_manifest),
            "--expected-commit".into(),
            pins.commit.clone(),
            "--repo".into(),
            p(repo),
            "--stock-run-id".into(),
            paths.kv_stock_id.clone(),
            "--expected-admission-policy".into(),
            ADMISSION_POLICY.into(),
            "--out".into(),
            p(&paths.ready.join("kv.json")),
        ],
    ));
    out.push(plain(
        "verify-record",
        vec![
            b,
            "verify-record-ready".into(),
            "--selected".into(),
            p(&record_selected),
            "--binary-manifest".into(),
            p(record_manifest),
            "--expected-commit".into(),
            pins.commit.clone(),
            "--repo".into(),
            p(repo),
            "--stock-run-id".into(),
            paths.record_stock_id.clone(),
            "--expected-admission-policy".into(),
            ADMISSION_POLICY.into(),
            "--out".into(),
            p(&paths.ready.join("record.json")),
        ],
    ));
    Ok(out)
}

fn generation_status(state: &str, id: &str, commit: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("state".into(), json!(state));
    m.insert("generation_id".into(), json!(id));
    m.insert("commit".into(), json!(commit));
    m
}

pub(crate) fn run(args: GenerationArgs<'_>) -> Result<i32> {
    let pins = load_pins(
        args.repo,
        args.kv_binary_manifest,
        args.record_binary_manifest,
    )?;
    let paths = generation_paths(args.generation_root, args.generation_id);
    for dir in [&paths.plans, &paths.status, &paths.selected, &paths.ready] {
        fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?
    }
    let planned = stages(StagePlanArgs {
        repo: args.repo,
        paths: &paths,
        pins: &pins,
        kv_manifest: args.kv_binary_manifest,
        record_manifest: args.record_binary_manifest,
        lock_file: args.lock_file,
        watch: args.watch,
        busy_sleep: args.busy_sleep,
    })?;
    if args.dry_run {
        println!(
            "{}",
            serde_json::to_string_pretty(&DryRun {
                commit: &pins.commit,
                generation_id: args.generation_id,
                stages: &planned
            })?
        );
        return Ok(0);
    }
    let overall = paths.status.join("baseline-generation.json");
    let total = planned.len();
    for (offset, stage) in planned.iter().enumerate() {
        let index = offset + 1;
        let mut s = generation_status("running", args.generation_id, &pins.commit);
        s.insert("stage".into(), json!(stage.name));
        s.insert("stage_index".into(), json!(index));
        s.insert("stage_count".into(), json!(total));
        let _ = idle::write_status(&overall, s)?;
        let status = Command::new(&stage.argv[0])
            .args(&stage.argv[1..])
            .current_dir(args.repo)
            .status()
            .with_context(|| format!("failed to run generation stage {}", stage.name))?;
        let code = status.code().unwrap_or(2);
        if code != 0 {
            let mut s = generation_status(
                if code == 75 {
                    "waiting-for-idle"
                } else {
                    "failed"
                },
                args.generation_id,
                &pins.commit,
            );
            s.insert("stage".into(), json!(stage.name));
            s.insert("stage_index".into(), json!(index));
            s.insert("stage_count".into(), json!(total));
            s.insert("returncode".into(), json!(code));
            let _ = idle::write_status(&overall, s)?;
            return Ok(code);
        }
    }
    let mut s = generation_status("complete", args.generation_id, &pins.commit);
    s.insert("stage_count".into(), json!(total));
    s.insert("kv_ready".into(), json!(p(&paths.ready.join("kv.json"))));
    s.insert(
        "record_ready".into(),
        json!(p(&paths.ready.join("record.json"))),
    );
    let _ = idle::write_status(&overall, s)?;
    println!("baseline generation complete: {}", args.generation_id);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };
    fn temp() -> Result<PathBuf> {
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "baseline-generation-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p)?;
        Ok(p)
    }
    fn fake_repo(root: &Path) -> Result<PathBuf> {
        let repo = root.join("repo");
        fs::create_dir_all(repo.join("scripts"))?;
        let b = repo.join("scripts/benchctl.sh");
        fs::write(&b, "#!/bin/sh\nexit 0\n")?;
        let mut perms = fs::metadata(&b)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&b, perms)?;
        Ok(repo)
    }
    #[test]
    fn generation_namespace_is_consistent() -> Result<()> {
        let p = generation_paths(Path::new("/generation"), "refresh");
        assert_eq!(p.kv_stock_id, "refresh-kv-stock");
        assert_eq!(p.record_resize_prefix, "refresh-record-resize");
        assert_eq!(
            p.kv_audit,
            Path::new("/generation/plans/refresh-kv-sizing.json")
        );
        Ok(())
    }
    #[test]
    fn stage_plan_has_thirteen_generation_scoped_steps() -> Result<()> {
        let root = temp()?;
        let repo = fake_repo(&root)?;
        let paths = generation_paths(&root.join("generation"), "g");
        let pins = Pins {
            commit: "d".repeat(40),
            kvbench: root.join("kvbench"),
            kvbench_sha256: "kvsha".into(),
            recordbench: root.join("recordbench"),
            recordbench_sha256: "recordsha".into(),
            rocks_recordbench: root.join("rocks"),
            rocks_recordbench_sha256: "rockssha".into(),
        };
        let kv_manifest = root.join("kv.json");
        let record_manifest = root.join("record.json");
        let plan = stages(StagePlanArgs {
            repo: &repo,
            paths: &paths,
            pins: &pins,
            kv_manifest: &kv_manifest,
            record_manifest: &record_manifest,
            lock_file: Path::new("/run/lock/all-db-bench-performance.lock"),
            watch: true,
            busy_sleep: 3.0,
        })?;
        assert_eq!(plan.len(), 13);
        assert_eq!(plan[0].name, "kv-stock");
        assert_eq!(plan[12].name, "verify-record");
        let all = plan
            .iter()
            .flat_map(|s| s.argv.iter())
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains("g-kv-stock"));
        assert!(all.contains("g-record-resize"));
        assert!(plan[0].argv.iter().any(|x| x == "3.0"));
        fs::remove_dir_all(root)?;
        Ok(())
    }
    #[test]
    fn manifest_sha_must_be_lower_hex() {
        assert!(checked_sha(Some(&json!("a".repeat(64))), "x").is_ok());
        assert!(checked_sha(Some(&json!("A".repeat(64))), "x").is_err());
        assert!(checked_sha(Some(&json!("bad")), "x").is_err());
    }
}
