use crate::file_hash::sha256;
use crate::json_value::py_string;
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
};

pub(crate) const QUALITY_POLICY_VERSION: i64 = 1;

#[derive(Clone)]
pub(crate) struct ScrubConfig {
    pub(crate) max_elapsed_s: f64,
    pub(crate) max_runqueue_fraction: f64,
    pub(crate) max_cpu_psi_fraction: f64,
    pub(crate) max_benchmark_cpu_share: f64,
    pub(crate) dry_run: bool,
    pub(crate) cpu_count: Option<i64>,
    pub(crate) case_ids: BTreeSet<String>,
    pub(crate) intervals: Vec<String>,
}

impl Default for ScrubConfig {
    fn default() -> Self {
        Self {
            max_elapsed_s: 0.5,
            max_runqueue_fraction: 0.03,
            max_cpu_psi_fraction: 0.05,
            max_benchmark_cpu_share: 0.5,
            dry_run: false,
            cpu_count: None,
            case_ids: BTreeSet::new(),
            intervals: vec!["measured".to_owned()],
        }
    }
}

pub(crate) struct AuditStockArgs<'a> {
    pub(crate) run_dir: &'a Path,
    pub(crate) sizing_audit: &'a Path,
    pub(crate) lane: &'a str,
    pub(crate) json_out: &'a Path,
    pub(crate) markdown_out: Option<&'a Path>,
}

fn value_f64(value: Option<&Value>, default: f64) -> Result<f64> {
    let Some(value) = value else {
        return Ok(default);
    };
    match value {
        Value::Number(value) => value.as_f64().context("number is outside f64 range"),
        Value::String(raw) => raw
            .parse::<f64>()
            .with_context(|| format!("invalid number {raw:?}")),
        Value::Null => Ok(default),
        other => bail!("expected numeric-compatible value, got {other}"),
    }
}

fn value_i64(value: Option<&Value>, default: i64) -> Result<i64> {
    let Some(value) = value else {
        return Ok(default);
    };
    match value {
        Value::Number(value) => value.as_i64().context("integer is outside i64 range"),
        Value::String(raw) => raw
            .parse::<i64>()
            .with_context(|| format!("invalid integer {raw:?}")),
        Value::Null => Ok(default),
        other => bail!("expected integer-compatible value, got {other}"),
    }
}

fn python_compact(value: &Value) -> Result<String> {
    Ok(match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            serde_json::to_string(value)?
        }
        Value::Array(values) => {
            let body = values
                .iter()
                .map(python_compact)
                .collect::<Result<Vec<_>>>()?
                .join(", ");
            format!("[{body}]")
        }
        Value::Object(object) => {
            let body = object
                .iter()
                .map(|(key, value)| {
                    Ok(format!(
                        "{}: {}",
                        serde_json::to_string(key)?,
                        python_compact(value)?
                    ))
                })
                .collect::<Result<Vec<_>>>()?
                .join(", ");
            format!("{{{body}}}")
        }
    })
}

fn cpu_list_count(spec: &str) -> Result<i64> {
    let mut count = 0_i64;
    for part in spec.split(',').filter(|part| !part.is_empty()) {
        if let Some((start, end)) = part.split_once('-') {
            let start = start
                .parse::<i64>()
                .with_context(|| format!("invalid CPU range {part:?}"))?;
            let end = end
                .parse::<i64>()
                .with_context(|| format!("invalid CPU range {part:?}"))?;
            if end < start {
                bail!("invalid CPU range {part:?}");
            }
            count += end - start + 1;
        } else {
            part.parse::<i64>()
                .with_context(|| format!("invalid CPU id {part:?}"))?;
            count += 1;
        }
    }
    Ok(count.max(1))
}

pub(crate) fn available_cpus() -> i64 {
    if let Ok(status) = fs::read_to_string("/proc/self/status")
        && let Some(spec) = status
            .lines()
            .find_map(|line| line.strip_prefix("Cpus_allowed_list:").map(str::trim))
        && let Ok(count) = cpu_list_count(spec)
    {
        return count;
    }
    std::thread::available_parallelism()
        .map_or(1, |count| i64::try_from(count.get()).unwrap_or(i64::MAX))
        .max(1)
}

