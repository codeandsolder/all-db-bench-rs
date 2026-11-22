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

#[derive(Clone, Debug, Copy)]
pub struct GenerationArgs<'a> {
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
struct BinaryPin {
    path: PathBuf,
    sha256: String,
}

#[derive(Clone, Debug)]
struct Pins {
    commit: String,
    kv: BinaryPin,
    record: BinaryPin,
    rocks: BinaryPin,
}

#[derive(Clone, Copy, Debug)]
enum Lane {
    Kv,
    Record,
}

impl Lane {
    const fn name(self) -> &'static str {
        match self {
            Self::Kv => "kv",
            Self::Record => "record",
        }
    }
}

#[derive(Clone, Debug)]
struct LanePaths {
    stock_id: String,
    resize_prefix: String,
    audit: PathBuf,
    quality_initial: PathBuf,
    quality_refined: PathBuf,
}

impl LanePaths {
    fn new(plans: &Path, id: &str, lane: Lane) -> Self {
        let lane = lane.name();
        Self {
            stock_id: format!("{id}-{lane}-stock"),
            resize_prefix: format!("{id}-{lane}-resize"),
            audit: plans.join(format!("{id}-{lane}-sizing.json")),
            quality_initial: plans.join(format!("{id}-{lane}-quality-initial.json")),
            quality_refined: plans.join(format!("{id}-{lane}-quality-refined.json")),
        }
    }
}

#[derive(Clone, Debug)]
struct GenerationPaths {
    plans: PathBuf,
    status: PathBuf,
    selected: PathBuf,
    ready: PathBuf,
    kv: LanePaths,
    record: LanePaths,
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
    }
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
    }
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
    }
    Ok(sha.to_owned())
}
fn checked_binary(path: PathBuf, label: &str) -> Result<PathBuf> {
    if !executable(&path) {
        bail!("{label} binary is not executable: {}", path.display())
    }
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
    let pin = |item: &Map<String, Value>, label: &str| -> Result<BinaryPin> {
        Ok(BinaryPin {
            path: checked_binary(
                PathBuf::from(item.get("path").and_then(Value::as_str).unwrap_or("")),
                label,
            )?,
            sha256: checked_sha(item.get("sha256"), label)?,
        })
    };
    Ok(Pins {
        commit,
        kv: pin(kv_item, "kvbench")?,
        record: pin(rec, "recordbench")?,
        rocks: pin(rocks, "surrealdb-rocksdb-recordbench")?,
    })
}

fn generation_paths(root: &Path, id: &str) -> GenerationPaths {
    let plans = root.join("plans");
    GenerationPaths {
        kv: LanePaths::new(&plans, id, Lane::Kv),
        record: LanePaths::new(&plans, id, Lane::Record),
        plans,
        status: root.join("status"),
        selected: root.join("selected"),
        ready: root.join("ready"),
    }
}
fn script(repo: &Path) -> Result<String> {
    let p = repo.join("scripts/benchctl.sh");
    if !executable(&p) {
        bail!("generation script is not executable: {}", p.display())
    }
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
#[derive(Clone, Copy)]
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
#[derive(Clone, Copy)]
enum QualityPass {
    Initial,
    Refined,
}

impl QualityPass {
    const fn name(self) -> &'static str {
        match self {
            Self::Initial => "initial",
            Self::Refined => "refined",
        }
    }

    fn path(self, paths: &LanePaths) -> &Path {
        match self {
            Self::Initial => &paths.quality_initial,
            Self::Refined => &paths.quality_refined,
        }
    }
}

struct LanePlan<'a> {
    lane: Lane,
    paths: &'a LanePaths,
    primary: &'a BinaryPin,
    rocks: Option<&'a BinaryPin>,
    manifest: &'a Path,
    input: StagePlanArgs<'a>,
    script: &'a str,
    run_root: &'a Path,
}

impl<'a> LanePlan<'a> {
    const fn new(
        lane: Lane,
        input: StagePlanArgs<'a>,
        script: &'a str,
        run_root: &'a Path,
    ) -> Self {
        let (paths, primary, rocks, manifest) = match lane {
            Lane::Kv => (&input.paths.kv, &input.pins.kv, None, input.kv_manifest),
            Lane::Record => (
                &input.paths.record,
                &input.pins.record,
                Some(&input.pins.rocks),
                input.record_manifest,
            ),
        };
        Self {
            lane,
            paths,
            primary,
            rocks,
            manifest,
            input,
            script,
            run_root,
        }
    }

