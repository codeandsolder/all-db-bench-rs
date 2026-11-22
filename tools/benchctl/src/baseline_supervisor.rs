use crate::admission::{AdmissionLoop, RunEvent, sleep_duration};
use crate::baseline_identity::{matches_followup, matches_trials, run_id};
use crate::baseline_sizing::TrialStatistics;
use crate::json_io::{read_json, read_ndjson};
use crate::json_value::json_truthy;
use crate::{baseline_sizing, file_hash, idle};
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command, time::Duration};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Lane {
    Kv,
    Record,
}
impl Lane {
    fn parse(raw: &str) -> Result<Self> {
        match raw {
            "kv" => Ok(Self::Kv),
            "record" => Ok(Self::Record),
            _ => bail!("unsupported baseline lane: {raw:?}"),
        }
    }
    const fn campaign_quick(self) -> &'static str {
        match self {
            Self::Kv => "kv-quick-stock",
            Self::Record => "record-quick-stock",
        }
    }
    const fn campaign_followup(self) -> &'static str {
        match self {
            Self::Kv => "raw-kv-sizing-followups",
            Self::Record => "record-sizing-followups",
        }
    }
    const fn matrix(self) -> &'static str {
        match self {
            Self::Kv => "run-kv-matrix.sh",
            Self::Record => "run-record-matrix.sh",
        }
    }
    const fn quick_shape(self) -> (i64, i64) {
        match self {
            Self::Kv => (540, 180),
            Self::Record => (120, 40),
        }
    }
}

#[derive(Clone, Copy)]
pub struct QuickArgs<'a> {
    pub(crate) lane: &'a str,
    pub(crate) repo: &'a Path,
    pub(crate) bench_bin: &'a Path,
    pub(crate) rocks_bench_bin: Option<&'a Path>,
    pub(crate) expected_bench_sha256: Option<&'a str>,
    pub(crate) expected_rocks_bench_sha256: Option<&'a str>,
    pub(crate) run_id: &'a str,
    pub(crate) lock_file: &'a Path,
    pub(crate) status_file: &'a Path,
    pub(crate) audit_json: &'a Path,
    pub(crate) audit_markdown: &'a Path,
    pub(crate) watch: bool,
    pub(crate) busy_sleep: f64,
    pub(crate) max_io_full_avg10: f64,
}

#[derive(Clone, Copy)]
pub struct FollowupArgs<'a> {
    pub(crate) lane: &'a str,
    pub(crate) repo: &'a Path,
    pub(crate) plan: &'a Path,
    pub(crate) quality_plan: Option<&'a Path>,
    pub(crate) run_prefix: &'a str,
    pub(crate) quality_only: bool,
    pub(crate) bench_bin: &'a Path,
    pub(crate) rocks_bench_bin: Option<&'a Path>,
    pub(crate) expected_bench_sha256: Option<&'a str>,
    pub(crate) expected_rocks_bench_sha256: Option<&'a str>,
    pub(crate) lock_file: &'a Path,
    pub(crate) status_file: &'a Path,
    pub(crate) limit: Option<usize>,
    pub(crate) watch: bool,
    pub(crate) busy_sleep: f64,
    pub(crate) max_io_full_avg10: f64,
    pub(crate) calibration_count: usize,
    pub(crate) calibration_min_seconds: f64,
    pub(crate) calibration_max_seconds: f64,
    pub(crate) dry_run: bool,
}

fn i64v(value: Option<&Value>) -> Result<i64> {
    match value.context("missing integer")? {
        Value::Number(n) => n.as_i64().context("integer out of range"),
        Value::String(s) => Ok(s.parse()?),
        other => bail!("expected integer, got {other}"),
    }
}
fn f64v(value: Option<&Value>) -> Result<f64> {
    match value.context("missing number")? {
        Value::Number(n) => n.as_f64().context("number out of range"),
        Value::String(s) => Ok(s.parse()?),
        other => bail!("expected number, got {other}"),
    }
}
fn executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}
fn status(state: &str, campaign: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("state".into(), json!(state));
    m.insert("campaign".into(), json!(campaign));
    m
}
fn verify_expected(path: &Path, expected: Option<&str>) -> Result<()> {
    if let Some(expected) = expected {
        let actual = file_hash::sha256(path)?;
        if actual != expected {
            bail!("benchmark binary SHA-256 mismatch: {}", path.display());
        }
    }
    Ok(())
}

