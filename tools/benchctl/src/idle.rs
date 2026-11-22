use crate::admission::{AdmissionLoop, sleep_duration};
use crate::json_io::{read_json, read_ndjson};
use crate::support::{
    CONTINUOUS_ADMISSION, canonical_identity_sha256, support_identity, verify_plan_identity,
};
use anyhow::{Context, Result, bail};
use fs2::FileExt;
use num_traits::ToPrimitive;
use serde_json::{Map, Value, json};
use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
};

const DEFAULT_LOCK_FILE: &str = "/run/lock/all-db-bench-performance.lock";
const DEFAULT_MIN_FREE_GIB: f64 = 8.0;
const MAX_IO_FULL_AVG10: f64 = 5.0;
const RETRYABLE_STATUS_ERRNOS: [i32; 2] = [28, 122];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lane {
    Kv,
    Record,
}

impl Lane {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "kv-concurrency" | "kv" => Ok(Self::Kv),
            "record-concurrency" | "record" => Ok(Self::Record),
            _ => bail!("unsupported concurrency idle lane: {value:?}"),
        }
    }

    const fn support_lane(self) -> &'static str {
        match self {
            Self::Kv => "kv-concurrency",
            Self::Record => "record-concurrency",
        }
    }

    const fn runner_name(self) -> &'static str {
        match self {
            Self::Kv => "run-kv-concurrency-plan.sh",
            Self::Record => "run-record-concurrency-plan.sh",
        }
    }
}

#[derive(Clone, Copy)]
pub struct IdleArgs<'a> {
    pub(crate) lane: &'a str,
    pub(crate) repo: &'a Path,
    pub(crate) plan: &'a Path,
    pub(crate) run_id: &'a str,
    pub(crate) bench_bin: &'a Path,
    pub(crate) bench_bin_sha256: &'a str,
    pub(crate) rocks_bench_bin: Option<&'a Path>,
    pub(crate) rocks_bench_bin_sha256: Option<&'a str>,
    pub(crate) lock_file: Option<&'a Path>,
    pub(crate) status_file: &'a Path,
    pub(crate) busy_sleep_s: f64,
}

pub fn verify_binary(path: &Path, expected_sha256: &str) -> Result<()> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("pinned binary is not accessible: {}", path.display()))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        bail!("pinned binary is not executable: {}", path.display());
    }
    let actual = crate::file_hash::sha256(path)?;
    if actual != expected_sha256 {
        bail!("pinned binary SHA-256 mismatch: expected {expected_sha256}, got {actual}");
    }
    Ok(())
}

pub struct CampaignLock(File);

impl Drop for CampaignLock {
    fn drop(&mut self) {
        // Explicitly release the open-file-description lock even if a child
        // created by a concurrent fork still holds an inherited descriptor.
        let _ = FileExt::unlock(&self.0);
    }
}

pub fn acquire_lock(path: &Path) -> Result<Option<CampaignLock>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create lock directory {}", parent.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("failed to open campaign lock {}", path.display()))?;
    match file.try_lock_exclusive() {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to lock campaign lock {}", path.display()));
        }
    }
    let mut guard = CampaignLock(file);
    let file = &mut guard.0;
    file.set_len(0)
        .with_context(|| format!("failed to truncate campaign lock {}", path.display()))?;
    writeln!(file, "pid={}", std::process::id())
        .with_context(|| format!("failed to write campaign lock {}", path.display()))?;
    file.flush()
        .with_context(|| format!("failed to flush campaign lock {}", path.display()))?;
    Ok(Some(guard))
}