    fn command(&self, command: &str) -> Vec<String> {
        vec![self.script.to_owned(), command.to_owned()]
    }

    fn lane_repo(&self, argv: &mut Vec<String>) {
        argv.extend([
            "--lane".into(),
            self.lane.name().into(),
            "--repo".into(),
            p(self.input.repo),
        ]);
    }

    fn binary_args(&self, argv: &mut Vec<String>) {
        argv.extend(["--bench-bin".into(), p(&self.primary.path)]);
        if let Some(rocks) = self.rocks {
            argv.extend(["--rocks-bench-bin".into(), p(&rocks.path)]);
        }
        argv.extend([
            "--expected-bench-sha256".into(),
            self.primary.sha256.clone(),
        ]);
        if let Some(rocks) = self.rocks {
            argv.extend(["--expected-rocks-bench-sha256".into(), rocks.sha256.clone()]);
        }
    }

    fn idle_args(&self, argv: &mut Vec<String>, stage: &str) {
        argv.extend([
            "--lock-file".into(),
            p(self.input.lock_file),
            "--status-file".into(),
            p(&self
                .input
                .paths
                .status
                .join(format!("{}-{stage}.json", self.lane.name()))),
        ]);
    }

    fn watch_args(&self, argv: &mut Vec<String>) {
        if self.input.watch {
            argv.extend([
                "--watch".into(),
                "--busy-sleep".into(),
                py_float(self.input.busy_sleep),
            ]);
        }
    }

    fn stage(&self, name: &str, argv: Vec<String>) -> StageDump {
        StageDump {
            name: format!("{}-{name}", self.lane.name()),
            argv,
        }
    }

    fn stock(&self) -> StageDump {
        let mut argv = self.command("baseline-quick-idle");
        self.lane_repo(&mut argv);
        self.binary_args(&mut argv);
        argv.extend(["--run-id".into(), self.paths.stock_id.clone()]);
        self.idle_args(&mut argv, "stock");
        argv.extend([
            "--audit-json".into(),
            p(&self.paths.audit),
            "--audit-markdown".into(),
            p(&self.paths.audit.with_extension("md")),
        ]);
        self.watch_args(&mut argv);
        self.stage("stock", argv)
    }

    fn quality(&self) -> StageDump {
        let mut argv = self.command("audit-stock-short-pressure");
        argv.extend([
            p(&self.run_root.join(&self.paths.stock_id)),
            p(&self.paths.audit),
            "--lane".into(),
            self.lane.name().into(),
        ]);
        output_args(&mut argv, &self.paths.quality_initial);
        self.stage("quality-plan", argv)
    }

    fn followups(&self, pass: QualityPass) -> StageDump {
        let stage = format!("followups-{}", pass.name());
        let mut argv = self.command("baseline-sizing-followups");
        self.lane_repo(&mut argv);
        argv.extend([
            "--plan".into(),
            p(&self.paths.audit),
            "--quality-plan".into(),
            p(pass.path(self.paths)),
            "--run-prefix".into(),
            self.paths.resize_prefix.clone(),
        ]);
        self.binary_args(&mut argv);
        self.idle_args(&mut argv, &stage);
        self.watch_args(&mut argv);
        self.stage(&stage, argv)
    }

    fn refine(&self) -> StageDump {
        let mut argv = self.command("refine-stock-quality-repairs");
        argv.extend([
            p(&self.paths.quality_initial),
            p(&self.paths.audit),
            p(self.run_root),
            "--resize-prefix".into(),
            self.paths.resize_prefix.clone(),
        ]);
        output_args(&mut argv, &self.paths.quality_refined);
        self.stage("refine-quality", argv)
    }

    fn finalization_args(&self, argv: &mut Vec<String>) {
        let lane = self.lane.name();
        argv.extend([
            format!("--{lane}-stock-results"),
            p(&self
                .run_root
                .join(&self.paths.stock_id)
                .join("results.ndjson")),
            format!("--{lane}-audit"),
            p(&self.paths.audit),
            format!("--{lane}-resize-prefix"),
            self.paths.resize_prefix.clone(),
            format!("--{lane}-out-dir"),
            p(&self.input.paths.selected.join(lane)),
            format!("--{lane}-quality-repair-plan"),
            p(&self.paths.quality_refined),
        ]);
    }