fn interval_parts<'a>(row: &'a Value, interval: &str) -> Result<(f64, &'a Value, &'a Value)> {
    let (elapsed_field, process_field, system_field) = match interval {
        "measured" => ("elapsed_s", "measured_process", "measured_system_delta"),
        "open" => ("open_s", "open_process", "open_system_delta"),
        other => bail!("unknown pressure interval: {other}"),
    };
    static EMPTY: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    let empty = EMPTY.get_or_init(|| json!({}));
    let elapsed = value_f64(row.get(elapsed_field), 0.0)?;
    let process = row
        .get(process_field)
        .filter(|value| !value.is_null())
        .unwrap_or(empty);
    let system = row
        .get(system_field)
        .filter(|value| !value.is_null())
        .unwrap_or(empty);
    Ok((elapsed, process, system))
}

pub(crate) fn pressure_evidence(
    row: &Value,
    interval: &str,
    config: &ScrubConfig,
    cpu_count: i64,
) -> Result<Option<Value>> {
    let (elapsed, process, system) = interval_parts(row, interval)?;
    if elapsed <= 0.0 || elapsed > config.max_elapsed_s {
        return Ok(None);
    }
    let cpu_fraction = value_f64(process.get("cpu_runtime_fraction_of_wall"), 0.0)?;
    let max_benchmark_cpu_fraction = (cpu_count as f64 * config.max_benchmark_cpu_share).max(1.0);
    if cpu_fraction > max_benchmark_cpu_fraction {
        return Ok(None);
    }
    let runqueue_fraction = value_f64(process.get("runqueue_wait_fraction_of_wall"), 0.0)?;
    let mut accounting_wall_ns = value_f64(system.get("accounting_wall_ns"), 0.0)?;
    if accounting_wall_ns <= 0.0 {
        accounting_wall_ns = elapsed * 1_000_000_000.0;
    }
    let cpu_psi_us = value_f64(system.get("psi_cpu_some_us"), 0.0)?;
    let cpu_psi_fraction = cpu_psi_us / (accounting_wall_ns / 1_000.0).max(1.0);
    let mut reasons = Vec::new();
    if runqueue_fraction > config.max_runqueue_fraction {
        reasons.push("runqueue");
    }
    if cpu_psi_fraction > config.max_cpu_psi_fraction {
        reasons.push("cpu-psi");
    }
    if reasons.is_empty() {
        return Ok(None);
    }
    Ok(Some(json!({
        "interval": interval,
        "reasons": reasons,
        "elapsed_s": elapsed,
        "runqueue_wait_fraction_of_wall": runqueue_fraction,
        "cpu_psi_some_fraction_of_wall": cpu_psi_fraction,
        "benchmark_cpu_runtime_fraction_of_wall": cpu_fraction,
        "max_benchmark_cpu_fraction": max_benchmark_cpu_fraction,
        "write_bytes": value_i64(process.get("write_bytes"), 0)?,
    })))
}