fn quick_complete(lane: Lane, repo: &Path, run_id: &str) -> bool {
    let run = repo.join("results/runs").join(run_id);
    let Ok(summary) = read_json(&run.join("summary.json")) else {
        return false;
    };
    let (rows, groups) = lane.quick_shape();
    let failures = run.join("failures.ndjson");
    i64v(summary.get("row_count")).ok() == Some(rows)
        && i64v(summary.get("group_count")).ok() == Some(groups)
        && !json_truthy(summary.get("problems"))
        && !failures.metadata().is_ok_and(|m| m.len() > 0)
}

fn audit_quick(repo: &Path, run_id: &str, json_out: &Path, markdown_out: &Path) -> Result<()> {
    let input = repo
        .join("results/runs")
        .join(run_id)
        .join("results.ndjson");
    let code = baseline_sizing::run_audit(baseline_sizing::AuditArgs {
        results: &input,
        expect_trials: 3,
        config: baseline_sizing::SizingConfig::default(),
        json_out: Some(json_out),
        markdown_out: Some(markdown_out),
    })?;
    if code != 0 {
        bail!("sizing audit failed rc={code}");
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct BinaryPins<'a> {
    primary: &'a Path,
    primary_hash: Option<&'a str>,
    rocks: Option<&'a Path>,
    rocks_hash: Option<&'a str>,
}

impl BinaryPins<'_> {
    fn executable(self, lane: Lane) -> bool {
        executable(self.primary) && (lane == Lane::Kv || self.rocks.is_some_and(executable))
    }

    fn verify(self, lane: Lane) -> Result<()> {
        verify_expected(self.primary, self.primary_hash)?;
        if lane == Lane::Record {
            verify_expected(self.rocks.context("missing Rocks binary")?, self.rocks_hash)?;
        }
        Ok(())
    }
}

struct MatrixRunner<'a> {
    repo: &'a Path,
    lane: Lane,
    pins: BinaryPins<'a>,
    max_io: f64,
    verified: bool,
    admission: AdmissionLoop,
}

impl<'a> MatrixRunner<'a> {
    const fn new(
        repo: &'a Path,
        lane: Lane,
        pins: BinaryPins<'a>,
        max_io: f64,
        watch: bool,
        sleep: Duration,
    ) -> Self {
        Self {
            repo,
            lane,
            pins,
            max_io,
            verified: false,
            admission: AdmissionLoop::new(watch, sleep),
        }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(self.repo.join("scripts").join(self.lane.matrix()));
        cmd.arg("quick").current_dir(self.repo);
        cmd
    }

