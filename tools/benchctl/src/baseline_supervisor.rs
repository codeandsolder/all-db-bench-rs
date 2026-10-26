use crate::{baseline_sizing, idle, noise, support};
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command, thread, time::Duration};

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

pub(crate) struct QuickArgs<'a> {
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

pub(crate) struct FollowupArgs<'a> {
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

fn json_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(v)) => *v,
        Some(Value::Number(v)) => v.as_f64().is_some_and(|n| n != 0.0),
        Some(Value::String(v)) => !v.is_empty(),
        Some(Value::Array(v)) => !v.is_empty(),
        Some(Value::Object(v)) => !v.is_empty(),
    }
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
fn read_json(path: &Path) -> Result<Value> {
    serde_json::from_str(
        &fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?,
    )
    .with_context(|| format!("invalid JSON in {}", path.display()))
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
fn sleep_duration(value: f64) -> Result<Duration> {
    if !value.is_finite() {
        bail!("busy sleep must be finite")
    };
    Ok(Duration::from_secs_f64(value.max(0.1)))
}

fn preflight(repo: &Path, max_io: f64, storage: bool) -> i32 {
    if storage {
        let rc = idle::storage_preflight(repo);
        if rc != 0 {
            return rc;
        }
    }
    let rc = idle::io_pressure_preflight(max_io, Path::new("/proc/pressure/io"));
    if rc != 0 {
        return rc;
    }
    noise::run_external(noise::ExternalNoiseArgs {
        ignore_nice_at_least: 15,
        json_out: Some(Path::new("/dev/null")),
        exclude_pids: &[],
        max_aggregate_cpu_percent: 50.0,
        sample_ms: 250.0,
    })
    .unwrap_or(2)
}

fn verify_expected(path: &Path, expected: Option<&str>) -> Result<()> {
    if let Some(expected) = expected {
        let actual = support::sha256(path)?;
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

pub(crate) fn run_quick(args: QuickArgs<'_>) -> Result<i32> {
    let lane = Lane::parse(args.lane)?;
    let sleep = sleep_duration(args.busy_sleep)?;
    if !executable(args.bench_bin) {
        return Ok(2);
    }
    if lane == Lane::Record && !args.rocks_bench_bin.is_some_and(executable) {
        return Ok(2);
    }
    let Some(_lock) = idle::acquire_lock(args.lock_file)? else {
        eprintln!(
            "benchmark campaign lock is already held: {}",
            args.lock_file.display()
        );
        return Ok(73);
    };
    let campaign = lane.campaign_quick();
    if quick_complete(lane, args.repo, args.run_id) {
        audit_quick(args.repo, args.run_id, args.audit_json, args.audit_markdown)?;
        let mut s = status("complete", campaign);
        s.insert("run_id".into(), json!(args.run_id));
        s.insert("busy_events".into(), json!(0));
        let _ = idle::write_status(args.status_file, s)?;
        println!(
            "{} quick already complete: {}",
            if lane == Lane::Kv { "KV" } else { "record" },
            args.run_id
        );
        return Ok(0);
    }
    let mut busy = 0_u64;
    let mut verified =
        args.expected_bench_sha256.is_none() && args.expected_rocks_bench_sha256.is_none();
    loop {
        let rc = preflight(args.repo, args.max_io_full_avg10, false);
        if rc == 75 {
            busy += 1;
            let mut s = status("waiting-for-idle", campaign);
            s.insert("run_id".into(), json!(args.run_id));
            s.insert("busy_events".into(), json!(busy));
            let _ = idle::write_status(args.status_file, s)?;
            if !args.watch {
                return Ok(75);
            };
            if busy == 1 || busy.is_multiple_of(30) {
                println!(
                    "{} quick waiting for idle: busy_events={busy}",
                    if lane == Lane::Kv { "KV" } else { "record" }
                );
            }
            thread::sleep(sleep);
            continue;
        }
        if rc != 0 {
            let mut s = status("failed", campaign);
            s.insert("run_id".into(), json!(args.run_id));
            s.insert("returncode".into(), json!(rc));
            let _ = idle::write_status(args.status_file, s)?;
            return Ok(rc);
        }
        if !verified {
            if let Err(error) = verify_expected(args.bench_bin, args.expected_bench_sha256) {
                let mut s = status("failed", campaign);
                s.insert("run_id".into(), json!(args.run_id));
                s.insert("error".into(), json!(error.to_string()));
                let _ = idle::write_status(args.status_file, s)?;
                return Ok(2);
            }
            if lane == Lane::Record
                && let Some(path) = args.rocks_bench_bin
                && let Err(error) = verify_expected(path, args.expected_rocks_bench_sha256)
            {
                let mut s = status("failed", campaign);
                s.insert("run_id".into(), json!(args.run_id));
                s.insert("error".into(), json!(error.to_string()));
                let _ = idle::write_status(args.status_file, s)?;
                return Ok(2);
            }
            verified = true;
        }
        let mut s = status("running", campaign);
        s.insert("run_id".into(), json!(args.run_id));
        s.insert("busy_events".into(), json!(busy));
        let _ = idle::write_status(args.status_file, s)?;
        let mut cmd = Command::new(args.repo.join("scripts").join(lane.matrix()));
        cmd.arg("quick")
            .current_dir(args.repo)
            .env("RUN_ID", args.run_id)
            .env("BENCH_BIN", args.bench_bin)
            .env("MATRIX_RESUME_SHUFFLE_REMAINING", "1");
        if let Some(rocks) = args.rocks_bench_bin {
            cmd.env("ROCKS_BENCH_BIN", rocks);
        }
        let output = cmd
            .output()
            .with_context(|| format!("failed to run {}", lane.matrix()))?;
        if !output.stdout.is_empty() {
            print!("{}", String::from_utf8_lossy(&output.stdout));
        }
        if !output.stderr.is_empty() {
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
        }
        let code = output.status.code().unwrap_or(2);
        if code == 0 {
            if !quick_complete(lane, args.repo, args.run_id) {
                let mut s = status("failed", campaign);
                s.insert("run_id".into(), json!(args.run_id));
                s.insert("error".into(), json!("runner success did not validate"));
                let _ = idle::write_status(args.status_file, s)?;
                return Ok(2);
            }
            if let Err(error) =
                audit_quick(args.repo, args.run_id, args.audit_json, args.audit_markdown)
            {
                let mut s = status("audit-failed", campaign);
                s.insert("run_id".into(), json!(args.run_id));
                s.insert("error".into(), json!(error.to_string()));
                let _ = idle::write_status(args.status_file, s)?;
                return Ok(2);
            }
            let mut s = status("complete", campaign);
            s.insert("run_id".into(), json!(args.run_id));
            s.insert("busy_events".into(), json!(busy));
            let _ = idle::write_status(args.status_file, s)?;
            println!(
                "{} quick complete and audited: {}",
                if lane == Lane::Kv { "KV" } else { "record" },
                args.run_id
            );
            return Ok(0);
        }
        if code != 75 {
            let mut s = status("failed", campaign);
            s.insert("run_id".into(), json!(args.run_id));
            s.insert("returncode".into(), json!(code));
            s.insert("busy_events".into(), json!(busy));
            let _ = idle::write_status(args.status_file, s)?;
            return Ok(code);
        }
        busy += 1;
        let mut s = status("waiting-for-idle", campaign);
        s.insert("run_id".into(), json!(args.run_id));
        s.insert("busy_events".into(), json!(busy));
        let _ = idle::write_status(args.status_file, s)?;
        if !args.watch {
            return Ok(75);
        };
        if busy == 1 || busy.is_multiple_of(30) {
            println!(
                "{} quick yielded after host became busy: busy_events={busy}",
                if lane == Lane::Kv { "KV" } else { "record" }
            );
        }
        thread::sleep(sleep);
    }
}

fn slug(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect()
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
fn run_id(group: &Value, prefix: &str) -> Result<String> {
    Ok(format!(
        "{prefix}-{}-{}-{}-e{}-t{}",
        slug(&group_string(group, "engine")),
        slug(&group_string(group, "durability")),
        slug(&group_string(group, "workload")),
        i64v(group.get("suggested_effective_ops"))?,
        i64v(group.get("suggested_trials"))?
    ))
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
            .into_iter()
            .flatten()
            .filter(|g| g.get("status").and_then(Value::as_str) == Some("undersized"))
            .cloned()
            .collect()
    };
    if let Some(path) = quality_path {
        let q = read_json(path)?;
        if i64v(q.get("quality_policy_version")).unwrap_or(-1) != 1 {
            bail!("unsupported stock quality policy")
        };
        groups.extend(
            q.get("groups")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .cloned(),
        );
    }
    fn order(g: &Value) -> i32 {
        match g.get("resize_strategy").and_then(Value::as_str) {
            Some("more-ops") => 0,
            Some("more-trials") => 1,
            Some("quality-repair") => 2,
            _ => 99,
        }
    }
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

fn followup_complete(repo: &Path, group: &Value, prefix: &str) -> bool {
    let Ok(rid) = run_id(group, prefix) else {
        return false;
    };
    let Ok(data) = read_json(&repo.join("results/runs").join(rid).join("summary.json")) else {
        return false;
    };
    let Ok(trials) = i64v(group.get("suggested_trials")) else {
        return false;
    };
    if i64v(data.get("row_count")).ok() != Some(trials)
        || i64v(data.get("group_count")).ok() != Some(1)
        || json_truthy(data.get("problems"))
    {
        return false;
    };
    let Some(gs) = data.get("groups").and_then(Value::as_array) else {
        return false;
    };
    if gs.len() != 1 {
        return false;
    };
    let r = &gs[0];
    r.get("engine") == group.get("engine")
        && r.get("durability") == group.get("durability")
        && r.get("workload") == group.get("workload")
        && i64v(r.get("ops_requested")).ok() == i64v(group.get("suggested_effective_ops")).ok()
        && r.get("trials").and_then(Value::as_array).is_some_and(|v| {
            v.iter()
                .enumerate()
                .all(|(i, x)| x.as_i64() == Some((i + 1) as i64))
                && v.len() == trials as usize
        })
}

fn median(mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let m = values.len() / 2;
    Some(if values.len().is_multiple_of(2) {
        (values[m - 1] + values[m]) / 2.0
    } else {
        values[m]
    })
}
fn median_elapsed(repo: &Path, group: &Value, prefix: &str) -> Result<f64> {
    let rid = run_id(group, prefix)?;
    let dir = repo.join("results/runs").join(&rid).join("cases");
    let mut values = Vec::new();
    for entry in fs::read_dir(&dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let path = entry?.path();
        if path.extension().and_then(|v| v.to_str()) != Some("json") {
            continue;
        }
        let row = read_json(&path)?;
        if row.get("engine") == group.get("engine")
            && row.get("durability") == group.get("durability")
            && row.get("workload") == group.get("workload")
            && i64v(row.get("ops_requested")).ok()
                == i64v(group.get("suggested_effective_ops")).ok()
        {
            values.push(f64v(row.get("elapsed_s"))?);
        }
    }
    let expected = i64v(group.get("suggested_trials"))? as usize;
    if values.len() != expected {
        bail!(
            "expected {expected} validated trials for {rid}, found {}",
            values.len()
        )
    }
    median(values).context("no elapsed values")
}
fn calibration_elapsed(
    repo: &Path,
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
    let elapsed = median_elapsed(repo, group, args.run_prefix)?;
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

pub(crate) fn run_followups(args: FollowupArgs<'_>) -> Result<i32> {
    let lane = Lane::parse(args.lane)?;
    let sleep = sleep_duration(args.busy_sleep)?;
    if !args.repo.is_dir() || !args.plan.is_file() || !executable(args.bench_bin) {
        return Ok(2);
    }
    if args.quality_plan.is_some_and(|p| !p.is_file()) {
        return Ok(2);
    }
    if lane == Lane::Record && !args.rocks_bench_bin.is_some_and(executable) {
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
        groups.truncate(limit)
    }
    let total = groups.len();
    if lane == Lane::Kv {
        println!(
            "planned={total} repo={} bench={} watch={}",
            args.repo.display(),
            args.bench_bin.display(),
            args.watch
        )
    } else {
        println!("record resize planned={total}")
    }
    let mut completed = 0_usize;
    let mut skipped = 0_usize;
    let mut busy = 0_u64;
    let mut verified =
        args.expected_bench_sha256.is_none() && args.expected_rocks_bench_sha256.is_none();
    for (offset, group) in groups.iter().enumerate() {
        let index = offset + 1;
        let rid = run_id(group, args.run_prefix)?;
        if followup_complete(args.repo, group, args.run_prefix) {
            skipped += 1;
            if lane == Lane::Kv {
                println!("[{index}/{total}] skip complete {rid}")
            }
            let cal = calibration_elapsed(args.repo, group, index, &args, lane);
            match cal {
                Ok(Some(v)) => println!(
                    "{}calibration {index}/{}: median_elapsed_s={v:.3}",
                    if lane == Lane::Record { "record " } else { "" },
                    args.calibration_count
                ),
                Ok(None) => {}
                Err(error) => {
                    let mut s = followup_status(
                        "calibration-failed",
                        lane,
                        Some(index),
                        total,
                        Some(&rid),
                        (completed, skipped, busy),
                    );
                    s.insert("error".into(), json!(error.to_string()));
                    let _ = idle::write_status(args.status_file, s)?;
                    eprintln!("{error}");
                    return Ok(2);
                }
            }
            continue;
        }
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
        loop {
            let rc = preflight(args.repo, args.max_io_full_avg10, true);
            if rc == 75 {
                busy += 1;
                let s = followup_status(
                    "waiting-for-idle",
                    lane,
                    Some(index),
                    total,
                    Some(&rid),
                    (completed, skipped, busy),
                );
                let _ = idle::write_status(args.status_file, s)?;
                if !args.watch {
                    return Ok(75);
                }
                if busy == 1 || busy.is_multiple_of(30) {
                    println!(
                        "{}waiting for idle: run={rid} busy_events={busy}",
                        if lane == Lane::Record {
                            "record resize "
                        } else {
                            ""
                        }
                    );
                }
                thread::sleep(sleep);
                continue;
            }
            if rc != 0 {
                let mut s = followup_status(
                    "failed",
                    lane,
                    Some(index),
                    total,
                    Some(&rid),
                    (completed, skipped, busy),
                );
                s.insert("returncode".into(), json!(rc));
                s.insert("error".into(), json!("host preflight failed"));
                let _ = idle::write_status(args.status_file, s)?;
                return Ok(rc);
            }
            if !verified {
                if let Err(error) = verify_expected(args.bench_bin, args.expected_bench_sha256) {
                    let mut s = followup_status(
                        "failed",
                        lane,
                        Some(index),
                        total,
                        Some(&rid),
                        (completed, skipped, busy),
                    );
                    s.insert("error".into(), json!(error.to_string()));
                    let _ = idle::write_status(args.status_file, s)?;
                    return Ok(2);
                }
                if lane == Lane::Record
                    && let Some(r) = args.rocks_bench_bin
                    && let Err(error) = verify_expected(r, args.expected_rocks_bench_sha256)
                {
                    let mut s = followup_status(
                        "failed",
                        lane,
                        Some(index),
                        total,
                        Some(&rid),
                        (completed, skipped, busy),
                    );
                    s.insert("error".into(), json!(error.to_string()));
                    let _ = idle::write_status(args.status_file, s)?;
                    return Ok(2);
                }
                verified = true
            }
            let s = followup_status(
                "running",
                lane,
                Some(index),
                total,
                Some(&rid),
                (completed, skipped, busy),
            );
            let _ = idle::write_status(args.status_file, s)?;
            let mut cmd = Command::new(args.repo.join("scripts").join(lane.matrix()));
            cmd.arg("quick").current_dir(args.repo);
            add_group_env(
                &mut cmd,
                lane,
                group,
                args.run_prefix,
                args.bench_bin,
                args.rocks_bench_bin,
            )?;
            let out = cmd
                .output()
                .with_context(|| format!("failed to run {}", lane.matrix()))?;
            if !out.stdout.is_empty() {
                print!("{}", String::from_utf8_lossy(&out.stdout));
            }
            if !out.stderr.is_empty() {
                eprint!("{}", String::from_utf8_lossy(&out.stderr));
            }
            let code = out.status.code().unwrap_or(2);
            if code == 0 {
                if !followup_complete(args.repo, group, args.run_prefix) {
                    return Ok(2);
                }
                completed += 1;
                break;
            }
            if code != 75 {
                let mut s = followup_status(
                    "failed",
                    lane,
                    Some(index),
                    total,
                    Some(&rid),
                    (completed, skipped, busy),
                );
                s.insert("returncode".into(), json!(code));
                let _ = idle::write_status(args.status_file, s)?;
                return Ok(code);
            }
            busy += 1;
            let s = followup_status(
                "waiting-for-idle",
                lane,
                Some(index),
                total,
                Some(&rid),
                (completed, skipped, busy),
            );
            let _ = idle::write_status(args.status_file, s)?;
            if !args.watch {
                return Ok(75);
            }
            if busy == 1 || busy.is_multiple_of(30) {
                println!(
                    "{}yielded after host became busy: run={rid} busy_events={busy}",
                    if lane == Lane::Record {
                        "record resize "
                    } else {
                        ""
                    }
                );
            }
            thread::sleep(sleep)
        }
        match calibration_elapsed(args.repo, group, index, &args, lane) {
            Ok(Some(v)) => println!(
                "{}calibration {index}/{}: median_elapsed_s={v:.3}",
                if lane == Lane::Record { "record " } else { "" },
                args.calibration_count
            ),
            Ok(None) => {}
            Err(error) => {
                let mut s = followup_status(
                    "calibration-failed",
                    lane,
                    Some(index),
                    total,
                    Some(&rid),
                    (completed, skipped, busy),
                );
                s.insert("error".into(), json!(error.to_string()));
                let _ = idle::write_status(args.status_file, s)?;
                return Ok(2);
            }
        }
    }
    let state = if args.dry_run {
        "dry-run-complete"
    } else {
        "complete"
    };
    let s = followup_status(state, lane, None, total, None, (completed, skipped, busy));
    let _ = idle::write_status(args.status_file, s)?;
    if lane == Lane::Kv {
        println!(
            "done completed_now={completed} skipped={skipped} planned={total} busy_events={busy}"
        )
    } else {
        println!(
            "record resize complete: completed_now={completed} skipped={skipped} total={total}"
        )
    }
    Ok(0)
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
        json!({"engine":"redb","durability":"sync","workload":"point-read","records":100000,"suggested_effective_ops":300000,"runner_ops_override":300000,"suggested_trials":3,"resize_strategy":"more-ops","median_elapsed_s":0.5,"status":"undersized"})
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
                &json!({"row_count":3,"group_count":1,"problems":[],"groups":[{"engine":"redb","durability":"sync","workload":"point-read","ops_requested":300000,"trials":[1,2,3]}]}),
            )?,
        )?;
        assert!(followup_complete(&p, &g, "x"));
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