fn sorted_case_paths(run_dir: &Path) -> Result<Vec<PathBuf>> {
    let cases_dir = run_dir.join("cases");
    if !cases_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut paths = fs::read_dir(&cases_dir)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

fn next_attempt_dir(root: &Path, case_id: &str) -> Result<PathBuf> {
    let case_root = root.join(case_id);
    fs::create_dir_all(&case_root)?;
    let mut maximum = 0_i64;
    for entry in fs::read_dir(&case_root)? {
        let path = entry?.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if let Some(raw) = name.strip_prefix("attempt-") {
            maximum = maximum.max(
                raw.parse::<i64>()
                    .with_context(|| format!("invalid attempt directory {name:?}"))?,
            );
        }
    }
    let target = case_root.join(format!("attempt-{:03}", maximum + 1));
    fs::create_dir(&target)?;
    Ok(target)
}

fn move_if_exists(source: &Path, target: &Path) -> Result<()> {
    if source.exists() {
        fs::rename(source, target).with_context(|| {
            format!(
                "failed to move {} to {}",
                source.display(),
                target.display()
            )
        })?;
    }
    Ok(())
}

fn archive_case(run_dir: &Path, case_path: &Path, evidence: &Value) -> Result<PathBuf> {
    let case_id = case_path
        .file_stem()
        .and_then(|value| value.to_str())
        .context("case filename has no stem")?;
    let target = next_attempt_dir(&run_dir.join("rejected-pressure"), case_id)?;
    fs::rename(case_path, target.join("case.json"))?;
    for (source, name) in [
        (
            run_dir.join("noise").join(format!("{case_id}.before.json")),
            "noise-before.json",
        ),
        (
            run_dir.join("noise").join(format!("{case_id}.ready.json")),
            "noise-ready.json",
        ),
        (
            run_dir.join("noise").join(format!("{case_id}.after.json")),
            "noise-after.json",
        ),
        (
            run_dir
                .join("stderr")
                .join(format!("{case_id}.prepare.log")),
            "prepare-stderr.log",
        ),
        (
            run_dir.join("stderr").join(format!("{case_id}.log")),
            "stderr.log",
        ),
    ] {
        move_if_exists(&source, &target.join(name))?;
    }
    fs::write(
        target.join("rejection.json"),
        serde_json::to_string_pretty(evidence)? + "\n",
    )?;
    Ok(target)
}

pub(crate) fn scrub(run_dir: &Path, config: &ScrubConfig) -> Result<Value> {
    let cpu_count = config.cpu_count.unwrap_or_else(available_cpus).max(1);
    let intervals = if config.intervals.is_empty() {
        vec!["measured".to_owned()]
    } else {
        config.intervals.clone()
    };
    let mut rejected = Vec::new();
    let mut examined = 0_i64;
    for case_path in sorted_case_paths(run_dir)? {
        let case_id = case_path
            .file_stem()
            .and_then(|value| value.to_str())
            .context("case filename has no stem")?
            .to_owned();
        if !config.case_ids.is_empty() && !config.case_ids.contains(&case_id) {
            continue;
        }
        let row: Value = serde_json::from_str(&fs::read_to_string(&case_path)?)?;
        examined += 1;
        let mut evidence = None;
        for interval in &intervals {
            evidence = pressure_evidence(&row, interval, config, cpu_count)?;
            if evidence.is_some() {
                break;
            }
        }
        let Some(base) = evidence else {
            continue;
        };
        let mut object = Map::new();
        object.insert("case_id".to_owned(), json!(case_id));
        for key in ["trial", "engine", "durability", "workload"] {
            object.insert(key.to_owned(), row.get(key).cloned().unwrap_or(Value::Null));
        }
        object.insert(
            "read_materialization".to_owned(),
            row.get("read_materialization")
                .cloned()
                .unwrap_or_else(|| json!("legacy-read-v0")),
        );
        object.insert(
            "write_materialization".to_owned(),
            row.get("write_materialization")
                .cloned()
                .unwrap_or_else(|| json!("legacy-return-v0")),
        );
        object.insert(
            "thresholds".to_owned(),
            json!({
                "max_elapsed_s": config.max_elapsed_s,
                "max_runqueue_fraction": config.max_runqueue_fraction,
                "max_cpu_psi_fraction": config.max_cpu_psi_fraction,
                "max_benchmark_cpu_share": config.max_benchmark_cpu_share,
                "available_cpus": cpu_count,
            }),
        );
        object.extend(
            base.as_object()
                .cloned()
                .context("pressure evidence is not an object")?,
        );
        let mut enriched = Value::Object(object);
        if !config.dry_run {
            let target = archive_case(run_dir, &case_path, &enriched)?;
            enriched
                .as_object_mut()
                .context("pressure evidence is not an object")?
                .insert("archive".to_owned(), json!(target.to_string_lossy()));
        }
        rejected.push(enriched);
    }
    if !rejected.is_empty() && !config.dry_run {
        for name in [
            "results.ndjson",
            "summary.json",
            "summary.md",
            "host-end.txt",
        ] {
            let path = run_dir.join(name);
            if path.exists() {
                fs::remove_file(path)?;
            }
        }
    }
    Ok(json!({
        "run_dir": run_dir.to_string_lossy(),
        "examined": examined,
        "rejected": rejected.len(),
        "dry_run": config.dry_run,
        "available_cpus": cpu_count,
        "intervals": intervals,
        "thresholds": {
            "max_elapsed_s": config.max_elapsed_s,
            "max_runqueue_fraction": config.max_runqueue_fraction,
            "max_cpu_psi_fraction": config.max_cpu_psi_fraction,
            "max_benchmark_cpu_share": config.max_benchmark_cpu_share,
        },
        "cases": rejected,
    }))
}

pub(crate) fn run_scrub(run_dir: &Path, config: ScrubConfig) -> Result<()> {
    println!("{}", python_compact(&scrub(run_dir, &config)?)?);
    Ok(())
}

fn identity(item: &Value, lane: &str) -> Result<Vec<String>> {
    let mut key = vec![
        py_string(item.get("engine").context("engine missing")?),
        py_string(item.get("durability").context("durability missing")?),
        py_string(item.get("workload").context("workload missing")?),
    ];
    if lane == "record" {
        key.push(
            item.get("read_materialization")
                .map_or_else(|| "legacy-read-v0".to_owned(), py_string),
        );
        key.push(
            item.get("write_materialization")
                .map_or_else(|| "legacy-return-v0".to_owned(), py_string),
        );
    }
    Ok(key)
}

fn runner_ops_override(lane: &str, workload: &str, effective_ops: i64) -> Result<i64> {
    if workload != "tiny-txn" {
        return Ok(effective_ops);
    }
    match lane {
        "kv" => Ok(effective_ops * 10),
        "record" => Ok(effective_ops * 2),
        other => bail!("unsupported lane for tiny-txn repair: {other:?}"),
    }
}

pub(crate) fn build_plan(
    run_dir: &Path,
    sizing_audit: &Value,
    scrub_report: &Value,
    lane: &str,
) -> Result<Value> {
    let sizing_version = value_i64(sizing_audit.get("sizing_policy_version"), -1)?;
    if !matches!(sizing_version, 2 | 3) {
        bail!(
            "unsupported sizing policy: {:?}",
            sizing_audit
                .get("sizing_policy_version")
                .unwrap_or(&Value::Null)
        );
    }
    let groups = sizing_audit
        .get("groups")
        .and_then(Value::as_array)
        .context("sizing groups missing")?;
    let mut by_identity = HashMap::<Vec<String>, Value>::new();
    for group in groups {
        let key = identity(group, lane)?;
        if by_identity.insert(key, group.clone()).is_some() {
            bail!("duplicate sizing audit identity");
        }
    }
    let mut pressure = BTreeMap::<Vec<String>, Vec<Value>>::new();
    for case in scrub_report
        .get("cases")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        pressure
            .entry(identity(&case, lane)?)
            .or_default()
            .push(case);
    }
    let expect_trials = value_i64(
        sizing_audit
            .get("thresholds")
            .and_then(|value| value.get("expect_trials")),
        0,
    )?;
    let mut repairs = Vec::new();
    let mut ignored = Vec::new();
    for (key, cases) in pressure {
        let group = by_identity
            .get(&key)
            .with_context(|| format!("pressure case missing from sizing audit: {key:?}"))?;
        let mut pressure_trials = cases
            .iter()
            .map(|case| value_i64(case.get("trial"), 0))
            .collect::<Result<Vec<_>>>()?;
        pressure_trials.sort_unstable();
        if group.get("status").and_then(Value::as_str) == Some("undersized") {
            let mut item = Map::new();
            item.insert("engine".to_owned(), json!(key[0]));
            item.insert("durability".to_owned(), json!(key[1]));
            item.insert("workload".to_owned(), json!(key[2]));
            if lane == "record" {
                item.insert("read_materialization".to_owned(), json!(key[3]));
                item.insert("write_materialization".to_owned(), json!(key[4]));
            }
            item.insert("pressure_trials".to_owned(), json!(pressure_trials));
            ignored.push(Value::Object(item));
            continue;
        }
        let effective_ops = value_i64(group.get("ops_requested"), 0)?;
        let workload = group
            .get("workload")
            .map(py_string)
            .unwrap_or_else(|| "None".to_owned());
        let mut item = group
            .as_object()
            .cloned()
            .context("sizing group is not an object")?;
        item.insert("quality_repair_required".to_owned(), json!(true));
        item.insert("resize_strategy".to_owned(), json!("quality-repair"));
        item.insert("suggested_effective_ops".to_owned(), json!(effective_ops));
        item.insert(
            "runner_ops_override".to_owned(),
            json!(runner_ops_override(lane, &workload, effective_ops)?),
        );
        item.insert("suggested_trials".to_owned(), json!(expect_trials));
        item.insert("pressure_trials".to_owned(), json!(pressure_trials));
        item.insert("pressure_evidence".to_owned(), Value::Array(cases));
        repairs.push(Value::Object(item));
    }
    let results_path = run_dir.join("results.ndjson");
    Ok(json!({
        "quality_policy_version": QUALITY_POLICY_VERSION,
        "lane": lane,
        "source_run_dir": run_dir.to_string_lossy(),
        "source_results_sha256": sha256(&results_path)?,
        "source_sizing_policy_version": sizing_audit.get("sizing_policy_version").cloned().unwrap_or(Value::Null),
        "pressure_thresholds": scrub_report.get("thresholds").cloned().unwrap_or_else(|| json!({})),
        "available_cpus": scrub_report.get("available_cpus").cloned().unwrap_or(Value::Null),
        "pressure_cases_total": value_i64(scrub_report.get("rejected"), 0)?,
        "repair_group_count": repairs.len(),
        "ignored_undersized_group_count": ignored.len(),
        "ignored_undersized_groups": ignored,
        "groups": repairs,
    }))
}