    fn run(
        &mut self,
        cmd: &mut Command,
        storage: bool,
        report: impl FnMut(RunEvent<'_>, u64) -> Result<()>,
    ) -> Result<i32> {
        self.admission.run(
            || {
                let rc = idle::host_preflight(self.repo, self.max_io, storage);
                if rc == 0 && !self.verified {
                    self.pins.verify(self.lane)?;
                    self.verified = true;
                }
                Ok(rc)
            },
            || {
                let output = cmd
                    .output()
                    .with_context(|| format!("failed to run {}", self.lane.matrix()))?;
                if !output.stdout.is_empty() {
                    print!("{}", String::from_utf8_lossy(&output.stdout));
                }
                if !output.stderr.is_empty() {
                    eprint!("{}", String::from_utf8_lossy(&output.stderr));
                }
                Ok(output.status.code().unwrap_or(2))
            },
            report,
        )
    }
}

fn quick_status(args: &QuickArgs<'_>, lane: Lane, state: &str, busy: u64) -> Map<String, Value> {
    let mut fields = status(state, lane.campaign_quick());
    fields.insert("run_id".into(), json!(args.run_id));
    fields.insert("busy_events".into(), json!(busy));
    fields
}

fn finish_quick(args: &QuickArgs<'_>, lane: Lane, busy: u64) -> Result<i32> {
    let (state, error) = if !quick_complete(lane, args.repo, args.run_id) {
        ("failed", Some("runner success did not validate".to_owned()))
    } else if let Err(error) =
        audit_quick(args.repo, args.run_id, args.audit_json, args.audit_markdown)
    {
        ("audit-failed", Some(error.to_string()))
    } else {
        ("complete", None)
    };
    let mut fields = quick_status(args, lane, state, busy);
    let code = error.map_or_else(
        || {
            println!(
                "{} quick complete and audited: {}",
                if lane == Lane::Kv { "KV" } else { "record" },
                args.run_id
            );
            0
        },
        |error| {
            fields.insert("error".into(), json!(error));
            2
        },
    );
    let _ = idle::write_status(args.status_file, fields)?;
    Ok(code)
}

pub fn run_quick(args: QuickArgs<'_>) -> Result<i32> {
    let lane = Lane::parse(args.lane)?;
    let sleep = sleep_duration(args.busy_sleep)?;
    let pins = BinaryPins {
        primary: args.bench_bin,
        primary_hash: args.expected_bench_sha256,
        rocks: args.rocks_bench_bin,
        rocks_hash: args.expected_rocks_bench_sha256,
    };
    if !pins.executable(lane) {
        return Ok(2);
    }
    let Some(_lock) = idle::acquire_lock(args.lock_file)? else {
        eprintln!(
            "benchmark campaign lock is already held: {}",
            args.lock_file.display()
        );
        return Ok(73);
    };
    if quick_complete(lane, args.repo, args.run_id) {
        return finish_quick(&args, lane, 0);
    }
    let mut runner = MatrixRunner::new(
        args.repo,
        lane,
        pins,
        args.max_io_full_avg10,
        args.watch,
        sleep,
    );
    let mut cmd = runner.command();
    cmd.env("RUN_ID", args.run_id)
        .env("BENCH_BIN", args.bench_bin)
        .env("MATRIX_RESUME_SHUFFLE_REMAINING", "1");
    if let Some(rocks) = args.rocks_bench_bin {
        cmd.env("ROCKS_BENCH_BIN", rocks);
    }
    let code = runner.run(&mut cmd, false, |event, busy| {
        let fields = event.fields(quick_status(&args, lane, event.state(), busy));
        let _ = idle::write_status(args.status_file, fields)?;
        Ok(())
    })?;
    if code == 0 {
        finish_quick(&args, lane, runner.admission.busy_events())
    } else {
        Ok(code)
    }
}

fn group_string(group: &Value, field: &str) -> String {
    group.get(field).map_or_else(
        || "None".into(),
        |v| match v {
            Value::String(s) => s.clone(),
            _ => v.to_string(),
        },
    )
}
fn groups_from_plan(
    plan_path: &Path,
    quality_path: Option<&Path>,
    quality_only: bool,
) -> Result<Vec<Value>> {
    let plan = read_json(plan_path)?;
    let version = i64v(plan.get("sizing_policy_version")).unwrap_or(-1);
    if !matches!(version, 2 | 3) {
        bail!("unsupported sizing policy: {version}")
    }
    let mut groups = if quality_only {
        Vec::new()
    } else {
        plan.get("groups")
            .and_then(Value::as_array)
            .context("sizing plan groups missing")?
            .iter()
            .filter(|g| g.get("status").and_then(Value::as_str) == Some("undersized"))
            .cloned()
            .collect()
    };
    if let Some(path) = quality_path {
        let q = read_json(path)?;
        if i64v(q.get("quality_policy_version")).unwrap_or(-1) != 1 {
            bail!("unsupported stock quality policy")
        }
        groups.extend(
            q.get("groups")
                .and_then(Value::as_array)
                .context("quality plan groups missing")?
                .iter()
                .cloned(),
        );
    }
    let order = |group: &Value| match group.get("resize_strategy").and_then(Value::as_str) {
        Some("more-ops") => 0,
        Some("more-trials") => 1,
        Some("quality-repair") => 2,
        _ => 99,
    };
    groups.sort_by(|a, b| {
        order(a)
            .cmp(&order(b))
            .then_with(|| {
                f64v(a.get("median_elapsed_s"))
                    .unwrap_or(f64::INFINITY)
                    .total_cmp(&f64v(b.get("median_elapsed_s")).unwrap_or(f64::INFINITY))
            })
            .then_with(|| group_string(a, "engine").cmp(&group_string(b, "engine")))
            .then_with(|| group_string(a, "durability").cmp(&group_string(b, "durability")))
            .then_with(|| group_string(a, "workload").cmp(&group_string(b, "workload")))
    });
    Ok(groups)
}

fn followup_evidence(repo: &Path, group: &Value, prefix: &str) -> Result<TrialStatistics> {
    let rid = run_id(group, prefix)?;
    let dir = repo.join("results/runs").join(&rid);
    if dir
        .join("failures.ndjson")
        .metadata()
        .is_ok_and(|m| m.len() > 0)
    {
        bail!("unresolved failures in {rid}");
    }
    let data = read_json(&dir.join("summary.json"))?;
    let trials = i64v(group.get("suggested_trials"))?;
    if i64v(data.get("row_count"))? != trials
        || i64v(data.get("group_count"))? != 1
        || json_truthy(data.get("problems"))
    {
        bail!("invalid followup summary: {rid}");
    }
    let gs = data
        .get("groups")
        .and_then(Value::as_array)
        .context("summary groups missing")?;
    let [summary] = gs.as_slice() else {
        bail!("wrong summary cardinality: {rid}");
    };
    let summary_trials = summary
        .get("trials")
        .and_then(Value::as_array)
        .and_then(|values| values.iter().map(Value::as_i64).collect::<Option<Vec<_>>>())
        .context("invalid summary trial IDs")?;
    if !matches_followup(group, summary) || !matches_trials(&summary_trials, trials) {
        bail!("followup summary identity or trials mismatch: {rid}");
    }
    let rows = read_ndjson(&dir.join("results.ndjson"))?;
    if rows.iter().any(|row| !matches_followup(group, row)) {
        bail!("followup result identity mismatch: {rid}");
    }
    let evidence = TrialStatistics::from_rows(&rows.iter().collect::<Vec<_>>())?;
    if !matches_trials(&evidence.trials, trials) {
        bail!(
            "expected trials 1..={trials} for {rid}, found {:?}",
            evidence.trials
        );
    }
    Ok(evidence)
}

fn calibration_elapsed(
    elapsed: f64,
    group: &Value,
    index: usize,
    args: &FollowupArgs<'_>,
    lane: Lane,
) -> Result<Option<f64>> {
    if group.get("resize_strategy").and_then(Value::as_str) != Some("more-ops")
        || index > args.calibration_count
    {
        return Ok(None);
    }
    if !(args.calibration_min_seconds..=args.calibration_max_seconds).contains(&elapsed) {
        let label = if lane == Lane::Record {
            "record resize calibration"
        } else {
            "resize calibration"
        };
        bail!(
            "{label} outside [{:.3}, {:.3}] s for {}: median_elapsed_s={elapsed:.3}",
            args.calibration_min_seconds,
            args.calibration_max_seconds,
            run_id(group, args.run_prefix)?
        );
    }
    Ok(Some(elapsed))
}

fn followup_status(
    state: &str,
    lane: Lane,
    index: Option<usize>,
    total: usize,
    rid: Option<&str>,
    counters: (usize, usize, u64),
) -> Map<String, Value> {
    let (completed, skipped, busy) = counters;
    let mut s = status(state, lane.campaign_followup());
    s.insert("total".into(), json!(total));
    s.insert("completed_now".into(), json!(completed));
    s.insert("skipped".into(), json!(skipped));
    s.insert("busy_events".into(), json!(busy));
    if let Some(i) = index {
        s.insert("current_index".into(), json!(i));
    }
    if let Some(r) = rid {
        s.insert("current_run_id".into(), json!(r));
    }
    s
}
fn add_group_env(
    cmd: &mut Command,
    lane: Lane,
    group: &Value,
    prefix: &str,
    bench: &Path,
    rocks: Option<&Path>,
) -> Result<()> {
    cmd.env("RUN_ID", run_id(group, prefix)?)
        .env("BENCH_BIN", bench)
        .env("ENGINES_OVERRIDE", group_string(group, "engine"))
        .env("DURABILITIES_OVERRIDE", group_string(group, "durability"))
        .env("WORKLOADS_OVERRIDE", group_string(group, "workload"));
    let ops = i64v(group.get("runner_ops_override"))?.to_string();
    let records = i64v(group.get("records"))?.to_string();
    let trials = i64v(group.get("suggested_trials"))?.to_string();
    match lane {
        Lane::Kv => {
            cmd.env("KV_OPS_OVERRIDE", ops)
                .env("KV_RECORDS_OVERRIDE", records)
                .env("KV_TRIALS_OVERRIDE", trials);
        }
        Lane::Record => {
            cmd.env("RECORD_OPS_OVERRIDE", ops)
                .env("RECORD_RECORDS_OVERRIDE", records)
                .env("RECORD_TRIALS_OVERRIDE", trials);
            if let Some(r) = rocks {
                cmd.env("ROCKS_BENCH_BIN", r);
            }
        }
    }
    Ok(())
}

struct FollowupProgress {
    lane: Lane,
    total: usize,
    completed: usize,
    skipped: usize,
}

impl FollowupProgress {
    fn fields(&self, state: &str, current: Option<(usize, &str)>, busy: u64) -> Map<String, Value> {
        let (index, rid) = current.map_or((None, None), |(index, rid)| (Some(index), Some(rid)));
        followup_status(
            state,
            self.lane,
            index,
            self.total,
            rid,
            (self.completed, self.skipped, busy),
        )
    }