fn utc_now() -> Result<String> {
    let output = Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%S.%6N+00:00"])
        .output()
        .context("failed to run date for supervisor status timestamp")?;
    if !output.status.success() {
        bail!(
            "date failed while creating supervisor status timestamp: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8(output.stdout)
        .context("date timestamp was not UTF-8")?
        .trim()
        .to_owned())
}

fn status_tmp_path(path: &Path) -> Result<PathBuf> {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("status path has no UTF-8 file name")?;
    Ok(path.with_file_name(format!("{name}.tmp")))
}

pub fn write_status(path: &Path, mut fields: Map<String, Value>) -> Result<bool> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create status directory {}", parent.display()))?;
    }
    fields.insert("updated_at".to_owned(), json!(utc_now()?));
    fields.insert("pid".to_owned(), json!(std::process::id()));
    let waiting = fields.get("state").and_then(Value::as_str) == Some("waiting-for-idle");
    let tmp = status_tmp_path(path)?;
    let encoded = serde_json::to_string_pretty(&Value::Object(fields))? + "\n";
    let result = fs::write(&tmp, encoded).and_then(|()| fs::rename(&tmp, path));
    match result {
        Ok(()) => Ok(true),
        Err(error)
            if waiting
                && error
                    .raw_os_error()
                    .is_some_and(|errno| RETRYABLE_STATUS_ERRNOS.contains(&errno)) =>
        {
            let _ = fs::remove_file(&tmp);
            Ok(false)
        }
        Err(error) => {
            let _ = fs::remove_file(&tmp);
            Err(error).with_context(|| format!("failed to write status {}", path.display()))
        }
    }
}

fn status_fields(
    state: &str,
    run_id: Option<&str>,
    busy_events: Option<u64>,
) -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert("state".to_owned(), json!(state));
    if let Some(run_id) = run_id {
        fields.insert("run_id".to_owned(), json!(run_id));
    }
    if let Some(busy_events) = busy_events {
        fields.insert("busy_events".to_owned(), json!(busy_events));
    }
    fields
}

fn status_failure(
    path: &Path,
    run_id: Option<&str>,
    returncode: Option<i32>,
    error: Option<&str>,
) -> Result<()> {
    let mut fields = status_fields("failed", run_id, None);
    if let Some(returncode) = returncode {
        fields.insert("returncode".to_owned(), json!(returncode));
    }
    if let Some(error) = error {
        fields.insert("error".to_owned(), json!(error));
    }
    let _ = write_status(path, fields)?;
    Ok(())
}

pub fn minimum_free_bytes(raw: &str) -> Result<u64> {
    let gib = raw
        .parse::<f64>()
        .with_context(|| format!("invalid PERFORMANCE_MIN_FREE_GIB={raw:?}"))?;
    if !gib.is_finite() || gib < 0.0 {
        bail!("PERFORMANCE_MIN_FREE_GIB must be non-negative");
    }
    let bytes = gib * 1024_f64.powi(3);
    bytes
        .to_u64()
        .context("PERFORMANCE_MIN_FREE_GIB is too large")
}