fn comma_int(value: i64) -> String {
    let digits = value.unsigned_abs().to_string();
    let mut out = String::new();
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    if value < 0 { format!("-{out}") } else { out }
}

pub(crate) fn render_markdown(plan: &Value) -> Result<String> {
    let groups = plan
        .get("groups")
        .and_then(Value::as_array)
        .context("quality plan groups missing")?;
    let mut lines = vec![
        "# Stock short-trial pressure repair plan".to_owned(),
        String::new(),
        format!(
            "Quality policy v{}; lane `{}`.",
            value_i64(plan.get("quality_policy_version"), 0)?,
            py_string(plan.get("lane").unwrap_or(&Value::Null))
        ),
        format!(
            "Pressure-hit stock cases: **{}**; retained groups requiring clean replacement: **{}**; pressure-hit groups already replaced by sizing: **{}**.",
            value_i64(plan.get("pressure_cases_total"), 0)?,
            value_i64(plan.get("repair_group_count"), 0)?,
            value_i64(plan.get("ignored_undersized_group_count"), 0)?
        ),
        String::new(),
        "| engine | durability | workload | stock status | ops | trials | pressure stock trials |"
            .to_owned(),
        "| --- | --- | --- | --- | ---: | ---: | --- |".to_owned(),
    ];
    for group in groups {
        let trials = group
            .get("pressure_trials")
            .and_then(Value::as_array)
            .context("pressure trials missing")?
            .iter()
            .map(py_string)
            .collect::<Vec<_>>()
            .join(",");
        lines.push(format!(
            "| {} | {} | {} | {} | {} | {} | {} |",
            py_string(group.get("engine").unwrap_or(&Value::Null)),
            py_string(group.get("durability").unwrap_or(&Value::Null)),
            py_string(group.get("workload").unwrap_or(&Value::Null)),
            py_string(group.get("status").unwrap_or(&Value::Null)),
            comma_int(value_i64(group.get("ops_requested"), 0)?),
            value_i64(group.get("suggested_trials"), 0)?,
            trials
        ));
    }
    Ok(lines.join("\n") + "\n")
}