    fn calibrate(
        &self,
        group: &Value,
        index: usize,
        rid: &str,
        args: &FollowupArgs<'_>,
        busy: u64,
        elapsed: f64,
    ) -> Result<bool> {
        match calibration_elapsed(elapsed, group, index, args, self.lane) {
            Ok(Some(v)) => println!(
                "{}calibration {index}/{}: median_elapsed_s={v:.3}",
                if self.lane == Lane::Record {
                    "record "
                } else {
                    ""
                },
                args.calibration_count
            ),
            Ok(None) => {}
            Err(error) => {
                let mut fields = self.fields("calibration-failed", Some((index, rid)), busy);
                fields.insert("error".into(), json!(error.to_string()));
                let _ = idle::write_status(args.status_file, fields)?;
                eprintln!("{error}");
                return Ok(false);
            }
        }
        Ok(true)
    }
    fn finish(&self, args: &FollowupArgs<'_>, busy: u64) -> Result<i32> {
        let state = if args.dry_run {
            "dry-run-complete"
        } else {
            "complete"
        };
        let _ = idle::write_status(args.status_file, self.fields(state, None, busy))?;
        println!(
            "done completed_now={} skipped={} planned={} busy_events={busy}",
            self.completed, self.skipped, self.total
        );
        Ok(0)
    }
}

fn run_followup(
    runner: &mut MatrixRunner<'_>,
    progress: &FollowupProgress,
    args: &FollowupArgs<'_>,
    group: &Value,
    index: usize,
    rid: &str,
) -> Result<std::result::Result<TrialStatistics, i32>> {
    let current = Some((index, rid));
    let mut cmd = runner.command();
    add_group_env(
        &mut cmd,
        progress.lane,
        group,
        args.run_prefix,
        args.bench_bin,
        args.rocks_bench_bin,
    )?;
    let code = runner.run(&mut cmd, true, |event, busy| {
        let fields = event.fields(progress.fields(event.state(), current, busy));
        let _ = idle::write_status(args.status_file, fields)?;
        Ok(())
    })?;
    if code != 0 {
        return Ok(Err(code));
    }
    match followup_evidence(args.repo, group, args.run_prefix) {
        Ok(evidence) => Ok(Ok(evidence)),
        Err(error) => {
            let mut fields = progress.fields("failed", current, runner.admission.busy_events());
            fields.insert(
                "error".into(),
                json!(format!("runner success did not validate: {error}")),
            );
            let _ = idle::write_status(args.status_file, fields)?;
            Ok(Err(2))
        }
    }
}

pub fn run_followups(args: FollowupArgs<'_>) -> Result<i32> {
    let lane = Lane::parse(args.lane)?;
    let sleep = sleep_duration(args.busy_sleep)?;
    let pins = BinaryPins {
        primary: args.bench_bin,
        primary_hash: args.expected_bench_sha256,
        rocks: args.rocks_bench_bin,
        rocks_hash: args.expected_rocks_bench_sha256,
    };
    if !args.repo.is_dir()
        || !args.plan.is_file()
        || !pins.executable(lane)
        || args.quality_plan.is_some_and(|p| !p.is_file())
    {
        return Ok(2);
    }
    let Some(_lock) = idle::acquire_lock(args.lock_file)? else {
        eprintln!(
            "benchmark campaign lock is already held: {}",
            args.lock_file.display()
        );
        return Ok(73);
    };
    let mut groups = groups_from_plan(args.plan, args.quality_plan, args.quality_only)?;
    if let Some(limit) = args.limit {
        groups.truncate(limit);
    }
    let total = groups.len();
    println!(
        "planned={total} repo={} bench={} watch={}",
        args.repo.display(),
        args.bench_bin.display(),
        args.watch
    );
    let mut progress = FollowupProgress {
        lane,
        total,
        completed: 0,
        skipped: 0,
    };
    let mut runner = MatrixRunner::new(
        args.repo,
        lane,
        pins,
        args.max_io_full_avg10,
        args.watch,
        sleep,
    );
    for (offset, group) in groups.iter().enumerate() {
        let index = offset + 1;
        let rid = run_id(group, args.run_prefix)?;
        let evidence = if let Ok(evidence) = followup_evidence(args.repo, group, args.run_prefix) {
            progress.skipped += 1;
            println!("[{index}/{total}] skip complete {rid}");
            evidence
        } else {
            println!(
                "[{index}/{total}] {rid} strategy={} effective_ops={} trials={} runner_ops={}",
                group_string(group, "resize_strategy"),
                i64v(group.get("suggested_effective_ops"))?,
                i64v(group.get("suggested_trials"))?,
                i64v(group.get("runner_ops_override"))?
            );
            if args.dry_run {
                continue;
            }
            match run_followup(&mut runner, &progress, &args, group, index, &rid)? {
                Ok(evidence) => {
                    progress.completed += 1;
                    evidence
                }
                Err(code) => return Ok(code),
            }
        };
        if !progress.calibrate(
            group,
            index,
            &rid,
            &args,
            runner.admission.busy_events(),
            evidence.median_elapsed,
        )? {
            return Ok(2);
        }
    }
    progress.finish(&args, runner.admission.busy_events())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use serde_json::json;
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };
    fn temp() -> Result<PathBuf> {
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "baseline-supervisor-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p)?;
        Ok(p)
    }
    fn group() -> Value {
        json!({"engine":"redb","durability":"sync","workload":"point-read","records":100_000,"suggested_effective_ops":300_000,"runner_ops_override":300_000,"suggested_trials":3,"resize_strategy":"more-ops","median_elapsed_s":0.5,"status":"undersized"})
    }
    #[test]
    fn run_id_matches_python() -> Result<()> {
        assert_eq!(
            run_id(&group(), "refresh")?,
            "refresh-redb-sync-point-read-e300000-t3"
        );
        Ok(())
    }
    #[test]
    fn v3_plans_are_accepted_for_kv_followups() -> Result<()> {
        let p = temp()?;
        let plan = p.join("plan.json");
        fs::write(
            &plan,
            serde_json::to_string(&json!({"sizing_policy_version":3,"groups":[group()]}))?,
        )?;
        assert_eq!(groups_from_plan(&plan, None, false)?.len(), 1);
        fs::remove_dir_all(p)?;
        Ok(())
    }
    #[test]
    fn complete_requires_identity_and_trials() -> Result<()> {
        let p = temp()?;
        let g = group();
        let rid = run_id(&g, "x")?;
        let d = p.join("results/runs").join(rid);
        fs::create_dir_all(&d)?;
        fs::write(
            d.join("summary.json"),
            serde_json::to_string(
                &json!({"row_count":3,"group_count":1,"problems":[],"groups":[{"engine":"redb","durability":"sync","workload":"point-read","records":100_000,"ops_requested":300_000,"trials":[1,2,3]}]}),
            )?,
        )?;
        assert!(followup_evidence(&p, &g, "x").is_err());
        let rows = (1..=3).map(|trial| json!({"engine":"redb","durability":"sync","workload":"point-read","records":100_000,
            "ops_requested":300_000,"ops_completed":300_000,"trial":trial,"elapsed_s":3.0,"ops_per_s":100_000.0}))
            .collect::<Vec<_>>();
        let write_rows = |rows: &[Value]| -> Result<()> {
            let content = rows
                .iter()
                .map(|row| serde_json::to_string(row).map(|s| s + "\n"))
                .collect::<std::result::Result<String, _>>()?;
            fs::write(d.join("results.ndjson"), content)?;
            Ok(())
        };
        write_rows(&rows)?;
        assert!(followup_evidence(&p, &g, "x").is_ok());
        let mut legacy = g.clone();
        legacy["read_materialization"] = json!("legacy-read-v0");
        legacy["write_materialization"] = json!("legacy-return-v0");
        assert!(followup_evidence(&p, &legacy, "x").is_ok());
        let mut bad = g.clone();
        bad["records"] = json!(99);
        assert!(followup_evidence(&p, &bad, "x").is_err());
        let mut bad_rows = rows.clone();
        bad_rows[1]["trial"] = json!(1);
        write_rows(&bad_rows)?;
        assert!(followup_evidence(&p, &g, "x").is_err());
        bad_rows[1]["trial"] = json!(2);
        bad_rows[1]["ops_completed"] = json!(99);
        write_rows(&bad_rows)?;
        assert!(followup_evidence(&p, &g, "x").is_err());
        write_rows(&rows)?;
        fs::write(d.join("failures.ndjson"), "failed")?;
        assert!(followup_evidence(&p, &g, "x").is_err());
        fs::remove_dir_all(p)?;
        Ok(())
    }
    #[test]
    fn quick_completion_shapes_are_lane_specific() -> Result<()> {
        let p = temp()?;
        for (lane, id, rows, groups) in [
            (Lane::Kv, "kv", 540, 180),
            (Lane::Record, "record", 120, 40),
        ] {
            let d = p.join("results/runs").join(id);
            fs::create_dir_all(&d)?;
            fs::write(
                d.join("summary.json"),
                serde_json::to_string(
                    &json!({"row_count":rows,"group_count":groups,"problems":[]}),
                )?,
            )?;
            assert!(quick_complete(lane, &p, id));
        }
        fs::remove_dir_all(p)?;
        Ok(())
    }
}