    fn verify(&self) -> StageDump {
        let lane = self.lane.name();
        let mut argv = self.command(&format!("verify-{lane}-ready"));
        argv.extend([
            "--selected".into(),
            p(&self.input.paths.selected.join(lane)),
            "--binary-manifest".into(),
            p(self.manifest),
            "--expected-commit".into(),
            self.input.pins.commit.clone(),
            "--repo".into(),
            p(self.input.repo),
            "--stock-run-id".into(),
            self.paths.stock_id.clone(),
            "--expected-admission-policy".into(),
            ADMISSION_POLICY.into(),
            "--out".into(),
            p(&self.input.paths.ready.join(format!("{lane}.json"))),
        ]);
        StageDump {
            name: format!("verify-{lane}"),
            argv,
        }
    }
}

fn output_args(argv: &mut Vec<String>, path: &Path) {
    argv.extend([
        "--json-out".into(),
        p(path),
        "--markdown-out".into(),
        p(&path.with_extension("md")),
    ]);
}

fn stages(input: StagePlanArgs<'_>) -> Result<Vec<StageDump>> {
    let b = script(input.repo)?;
    let run_root = input.repo.join("results/runs");
    let kv = LanePlan::new(Lane::Kv, input, &b, &run_root);
    let record = LanePlan::new(Lane::Record, input, &b, &run_root);
    let mut finalize = kv.command("finalize-baseline-sizing");
    finalize.extend(["--repo".into(), p(input.repo)]);
    kv.finalization_args(&mut finalize);
    record.finalization_args(&mut finalize);
    Ok(vec![
        kv.stock(),
        record.stock(),
        kv.quality(),
        record.quality(),
        kv.followups(QualityPass::Initial),
        record.followups(QualityPass::Initial),
        kv.refine(),
        record.refine(),
        kv.followups(QualityPass::Refined),
        record.followups(QualityPass::Refined),
        StageDump {
            name: "finalize".into(),
            argv: finalize,
        },
        kv.verify(),
        record.verify(),
    ])
}

fn generation_status(state: &str, id: &str, commit: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("state".into(), json!(state));
    m.insert("generation_id".into(), json!(id));
    m.insert("commit".into(), json!(commit));
    m
}

pub fn run(args: GenerationArgs<'_>) -> Result<i32> {
    let pins = load_pins(
        args.repo,
        args.kv_binary_manifest,
        args.record_binary_manifest,
    )?;
    let paths = generation_paths(args.generation_root, args.generation_id);
    for dir in [&paths.plans, &paths.status, &paths.selected, &paths.ready] {
        fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
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
    fn generation_namespace_is_consistent() {
        let p = generation_paths(Path::new("/generation"), "refresh");
        assert_eq!(p.kv.stock_id, "refresh-kv-stock");
        assert_eq!(p.record.resize_prefix, "refresh-record-resize");
        assert_eq!(
            p.kv.audit,
            Path::new("/generation/plans/refresh-kv-sizing.json")
        );
    }
    #[test]
    fn stage_plan_has_thirteen_generation_scoped_steps() -> Result<()> {
        let root = temp()?;
        let repo = fake_repo(&root)?;
        let paths = generation_paths(&root.join("generation"), "g");
        let pins = Pins {
            commit: "d".repeat(40),
            kv: BinaryPin {
                path: root.join("kvbench"),
                sha256: "kvsha".into(),
            },
            record: BinaryPin {
                path: root.join("recordbench"),
                sha256: "recordsha".into(),
            },
            rocks: BinaryPin {
                path: root.join("rocks"),
                sha256: "rockssha".into(),
            },
        };
        let kv_manifest = root.join("kv.json");
        let record_manifest = root.join("record.json");
        let input = StagePlanArgs {
            repo: &repo,
            paths: &paths,
            pins: &pins,
            kv_manifest: &kv_manifest,
            record_manifest: &record_manifest,
            lock_file: Path::new("/run/lock/all-db-bench-performance.lock"),
            watch: true,
            busy_sleep: 3.0,
        };
        let normalize = |plan: &[StageDump]| {
            plan.iter()
                .map(|stage| {
                    format!(
                        "{}|{}",
                        stage.name,
                        stage
                            .argv
                            .join("|")
                            .replace(root.to_string_lossy().as_ref(), "$ROOT")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let expected = include_str!("baseline_generation.expected.txt").trim_end();
        assert_eq!(normalize(&stages(input)?), expected);
        assert_eq!(
            normalize(&stages(StagePlanArgs {
                watch: false,
                ..input
            })?),
            expected.replace("|--watch|--busy-sleep|3.0", "")
        );
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