pub(crate) fn run_audit_stock(args: AuditStockArgs<'_>) -> Result<()> {
    if !matches!(args.lane, "kv" | "record") {
        bail!("unsupported lane: {:?}", args.lane);
    }
    let report = scrub(
        args.run_dir,
        &ScrubConfig {
            dry_run: true,
            ..ScrubConfig::default()
        },
    )?;
    let audit: Value = serde_json::from_str(&fs::read_to_string(args.sizing_audit)?)?;
    let plan = build_plan(args.run_dir, &audit, &report, args.lane)?;
    if let Some(parent) = args.json_out.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(args.json_out, serde_json::to_string_pretty(&plan)? + "\n")?;
    let markdown = render_markdown(&plan)?;
    if let Some(path) = args.markdown_out {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, markdown)?;
    } else {
        print!("{markdown}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ScrubConfig, build_plan, pressure_evidence, runner_ops_override, scrub};
    use anyhow::Result;
    use serde_json::{Value, json};
    use std::fs;

    fn row(trial: i64, elapsed: f64, rq: f64, psi: f64, cpu: f64) -> Value {
        let wall_ns = (elapsed * 1_000_000_000.0) as i64;
        json!({
            "trial":trial,"engine":"rocksdb","durability":"relaxed","workload":"write-burst",
            "elapsed_s":elapsed,"ops_per_s":1_000_000.0,
            "measured_process":{"cpu_runtime_fraction_of_wall":cpu,"runqueue_wait_fraction_of_wall":rq,"write_bytes":1234},
            "measured_system_delta":{"accounting_wall_ns":wall_ns,"psi_cpu_some_us":(psi*wall_ns as f64/1000.0) as i64}
        })
    }

    #[test]
    fn flags_short_pressure_without_self_pressure_false_positive() -> Result<()> {
        let config = ScrubConfig::default();
        assert!(
            pressure_evidence(&row(1, 0.03, 0.031, 0.0, 1.0), "measured", &config, 8)?.is_some()
        );
        assert!(
            pressure_evidence(&row(2, 0.03, 0.0, 0.051, 1.0), "measured", &config, 8)?.is_some()
        );
        assert!(pressure_evidence(&row(3, 0.6, 0.5, 0.5, 1.0), "measured", &config, 8)?.is_none());
        assert!(pressure_evidence(&row(4, 0.03, 0.5, 0.5, 4.1), "measured", &config, 8)?.is_none());
        Ok(())
    }

    #[test]
    fn scrub_archives_and_invalidates_outputs() -> Result<()> {
        let root = std::env::temp_dir().join(format!("short-pressure-{}", std::process::id()));
        if root.exists() {
            fs::remove_dir_all(&root)?;
        }
        fs::create_dir_all(root.join("cases"))?;
        fs::create_dir_all(root.join("noise"))?;
        fs::create_dir_all(root.join("stderr"))?;
        let good = root.join("cases/t1.json");
        let bad = root.join("cases/t2.json");
        fs::write(&good, serde_json::to_string(&row(1, 0.03, 0.0, 0.0, 1.0))?)?;
        fs::write(&bad, serde_json::to_string(&row(2, 0.03, 0.0, 0.2, 1.0))?)?;
        fs::write(root.join("noise/t2.before.json"), "{}")?;
        for name in [
            "results.ndjson",
            "summary.json",
            "summary.md",
            "host-end.txt",
        ] {
            fs::write(root.join(name), "stale")?;
        }
        let report = scrub(
            &root,
            &ScrubConfig {
                cpu_count: Some(8),
                ..ScrubConfig::default()
            },
        )?;
        assert_eq!(report["rejected"], 1);
        assert!(good.exists());
        assert!(!bad.exists());
        assert!(
            root.join("rejected-pressure/t2/attempt-001/rejection.json")
                .is_file()
        );
        assert!(!root.join("summary.json").exists());
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn quality_plan_ignores_undersized_and_repairs_retained() -> Result<()> {
        let root = std::env::temp_dir().join(format!("short-plan-{}", std::process::id()));
        if root.exists() {
            fs::remove_dir_all(&root)?;
        }
        fs::create_dir_all(&root)?;
        fs::write(root.join("results.ndjson"), "{}\n")?;
        let group = |engine: &str, status: &str| {
            json!({
                "engine":engine,"engine_version":"1","durability":"relaxed","workload":"write-burst",
                "records":100000,"ops_requested":50000,"status":status,"suggested_effective_ops":null,
                "runner_ops_override":null,"suggested_trials":null,"resize_strategy":null
            })
        };
        let audit = json!({"sizing_policy_version":2,"thresholds":{"expect_trials":3},"groups":[group("keep","variable"),group("replace","undersized")]});
        let report = json!({"thresholds":{"max_elapsed_s":0.5},"available_cpus":8,"rejected":2,"cases":[
            {"engine":"keep","durability":"relaxed","workload":"write-burst","trial":2},
            {"engine":"replace","durability":"relaxed","workload":"write-burst","trial":1}
        ]});
        let plan = build_plan(&root, &audit, &report, "kv")?;
        assert_eq!(plan["repair_group_count"], 1);
        assert_eq!(plan["ignored_undersized_group_count"], 1);
        assert_eq!(plan["groups"][0]["runner_ops_override"], 50000);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn tiny_txn_override_is_lane_specific() -> Result<()> {
        assert_eq!(runner_ops_override("kv", "tiny-txn", 5000)?, 50000);
        assert_eq!(runner_ops_override("record", "tiny-txn", 5000)?, 10000);
        Ok(())
    }
}