pub fn available_bytes(path: &Path) -> Result<u64> {
    let output = Command::new("df")
        .args(["-B1", "--output=avail", "--"])
        .arg(path)
        .output()
        .with_context(|| format!("failed to run df for {}", path.display()))?;
    if !output.status.success() {
        bail!(
            "df failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let text = String::from_utf8(output.stdout).context("df output was not UTF-8")?;
    let raw = text
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .context("df output did not contain available bytes")?
        .trim();
    raw.parse::<u64>()
        .with_context(|| format!("invalid df available-byte value {raw:?}"))
}

pub fn storage_preflight(repo: &Path) -> i32 {
    let root = env::var_os("DBBENCH_STORAGE_ROOT").map_or_else(|| repo.to_owned(), PathBuf::from);
    let raw_min =
        env::var("PERFORMANCE_MIN_FREE_GIB").unwrap_or_else(|_| DEFAULT_MIN_FREE_GIB.to_string());
    let Ok(required) = minimum_free_bytes(&raw_min) else {
        return 2;
    };
    let Ok(free) = available_bytes(&root) else {
        return 2;
    };
    if free < required { 75 } else { 0 }
}

pub fn parse_io_full_avg10(raw: &str) -> Result<f64> {
    for line in raw.lines() {
        let mut fields = line.split_whitespace();
        if fields.next() != Some("full") {
            continue;
        }
        for field in fields {
            if let Some(value) = field.strip_prefix("avg10=") {
                return value
                    .parse::<f64>()
                    .with_context(|| format!("invalid /proc/pressure/io avg10 value {value:?}"));
            }
        }
    }
    bail!("/proc/pressure/io did not contain full avg10")
}

pub fn io_pressure_preflight(max_io_full_avg10: f64, pressure_path: &Path) -> i32 {
    if env::var("DBBENCH_STORAGE_PROFILE_MODE").as_deref() == Ok("bounded") {
        return 0;
    }
    let Ok(raw) = fs::read_to_string(pressure_path) else {
        return 2;
    };
    let Ok(pressure) = parse_io_full_avg10(&raw) else {
        return 2;
    };
    if pressure > max_io_full_avg10 { 75 } else { 0 }
}

pub fn host_preflight(repo: &Path, max_io_full: f64, storage: bool) -> i32 {
    if storage {
        let storage_rc = storage_preflight(repo);
        if storage_rc != 0 {
            return storage_rc;
        }
    }
    let pressure_rc = io_pressure_preflight(max_io_full, Path::new("/proc/pressure/io"));
    if pressure_rc != 0 {
        return pressure_rc;
    }
    crate::noise::run_external(crate::noise::ExternalNoiseArgs {
        ignore_nice_at_least: 15,
        json_out: Some(Path::new("/dev/null")),
        exclude_pids: &[],
        max_aggregate_cpu_percent: 50.0,
        sample_ms: 250.0,
    })
    .unwrap_or(2)
}

fn value_as_i64(value: Option<&Value>) -> Option<i64> {
    value.and_then(|value| match value {
        Value::Number(number) => number.as_i64(),
        Value::String(raw) => raw.parse::<i64>().ok(),
        _ => None,
    })
}

fn plan_version(path: &Path) -> Result<i64> {
    let plan = read_json(path)?;
    let version = value_as_i64(plan.get("concurrency_probe_plan_version")).unwrap_or(-1);
    if !matches!(version, 1 | 2) {
        bail!("unsupported concurrency plan version: {version}");
    }
    Ok(version)
}

fn verify_plan_support(
    lane: Lane,
    repo: &Path,
    support: &Value,
    plan_path: &Path,
    rows: &[Value],
) -> Result<()> {
    use sha2::{Digest, Sha256};
    let bytes = fs::read(plan_path)?;
    let plan: Value = serde_json::from_slice(&bytes)?;
    let sha = crate::file_hash::lower_hex(&Sha256::digest(&bytes));
    if support.get("plan_sha256").and_then(Value::as_str) != Some(sha.as_str()) {
        bail!("completed run refers to a different plan");
    }
    let identity = support_identity(
        support,
        lane.support_lane(),
        Some(repo),
        Some(lane.runner_name()),
        CONTINUOUS_ADMISSION,
        "completed run support",
    )?;
    let digest = canonical_identity_sha256(&identity)?;
    verify_plan_identity(&plan, &identity, &digest)?;
    crate::planning::verify_plan_results(&plan, rows, lane.support_lane())
}

fn validate_completed_run(
    lane: Lane,
    repo: &Path,
    run_id: &str,
    plan_path: Option<&Path>,
) -> Result<()> {
    let run = repo.join("results/runs").join(run_id);
    let support = read_json(&run.join("support.json"))?;
    let summary = read_json(&run.join("summary.json"))?;
    match run.join("failures.ndjson").metadata() {
        Ok(metadata) if metadata.len() > 0 => bail!("run has failures"),
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let expected =
        usize::try_from(value_as_i64(support.get("case_count")).context("missing case count")?)?;
    let trials = match lane {
        Lane::Kv => value_as_i64(support.get("expect_trials")),
        Lane::Record => value_as_i64(support.get("expect_trials"))
            .or_else(|| value_as_i64(support.get("trials"))),
    }
    .context("missing trial count")?;
    let rows = read_ndjson(&run.join("results.ndjson"))?;
    crate::summary::verify_result_evidence(
        &rows,
        &summary,
        lane.support_lane(),
        expected,
        usize::try_from(trials)?,
    )?;
    if let Some(plan_path) = plan_path {
        verify_plan_support(lane, repo, &support, plan_path, &rows)?;
    }
    Ok(())
}

fn run_complete(lane: Lane, repo: &Path, run_id: &str, plan_path: Option<&Path>) -> bool {
    validate_completed_run(lane, repo, run_id, plan_path).is_ok()
}

fn command_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(2)
}

fn run_runner(lane: Lane, args: &IdleArgs<'_>, version: Option<i64>) -> Result<i32> {
    let runner = args.repo.join("scripts").join(lane.runner_name());
    let mut command = Command::new(&runner);
    command
        .arg(args.plan)
        .arg(args.run_id)
        .current_dir(args.repo)
        .env("ROOT", args.repo)
        .env("BENCH_BIN", args.bench_bin)
        .env("BENCH_BIN_SHA256", args.bench_bin_sha256);

    match lane {
        Lane::Kv => {
            let version = version.context("KV supervisor requires plan version")?;
            command.env("BENCH_BIN_PREVERIFIED", "1").env(
                "MATRIX_RESUME_SHUFFLE_REMAINING",
                if version >= 2 { "0" } else { "1" },
            );
        }
        Lane::Record => {
            let rocks_bin = args
                .rocks_bench_bin
                .context("--rocks-bench-bin is required for record-concurrency")?;
            let rocks_sha = args
                .rocks_bench_bin_sha256
                .context("--rocks-bench-bin-sha256 is required for record-concurrency")?;
            command
                .env("ROCKS_BENCH_BIN", rocks_bin)
                .env("ROCKS_BENCH_BIN_SHA256", rocks_sha)
                .env("MATRIX_RESUME_SHUFFLE_REMAINING", "1");
        }
    }

    let status = command
        .status()
        .with_context(|| format!("failed to run {}", runner.display()))?;
    Ok(command_code(status))
}

#[derive(Clone, Copy)]
enum PlanMode {
    KvLegacy,
    KvPinned,
    Record,
}

impl PlanMode {
    fn prepare(lane: Lane, args: &IdleArgs<'_>) -> Result<Self> {
        verify_binary(args.bench_bin, args.bench_bin_sha256)?;
        match lane {
            Lane::Kv => match plan_version(args.plan)? {
                1 => Ok(Self::KvLegacy),
                2 => Ok(Self::KvPinned),
                _ => bail!("unsupported KV plan version"),
            },
            Lane::Record => {
                let rocks_bin = args
                    .rocks_bench_bin
                    .context("--rocks-bench-bin is required for record-concurrency")?;
                let rocks_sha = args
                    .rocks_bench_bin_sha256
                    .context("--rocks-bench-bin-sha256 is required for record-concurrency")?;
                verify_binary(rocks_bin, rocks_sha)?;
                Ok(Self::Record)
            }
        }
    }

    const fn version(self) -> Option<i64> {
        match self {
            Self::KvLegacy => Some(1),
            Self::KvPinned => Some(2),
            Self::Record => None,
        }
    }

    const fn completion_plan(self, plan: &Path) -> Option<&Path> {
        match self {
            Self::KvLegacy => None,
            Self::KvPinned | Self::Record => Some(plan),
        }
    }

    fn preflight(self, repo: &Path) -> i32 {
        match self {
            Self::KvPinned => 0,
            Self::KvLegacy | Self::Record => host_preflight(repo, MAX_IO_FULL_AVG10, true),
        }
    }
}

fn report_complete(args: &IdleArgs<'_>, lane: Lane, busy: u64, skipped: bool) -> Result<i32> {
    let _ = write_status(
        args.status_file,
        status_fields("complete", Some(args.run_id), Some(busy)),
    )?;
    let label = if lane == Lane::Record {
        "record concurrency plan"
    } else {
        "concurrency plan"
    };
    if skipped {
        println!("skip complete {label}: {}", args.run_id);
    } else {
        println!("{label} complete: {}", args.run_id);
    }
    Ok(0)
}

pub fn run(args: IdleArgs<'_>) -> Result<i32> {
    let lane = Lane::parse(args.lane)?;
    let busy_sleep = sleep_duration(args.busy_sleep_s)?;
    let lock_path = args
        .lock_file
        .unwrap_or_else(|| Path::new(DEFAULT_LOCK_FILE));
    let Some(_lock) = acquire_lock(lock_path)? else {
        eprintln!(
            "benchmark campaign lock is already held: {}",
            lock_path.display()
        );
        return Ok(73);
    };
    let mode = match PlanMode::prepare(lane, &args) {
        Ok(mode) => mode,
        Err(error) => {
            status_failure(args.status_file, None, None, Some(&error.to_string()))?;
            eprintln!("{error}");
            return Ok(2);
        }
    };
    let completion_plan = mode.completion_plan(args.plan);
    if run_complete(lane, args.repo, args.run_id, completion_plan) {
        return report_complete(&args, lane, 0, true);
    }
    let mut admission = AdmissionLoop::new(true, busy_sleep);
    let code = admission.run(
        || Ok(mode.preflight(args.repo)),
        || run_runner(lane, &args, mode.version()),
        |event, busy| {
            let fields = event.fields(status_fields(event.state(), Some(args.run_id), Some(busy)));
            let _ = write_status(args.status_file, fields)?;
            Ok(())
        },
    )?;
    if code != 0 {
        return Ok(code);
    }
    if let Err(error) = validate_completed_run(lane, args.repo, args.run_id, completion_plan) {
        status_failure(
            args.status_file,
            Some(args.run_id),
            None,
            Some(&format!(
                "runner returned success but final validation failed: {error:#}"
            )),
        )?;
        return Ok(2);
    }
    report_complete(&args, lane, admission.busy_events(), false)
}

#[cfg(test)]
mod tests {
    use super::{
        IdleArgs, Lane, acquire_lock, minimum_free_bytes, parse_io_full_avg10, plan_version,
        run_complete, run_runner, verify_binary,
    };
    use anyhow::{Context, Result};
    use serde_json::json;
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Result<Self> {
            let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "all-db-benchctl-{label}-{}-{counter}",
                std::process::id()
            ));
            if path.exists() {
                fs::remove_dir_all(&path)?;
            }
            fs::create_dir_all(&path)?;
            Ok(Self(path))
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn campaign_lock_contention_is_nonblocking() -> Result<()> {
        let dir = TestDir::new("lock")?;
        let path = dir.0.join("campaign.lock");
        let first = acquire_lock(&path)?.context("first lock acquisition")?;
        assert!(acquire_lock(&path)?.is_none());
        let inherited = first.0.try_clone()?;
        drop(first);
        assert!(acquire_lock(&path)?.is_some());
        drop(inherited);
        Ok(())
    }

    #[test]
    fn parses_io_full_avg10() -> Result<()> {
        assert_eq!(
            parse_io_full_avg10("some avg10=1\nfull avg10=4.25 avg60=1\n")?,
            4.25
        );
        assert!(parse_io_full_avg10("some avg10=1\n").is_err());
        Ok(())
    }

    #[test]
    fn minimum_free_space_is_gib_and_rejects_invalid_values() -> Result<()> {
        assert_eq!(minimum_free_bytes("8")?, 8 * 1024 * 1024 * 1024);
        assert_eq!(minimum_free_bytes("0")?, 0);
        assert!(minimum_free_bytes("-1").is_err());
        assert!(minimum_free_bytes("nan").is_err());
        Ok(())
    }

    #[test]
    fn verifies_executable_binary_sha256() -> Result<()> {
        let dir = TestDir::new("binary")?;
        let path = dir.0.join("bin");
        fs::write(&path, b"abc")?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
        let sha = crate::file_hash::sha256(&path)?;
        verify_binary(&path, &sha)?;
        let error = verify_binary(&path, "bad")
            .err()
            .context("mismatched SHA must fail")?;
        assert!(error.to_string().contains("mismatch"));
        Ok(())
    }

    #[test]
    fn runner_environment_matches_lane_contracts() -> Result<()> {
        let dir = TestDir::new("runner-env")?;
        let scripts = dir.0.join("scripts");
        fs::create_dir_all(&scripts)?;
        let kv_runner = scripts.join("run-kv-concurrency-plan.sh");
        fs::write(
            &kv_runner,
            r#"#!/bin/sh
printf '%s\n' "$BENCH_BIN_PREVERIFIED|$MATRIX_RESUME_SHUFFLE_REMAINING|$BENCH_BIN_SHA256" > "$ROOT/kv.env"
"#,
        )?;
        fs::set_permissions(&kv_runner, fs::Permissions::from_mode(0o755))?;
        let record_runner = scripts.join("run-record-concurrency-plan.sh");
        fs::write(
            &record_runner,
            r#"#!/bin/sh
printf '%s\n' "$ROCKS_BENCH_BIN|$ROCKS_BENCH_BIN_SHA256|$MATRIX_RESUME_SHUFFLE_REMAINING" > "$ROOT/record.env"
"#,
        )?;
        fs::set_permissions(&record_runner, fs::Permissions::from_mode(0o755))?;
        let plan = dir.0.join("plan.json");
        fs::write(&plan, "{}")?;
        let bench = dir.0.join("bench");
        let rocks = dir.0.join("rocks");
        let status = dir.0.join("status.json");
        let kv_args = IdleArgs {
            lane: "kv-concurrency",
            repo: &dir.0,
            plan: &plan,
            run_id: "r",
            bench_bin: &bench,
            bench_bin_sha256: "bench-sha",
            rocks_bench_bin: None,
            rocks_bench_bin_sha256: None,
            lock_file: None,
            status_file: &status,
            busy_sleep_s: 1.0,
        };
        assert_eq!(run_runner(Lane::Kv, &kv_args, Some(2))?, 0);
        assert_eq!(
            fs::read_to_string(dir.0.join("kv.env"))?,
            "1|0|bench-sha
"
        );

        let record_args = IdleArgs {
            lane: "record-concurrency",
            repo: &dir.0,
            plan: &plan,
            run_id: "r",
            bench_bin: &bench,
            bench_bin_sha256: "bench-sha",
            rocks_bench_bin: Some(&rocks),
            rocks_bench_bin_sha256: Some("rocks-sha"),
            lock_file: None,
            status_file: &status,
            busy_sleep_s: 1.0,
        };
        assert_eq!(run_runner(Lane::Record, &record_args, None)?, 0);
        let record_env = fs::read_to_string(dir.0.join("record.env"))?;
        assert!(record_env.ends_with(
            "|rocks-sha|1
"
        ));
        assert!(record_env.starts_with(&rocks.display().to_string()));
        Ok(())
    }

    #[test]
    fn validates_kv_plan_versions() -> Result<()> {
        let dir = TestDir::new("plan")?;
        let path = dir.0.join("plan.json");
        fs::write(&path, r#"{"concurrency_probe_plan_version":2}"#)?;
        assert_eq!(plan_version(&path)?, 2);
        fs::write(&path, r#"{"concurrency_probe_plan_version":3}"#)?;
        assert!(plan_version(&path).is_err());
        Ok(())
    }

    fn write_complete_run(repo: &Path, lane: Lane, trials: u64, groups: u64) -> Result<()> {
        let run = repo.join("results/runs/r");
        fs::create_dir_all(&run)?;
        fs::write(
            run.join("support.json"),
            serde_json::to_vec(&json!({"case_count": trials * groups, "expect_trials": trials}))?,
        )?;
        let mut rows = String::new();
        for clients in 1..=groups {
            for trial in 1..=trials {
                rows.push_str(&serde_json::to_string(&json!({
                    "lane": lane.support_lane(), "scenario": "test", "engine": "test",
                    "engine_version": "1", "durability": "sync", "durability_mapping": "sync",
                    "workload": "point-read", "records": 100, "clients": clients,
                    "ops_requested": 100, "ops_completed": 100, "txn_size": 1,
                    "trial": trial, "ops_per_s": 100.0, "elapsed_s": 1.0, "db_bytes": 1024,
                }))?);
                rows.push('\n');
            }
        }
        let input = run.join("results.ndjson");
        fs::write(&input, rows)?;
        crate::summary::summarize_file(
            &input,
            Some(&run.join("summary.json")),
            Some(&run.join("summary.md")),
            Some(usize::try_from(trials)?),
        )
    }

    #[test]
    fn complete_run_requires_exact_group_count_and_no_failures() -> Result<()> {
        let dir = TestDir::new("complete")?;
        write_complete_run(&dir.0, Lane::Kv, 3, 4)?;
        assert!(run_complete(Lane::Kv, &dir.0, "r", None));
        fs::write(dir.0.join("results/runs/r/failures.ndjson"), "{}\n")?;
        assert!(!run_complete(Lane::Kv, &dir.0, "r", None));
        fs::remove_file(dir.0.join("results/runs/r/failures.ndjson"))?;
        fs::write(
            dir.0.join("results/runs/r/summary.json"),
            serde_json::to_vec(&json!({"row_count": 12, "group_count": 12, "problems": []}))?,
        )?;
        assert!(!run_complete(Lane::Kv, &dir.0, "r", None));
        Ok(())
    }

    #[test]
    fn complete_run_requires_actual_result_evidence() -> Result<()> {
        let dir = TestDir::new("result-evidence")?;
        write_complete_run(&dir.0, Lane::Kv, 2, 1)?;
        let input = dir.0.join("results/runs/r/results.ndjson");
        let original = fs::read_to_string(&input)?;
        let first = original.lines().next().context("first row")?;
        fs::write(&input, format!("{first}\n{first}\n"))?;
        assert!(!run_complete(Lane::Kv, &dir.0, "r", None));
        fs::write(&input, &original)?;
        assert!(run_complete(Lane::Kv, &dir.0, "r", None));
        fs::remove_file(&input)?;
        assert!(!run_complete(Lane::Kv, &dir.0, "r", None));
        Ok(())
    }

    #[test]
    fn kv_completion_does_not_use_legacy_trials_fallback() -> Result<()> {
        let dir = TestDir::new("kv-no-trials-fallback")?;
        let run = dir.0.join("results/runs/r");
        fs::create_dir_all(&run)?;
        fs::write(
            run.join("support.json"),
            serde_json::to_vec(&json!({"case_count": 3, "trials": 1}))?,
        )?;
        fs::write(
            run.join("summary.json"),
            serde_json::to_vec(&json!({"row_count": 3, "group_count": 3}))?,
        )?;
        assert!(!run_complete(Lane::Kv, &dir.0, "r", None));
        Ok(())
    }

    #[test]
    fn record_completion_uses_trials_fallback() -> Result<()> {
        let dir = TestDir::new("trials-fallback")?;
        let run = dir.0.join("results/runs/r");
        write_complete_run(&dir.0, Lane::Record, 1, 3)?;
        fs::write(
            run.join("support.json"),
            serde_json::to_vec(&json!({"case_count": 3, "trials": 1}))?,
        )?;
        assert!(run_complete(Lane::Record, &dir.0, "r", None));
        Ok(())
    }
}
