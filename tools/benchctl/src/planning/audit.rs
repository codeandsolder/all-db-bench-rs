use super::{
    get_f64, get_i64, get_str, median, number, object, read_json, sha256, support_identity,
    value_i64, write_json,
};
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

const SAFE_OP_SCALE_WORKLOADS: [&str; 3] = ["point-read", "range-scan", "read-heavy"];
const STATE_CHANGING_WORKLOADS: [&str; 5] = [
    "balanced",
    "tiny-txn",
    "write-burst",
    "churn",
    "delete-burst",
];
const TARGET_SECONDS: f64 = 3.0;
const FLOOR_SECONDS: f64 = 2.0;
const SEVERE_SECONDS: f64 = 0.25;
const STATEFUL_MAX_TRIALS: i64 = 15;
const POLICY_VERSION: i64 = 1;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct FamilyKey {
    scenario: String,
    engine: String,
    durability: String,
    workload: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ClientKey {
    family: FamilyKey,
    clients: i64,
}

fn family_key(value: &Value) -> Result<FamilyKey> {
    Ok(FamilyKey {
        scenario: get_str(value, "scenario")?.to_owned(),
        engine: get_str(value, "engine")?.to_owned(),
        durability: get_str(value, "durability")?.to_owned(),
        workload: get_str(value, "workload")?.to_owned(),
    })
}

fn client_key(value: &Value) -> Result<ClientKey> {
    Ok(ClientKey {
        family: family_key(value)?,
        clients: get_i64(value, "clients")?,
    })
}

fn stock_ops(
    profile: &str,
    workload: &str,
    default_ops: i64,
    records: i64,
    clients: i64,
) -> Result<i64> {
    let mut ops = if workload == "tiny-txn" {
        match profile {
            "smoke" => 300,
            "quick" => 1_000,
            "full" => 2_500,
            _ => bail!("unsupported profile for tiny-txn: {profile}"),
        }
    } else if workload == "point-read" && profile == "quick" {
        200_000
    } else if workload == "range-scan" {
        if profile == "quick" {
            1_000
        } else {
            (default_ops / 100).max(1)
        }
    } else {
        default_ops
    };
    if workload == "delete-burst" {
        ops = ops.min(records);
    }
    if matches!(workload, "range-scan" | "tiny-txn") {
        ops = ops.max(clients);
    }
    Ok(ops)
}

fn parse_jobs(run_dir: &Path, support: &Value) -> Result<Vec<Value>> {
    let path = run_dir.join("jobs.txt");
    let text = fs::read_to_string(&path)
        .with_context(|| format!("missing jobs.txt: {}", path.display()))?;
    let mut jobs = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('|').collect();
        let job = match fields.as_slice() {
            [
                scenario,
                engine,
                durability,
                workload,
                clients_raw,
                trial_raw,
            ] => {
                let profile = get_str(support, "profile")?;
                let default_ops = get_i64(support, "default_ops")?;
                let records = get_i64(support, "records")?;
                let clients = clients_raw
                    .parse::<i64>()
                    .with_context(|| format!("invalid jobs.txt clients on line {}", index + 1))?;
                let trial = trial_raw
                    .parse::<i64>()
                    .with_context(|| format!("invalid jobs.txt trial on line {}", index + 1))?;
                json!({
                    "scenario": format!("concurrency-{scenario}"),
                    "engine": engine,
                    "durability": durability,
                    "workload": workload,
                    "clients": clients,
                    "trial": trial,
                    "records": records,
                    "ops": stock_ops(profile, workload, default_ops, records, clients)?,
                })
            }
            [
                scenario,
                engine,
                durability,
                workload,
                clients,
                records,
                ops,
                trial,
                state,
                write,
                slots,
            ] => {
                json!({
                    "scenario": scenario,
                    "engine": engine,
                    "durability": durability,
                    "workload": workload,
                    "clients": clients.parse::<i64>().with_context(|| format!("invalid jobs.txt clients on line {}", index + 1))?,
                    "records": records.parse::<i64>().with_context(|| format!("invalid jobs.txt records on line {}", index + 1))?,
                    "ops": ops.parse::<i64>().with_context(|| format!("invalid jobs.txt ops on line {}", index + 1))?,
                    "trial": trial.parse::<i64>().with_context(|| format!("invalid jobs.txt trial on line {}", index + 1))?,
                    "state_evolution": state,
                    "write_pattern": write,
                    "bounded_churn_slots": slots.parse::<i64>().with_context(|| format!("invalid jobs.txt slots on line {}", index + 1))?,
                })
            }
            _ => bail!("invalid jobs.txt line {}: {line:?}", index + 1),
        };
        jobs.push(job);
    }
    Ok(jobs)
}

fn read_cases(run_dir: &Path) -> Result<Vec<Value>> {
    let mut rows = Vec::new();
    for path in super::case_paths(run_dir)? {
        let mut row = read_json(&path)?;
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .context("non-UTF-8 case filename")?;
        row.as_object_mut()
            .context("case must be an object")?
            .insert("_case_file".to_owned(), json!(name));
        rows.push(row);
    }
    Ok(rows)
}

pub(crate) struct AuditArgs<'a> {
    pub(crate) repo: &'a Path,
    pub(crate) run_dir: &'a Path,
    pub(crate) additional_runs: &'a [PathBuf],
    pub(crate) json_out: Option<&'a Path>,
    pub(crate) markdown_out: Option<&'a Path>,
    pub(crate) probe_plan_out: Option<&'a Path>,
    pub(crate) expected_admission_policy: Option<&'a str>,
}

fn audit(
    repo: &Path,
    run_dir: &Path,
    additional_runs: &[PathBuf],
    expected_admission_policy: Option<&str>,
) -> Result<Value> {
    let support_path = run_dir.join("support.json");
    let support = read_json(&support_path)?;
    if get_str(&support, "lane")? != "kv-concurrency" {
        bail!(
            "expected kv-concurrency support, got {:?}",
            support.get("lane")
        );
    }
    let measurement_identity = if let Some(expected) = expected_admission_policy {
        Some(support_identity(
            &support,
            "kv-concurrency",
            Some(repo),
            Some("run-kv-concurrency-plan.sh"),
            expected,
            &support_path.display().to_string(),
        )?)
    } else {
        None
    };

    let jobs = parse_jobs(run_dir, &support)?;
    let mut rows = read_cases(run_dir)?;
    for extra in additional_runs {
        if let Some(identity) = &measurement_identity {
            let extra_support_path = extra.join("support.json");
            if !extra_support_path.is_file() {
                bail!(
                    "missing additional-run support: {}",
                    extra_support_path.display()
                );
            }
            let extra_support = read_json(&extra_support_path)?;
            let extra_identity = support_identity(
                &extra_support,
                "kv-concurrency",
                Some(repo),
                Some("run-kv-concurrency-plan.sh"),
                expected_admission_policy.context("missing expected admission policy")?,
                &extra_support_path.display().to_string(),
            )?;
            if &extra_identity != identity {
                bail!(
                    "additional run measurement identity differs: {}",
                    extra.display()
                );
            }
        }
        rows.extend(read_cases(extra)?);
    }

    let mut seen_rows = BTreeSet::new();
    for row in &rows {
        let identity = (
            client_key(row)?,
            get_i64(row, "trial")?,
            get_i64(row, "records")?,
            get_i64(row, "ops_requested")?,
        );
        if !seen_rows.insert(identity.clone()) {
            bail!("duplicate result identity across runs: {identity:?}");
        }
    }

    let mut planned_groups: BTreeMap<ClientKey, Value> = BTreeMap::new();
    let mut planned_families: BTreeMap<FamilyKey, BTreeSet<i64>> = BTreeMap::new();
    let mut planned_family_work: BTreeMap<FamilyKey, (i64, i64)> = BTreeMap::new();
    for job in &jobs {
        let ck = client_key(job)?;
        planned_groups
            .entry(ck.clone())
            .or_insert_with(|| job.clone());
        let fk = ck.family;
        planned_families
            .entry(fk.clone())
            .or_default()
            .insert(get_i64(job, "clients")?);
        let work = (get_i64(job, "records")?, get_i64(job, "ops")?);
        if let Some(previous) = planned_family_work.get(&fk) {
            if *previous != work {
                bail!("family total-work mismatch: {fk:?}: {previous:?} != {work:?}");
            }
        } else {
            planned_family_work.insert(fk, work);
        }
    }

    let mut observed: BTreeMap<ClientKey, Vec<Value>> = BTreeMap::new();
    for row in &rows {
        let ck = client_key(row)?;
        let template = planned_groups.get(&ck).with_context(|| {
            format!(
                "result outside planned matrix: {:?}: {ck:?}",
                row.get("_case_file")
            )
        })?;
        let expected_ops = get_i64(template, "ops")?;
        let actual_ops = get_i64(row, "ops_requested")?;
        if actual_ops != expected_ops {
            bail!(
                "stock result op mismatch for {:?}: {actual_ops} != {expected_ops}",
                row.get("_case_file")
            );
        }
        if get_i64(row, "ops_completed")? != actual_ops {
            bail!("partial result: {:?}", row.get("_case_file"));
        }
        let elapsed = get_f64(row, "elapsed_s")?;
        let rate = get_f64(row, "ops_per_s")?;
        if !elapsed.is_finite() || elapsed <= 0.0 || !rate.is_finite() || rate <= 0.0 {
            bail!("invalid timing: {:?}", row.get("_case_file"));
        }
        observed.entry(ck).or_default().push(row.clone());
    }

    let mut client_groups = Vec::new();
    let mut missing_probe_cases = Vec::new();
    for (ck, template) in &planned_groups {
        let samples = observed.get(ck).map(Vec::as_slice).unwrap_or(&[]);
        let mut entry = object(template)?.clone();
        entry.remove("trial");
        let mut observed_trials: Vec<i64> = samples
            .iter()
            .map(|row| get_i64(row, "trial"))
            .collect::<Result<Vec<_>>>()?;
        observed_trials.sort_unstable();
        entry.insert("observed_trials".to_owned(), json!(observed_trials));
        entry.insert("observed_count".to_owned(), json!(samples.len()));
        if samples.is_empty() {
            let mut probe = template.clone();
            probe
                .as_object_mut()
                .context("planned group must be object")?
                .insert("trial".to_owned(), json!(1));
            missing_probe_cases.push(probe);
        } else {
            let mut elapsed: Vec<f64> = samples
                .iter()
                .map(|row| get_f64(row, "elapsed_s"))
                .collect::<Result<Vec<_>>>()?;
            let mut rates: Vec<f64> = samples
                .iter()
                .map(|row| get_f64(row, "ops_per_s"))
                .collect::<Result<Vec<_>>>()?;
            let min_elapsed = elapsed.iter().copied().fold(f64::INFINITY, f64::min);
            let max_elapsed = elapsed.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            entry.insert(
                "median_elapsed_s".to_owned(),
                number(median(&mut elapsed)?)?,
            );
            entry.insert("median_ops_per_s".to_owned(), number(median(&mut rates)?)?);
            entry.insert("min_elapsed_s".to_owned(), number(min_elapsed)?);
            entry.insert("max_elapsed_s".to_owned(), number(max_elapsed)?);
        }
        client_groups.push(Value::Object(entry));
    }

    let mut by_family_observed: BTreeMap<FamilyKey, Vec<Value>> = BTreeMap::new();
    for item in &client_groups {
        if get_i64(item, "observed_count")? != 0 {
            by_family_observed
                .entry(family_key(item)?)
                .or_default()
                .push(item.clone());
        }
    }

    let trials = get_i64(&support, "trials")?;
    let mut families = Vec::new();
    let mut counts: BTreeMap<String, i64> = BTreeMap::new();
    for (fk, clients) in &planned_families {
        let observed_groups = by_family_observed.get(fk).map(Vec::as_slice).unwrap_or(&[]);
        let observed_clients: BTreeSet<i64> = observed_groups
            .iter()
            .map(|item| get_i64(item, "clients"))
            .collect::<Result<_>>()?;
        let missing_clients: Vec<i64> = clients.difference(&observed_clients).copied().collect();
        let (current_records, current_ops) = planned_family_work
            .get(fk)
            .copied()
            .context("missing planned family work")?;
        let mut entry = Map::new();
        entry.insert("scenario".to_owned(), json!(fk.scenario));
        entry.insert("engine".to_owned(), json!(fk.engine));
        entry.insert("durability".to_owned(), json!(fk.durability));
        entry.insert("workload".to_owned(), json!(fk.workload));
        entry.insert(
            "clients".to_owned(),
            json!(clients.iter().copied().collect::<Vec<_>>()),
        );
        entry.insert("missing_clients".to_owned(), json!(missing_clients));
        entry.insert("current_records".to_owned(), json!(current_records));
        entry.insert("current_ops".to_owned(), json!(current_ops));
        entry.insert(
            "observed_client_groups".to_owned(),
            json!(observed_groups.len()),
        );
        entry.insert("planned_client_groups".to_owned(), json!(clients.len()));
        entry.insert(
            "semantics".to_owned(),
            json!(if SAFE_OP_SCALE_WORKLOADS.contains(&fk.workload.as_str()) {
                "fixed-keyspace"
            } else {
                "state-changing"
            }),
        );

        if observed_groups.is_empty() {
            entry.insert("status".to_owned(), json!("needs-probe"));
            entry.insert("recommendation".to_owned(), json!("collect-stock-probe"));
        } else {
            let fastest_elapsed = observed_groups
                .iter()
                .map(|item| get_f64(item, "median_elapsed_s"))
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .fold(f64::INFINITY, f64::min);
            let max_rate = observed_groups
                .iter()
                .map(|item| get_f64(item, "median_ops_per_s"))
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .fold(f64::NEG_INFINITY, f64::max);
            entry.insert(
                "fastest_observed_median_elapsed_s".to_owned(),
                number(fastest_elapsed)?,
            );
            entry.insert(
                "max_observed_median_ops_per_s".to_owned(),
                number(max_rate)?,
            );
            if !entry
                .get("missing_clients")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
            {
                entry.insert("status".to_owned(), json!("needs-probe"));
                entry.insert("recommendation".to_owned(), json!("collect-stock-probe"));
            } else if SAFE_OP_SCALE_WORKLOADS.contains(&fk.workload.as_str()) {
                if fastest_elapsed < FLOOR_SECONDS {
                    let suggested = super::rounded_ops(max_rate * TARGET_SECONDS)?;
                    entry.insert("status".to_owned(), json!("undersized"));
                    entry.insert("recommendation".to_owned(), json!("more-ops"));
                    entry.insert(
                        "suggested_ops".to_owned(),
                        json!(current_ops.max(suggested)),
                    );
                    entry.insert("suggested_trials".to_owned(), json!(trials));
                } else {
                    entry.insert("status".to_owned(), json!("sized"));
                    entry.insert("recommendation".to_owned(), json!("keep-stock"));
                    entry.insert("suggested_ops".to_owned(), json!(current_ops));
                    entry.insert("suggested_trials".to_owned(), json!(trials));
                }
            } else {
                let needed_trials = trials.max((TARGET_SECONDS / fastest_elapsed).ceil() as i64);
                entry.insert("suggested_ops".to_owned(), json!(current_ops));
                entry.insert("suggested_trials".to_owned(), json!(needed_trials));
                if fastest_elapsed >= FLOOR_SECONDS {
                    entry.insert("status".to_owned(), json!("sized"));
                    entry.insert("recommendation".to_owned(), json!("keep-stock"));
                } else if needed_trials <= STATEFUL_MAX_TRIALS && fastest_elapsed >= SEVERE_SECONDS
                {
                    entry.insert("status".to_owned(), json!("undersized"));
                    entry.insert("recommendation".to_owned(), json!("more-fresh-trials"));
                } else {
                    entry.insert("status".to_owned(), json!("redesign-required"));
                    entry.insert(
                        "recommendation".to_owned(),
                        json!("state-preserving-longer-window"),
                    );
                }
            }
        }

        if !observed_groups.is_empty() && STATE_CHANGING_WORKLOADS.contains(&fk.workload.as_str()) {
            let fastest = entry
                .get("fastest_observed_median_elapsed_s")
                .and_then(Value::as_f64)
                .context("missing fastest elapsed")?;
            let provisional_trials = trials.max((TARGET_SECONDS / fastest).ceil() as i64);
            entry.insert(
                "provisional_stateful_trials_needed".to_owned(),
                json!(provisional_trials),
            );
            entry.insert(
                "provisional_stateful_risk".to_owned(),
                json!(
                    if fastest < SEVERE_SECONDS || provisional_trials > STATEFUL_MAX_TRIALS {
                        "redesign-required"
                    } else if fastest < FLOOR_SECONDS {
                        "more-fresh-trials"
                    } else {
                        "sized"
                    }
                ),
            );
        }
        let status = entry
            .get("status")
            .and_then(Value::as_str)
            .context("missing family status")?
            .to_owned();
        *counts.entry(status).or_insert(0) += 1;
        families.push(Value::Object(entry));
    }

    let severe_stateful = families
        .iter()
        .filter(|item| {
            item.get("provisional_stateful_risk")
                .and_then(Value::as_str)
                == Some("redesign-required")
        })
        .count();
    Ok(json!({
        "concurrency_sizing_policy_version": POLICY_VERSION,
        "run_dir": run_dir.display().to_string(),
        "additional_run_dirs": additional_runs.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
        "source_support": support,
        "measurement_identity": measurement_identity,
        "thresholds": {
            "floor_seconds": FLOOR_SECONDS,
            "target_seconds": TARGET_SECONDS,
            "severe_seconds": SEVERE_SECONDS,
            "stateful_max_trials": STATEFUL_MAX_TRIALS,
            "safe_more_ops_workloads": SAFE_OP_SCALE_WORKLOADS,
            "state_changing_workloads": ["balanced", "churn", "delete-burst", "tiny-txn", "write-burst"],
        },
        "row_count": rows.len(),
        "planned_case_count": jobs.len(),
        "planned_client_group_count": planned_groups.len(),
        "observed_client_group_count": observed.len(),
        "planned_family_count": planned_families.len(),
        "observed_family_count": by_family_observed.len(),
        "missing_probe_case_count": missing_probe_cases.len(),
        "observed_stateful_redesign_risk_count": severe_stateful,
        "counts": counts,
        "client_groups": client_groups,
        "families": families,
        "missing_probe_cases": missing_probe_cases,
    }))
}

fn markdown(report: &Value) -> Result<String> {
    let mut lines = vec![
        "# Concurrency sizing audit".to_owned(),
        String::new(),
        format!(
            "Rows: **{}** / planned **{}**; client groups observed: **{}** / **{}**; families observed: **{}** / **{}**.",
            get_i64(report, "row_count")?, get_i64(report, "planned_case_count")?,
            get_i64(report, "observed_client_group_count")?, get_i64(report, "planned_client_group_count")?,
            get_i64(report, "observed_family_count")?, get_i64(report, "planned_family_count")?,
        ),
        String::new(),
        "Policy: fixed-keyspace point-read/range-scan/read-heavy families may increase total ops, but the same total work is retained across client counts. State-changing append/churn/delete families never increase ops solely to lengthen a timing; moderately short families add fresh trials, while severe short families require a state-preserving longer-window design.".to_owned(),
        String::new(),
        format!(
            "Observed state-changing families already severe enough to require redesign: **{}**.",
            get_i64(report, "observed_stateful_redesign_risk_count")?
        ),
        String::new(),
        "## Family counts".to_owned(),
    ];
    for (status, count) in object(report.get("counts").context("missing counts")?)? {
        lines.push(format!(
            "- {status}: **{}**",
            value_i64(count).context("invalid count")?
        ));
    }
    lines.extend([
        String::new(),
        format!(
            "Missing one-shot stock probes: **{}**.",
            get_i64(report, "missing_probe_case_count")?
        ),
        String::new(),
        "## Families requiring redesign".to_owned(),
    ]);
    let redesign: Vec<&Value> = report
        .get("families")
        .and_then(Value::as_array)
        .context("missing families")?
        .iter()
        .filter(|item| {
            item.get("provisional_stateful_risk")
                .and_then(Value::as_str)
                == Some("redesign-required")
        })
        .collect();
    if redesign.is_empty() {
        lines.push("- none yet".to_owned());
    } else {
        for item in redesign {
            lines.push(format!(
                "- `{}` {} {} ({}): fastest observed median {:.6}s; stock ops {}; naive fresh-trial requirement {}.",
                get_str(item, "engine")?, get_str(item, "durability")?, get_str(item, "workload")?,
                get_str(item, "scenario")?, get_f64(item, "fastest_observed_median_elapsed_s")?,
                get_i64(item, "current_ops")?, get_i64(item, "provisional_stateful_trials_needed")?,
            ));
        }
    }
    Ok(lines.join("\n") + "\n")
}

pub(crate) fn run_concurrency_sizing_audit(args: AuditArgs<'_>) -> Result<()> {
    let report = audit(
        args.repo,
        args.run_dir,
        args.additional_runs,
        args.expected_admission_policy,
    )?;
    if let Some(path) = args.json_out {
        write_json(path, &report)?;
    }
    if let Some(path) = args.markdown_out {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, markdown(&report)?)?;
    }
    if let Some(path) = args.probe_plan_out {
        let missing = report
            .get("missing_probe_cases")
            .cloned()
            .context("missing probe cases")?;
        let plan = json!({
            "concurrency_probe_plan_version": 1,
            "kind": "stock-client-group-coverage",
            "source_run_dir": args.run_dir.display().to_string(),
            "source_case_count": get_i64(&report, "row_count")?,
            "source_support_sha256": sha256(&args.run_dir.join("support.json"))?,
            "source_jobs_sha256": sha256(&args.run_dir.join("jobs.txt"))?,
            "source_sizing_policy_version": POLICY_VERSION,
            "case_count": missing.as_array().map_or(0, Vec::len),
            "cases": missing,
        });
        write_json(path, &plan)?;
    }
    let summary = json!({
        "row_count": get_i64(&report, "row_count")?,
        "planned_case_count": get_i64(&report, "planned_case_count")?,
        "observed_client_group_count": get_i64(&report, "observed_client_group_count")?,
        "planned_client_group_count": get_i64(&report, "planned_client_group_count")?,
        "observed_family_count": get_i64(&report, "observed_family_count")?,
        "planned_family_count": get_i64(&report, "planned_family_count")?,
        "missing_probe_case_count": get_i64(&report, "missing_probe_case_count")?,
        "observed_stateful_redesign_risk_count": get_i64(&report, "observed_stateful_redesign_risk_count")?,
        "counts": report.get("counts").cloned().context("missing counts")?,
    });
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{STATEFUL_MAX_TRIALS, audit, markdown};
    use crate::planning::json_truthy;
    use crate::planning::{TestDir, kv_support, repo};
    use anyhow::Result;
    use serde_json::{Value, json};
    use std::fs;

    fn row(workload: &str, clients: i64, trial: i64, elapsed: f64, rate: f64) -> Value {
        let ops = if workload == "point-read" {
            200_000
        } else if matches!(workload, "range-scan" | "tiny-txn") {
            1_000
        } else {
            50_000
        };
        json!({
            "scenario": "concurrency-primary", "engine": "fast", "durability": "sync",
            "workload": workload, "clients": clients, "trial": trial, "records": 100_000,
            "ops_requested": ops, "ops_completed": ops, "elapsed_s": elapsed, "ops_per_s": rate,
        })
    }

    fn make_run(jobs: &[String], rows: &[Value]) -> Result<TestDir> {
        let dir = TestDir::new("audit")?;
        fs::create_dir_all(dir.0.join("cases"))?;
        fs::write(
            dir.0.join("support.json"),
            serde_json::to_vec(&kv_support(&repo()?)?)?,
        )?;
        fs::write(dir.0.join("jobs.txt"), jobs.join("\n") + "\n")?;
        for (index, item) in rows.iter().enumerate() {
            fs::write(
                dir.0.join("cases").join(format!("r{index}.json")),
                serde_json::to_vec(item)?,
            )?;
        }
        Ok(dir)
    }

    #[test]
    fn missing_group_becomes_one_probe_case() -> Result<()> {
        let jobs: Vec<String> = [1, 2, 3]
            .into_iter()
            .flat_map(|trial| {
                [1, 8]
                    .into_iter()
                    .map(move |clients| format!("primary|fast|sync|point-read|{clients}|{trial}"))
            })
            .collect();
        let dir = make_run(&jobs, &[row("point-read", 1, 2, 1.0, 200_000.0)])?;
        let report = audit(&repo()?, &dir.0, &[], None)?;
        assert_eq!(report["planned_client_group_count"], 2);
        assert_eq!(report["observed_client_group_count"], 1);
        assert_eq!(report["missing_probe_case_count"], 1);
        assert_eq!(report["missing_probe_cases"][0]["clients"], 8);
        Ok(())
    }

    #[test]
    fn fixed_and_stateful_recommendations_match_policy() -> Result<()> {
        let point_jobs: Vec<String> = [1, 2, 3]
            .into_iter()
            .flat_map(|trial| {
                [1, 8]
                    .into_iter()
                    .map(move |clients| format!("primary|fast|sync|point-read|{clients}|{trial}"))
            })
            .collect();
        let point = make_run(
            &point_jobs,
            &[
                row("point-read", 1, 1, 1.0, 200_000.0),
                row("point-read", 8, 1, 0.5, 400_000.0),
            ],
        )?;
        let report = audit(&repo()?, &point.0, &[], None)?;
        assert_eq!(report["families"][0]["status"], "undersized");
        assert_eq!(report["families"][0]["suggested_ops"], 2_000_000);

        let write_jobs: Vec<String> = [1, 2, 3]
            .into_iter()
            .flat_map(|trial| {
                [1, 8]
                    .into_iter()
                    .map(move |clients| format!("primary|fast|sync|write-burst|{clients}|{trial}"))
            })
            .collect();
        let write = make_run(
            &write_jobs,
            &[
                row("write-burst", 1, 1, 0.8, 62_500.0),
                row("write-burst", 8, 1, 0.5, 100_000.0),
            ],
        )?;
        let report = audit(&repo()?, &write.0, &[], None)?;
        assert_eq!(report["families"][0]["recommendation"], "more-fresh-trials");
        assert_eq!(report["families"][0]["suggested_trials"], 6);
        Ok(())
    }

    #[test]
    fn severe_stateful_family_requires_redesign_and_markdown_surfaces_it() -> Result<()> {
        let jobs: Vec<String> = [1, 2, 3]
            .into_iter()
            .flat_map(|trial| {
                [1, 8]
                    .into_iter()
                    .map(move |clients| format!("primary|fast|sync|tiny-txn|{clients}|{trial}"))
            })
            .collect();
        let dir = make_run(
            &jobs,
            &[
                row("tiny-txn", 1, 1, 0.02, 50_000.0),
                row("tiny-txn", 8, 1, 0.004, 250_000.0),
            ],
        )?;
        let report = audit(&repo()?, &dir.0, &[], None)?;
        assert_eq!(report["families"][0]["status"], "redesign-required");
        assert!(
            report["families"][0]["suggested_trials"]
                .as_i64()
                .is_some_and(|value| value > STATEFUL_MAX_TRIALS)
        );
        assert!(markdown(&report)?.contains("severe enough to require redesign: **1**"));
        Ok(())
    }

    #[test]
    fn partial_ops_and_wrong_admission_are_rejected() -> Result<()> {
        let jobs = vec!["primary|fast|sync|point-read|1|1".to_owned()];
        let mut bad = row("point-read", 1, 1, 1.0, 200_000.0);
        bad["ops_completed"] = json!(199_999);
        let dir = make_run(&jobs, &[bad])?;
        assert!(audit(&repo()?, &dir.0, &[], None).is_err());

        let good = make_run(&jobs, &[row("point-read", 1, 1, 1.0, 200_000.0)])?;
        let mut support: Value =
            serde_json::from_str(&fs::read_to_string(good.0.join("support.json"))?)?;
        support["admission_policy"] = json!("pre-io+pre/post-external-v2");
        fs::write(good.0.join("support.json"), serde_json::to_vec(&support)?)?;
        audit(&repo()?, &good.0, &[], Some("pre-io+pre/post-external-v2"))?;
        assert!(audit(&repo()?, &good.0, &[], Some("wrong-policy")).is_err());
        Ok(())
    }

    #[test]
    fn exact_plan_jobs_are_supported() -> Result<()> {
        let dir = TestDir::new("audit-exact")?;
        fs::create_dir_all(dir.0.join("cases"))?;
        let mut support = kv_support(&repo()?)?;
        support["trials"] = json!(1);
        fs::write(dir.0.join("support.json"), serde_json::to_vec(&support)?)?;
        fs::write(
            dir.0.join("jobs.txt"),
            "concurrency-primary|fast|sync|point-read|1|100000|200000|1|growth|append|0\n",
        )?;
        fs::write(
            dir.0.join("cases/r0.json"),
            serde_json::to_vec(&row("point-read", 1, 1, 1.0, 200_000.0))?,
        )?;
        let report = audit(&repo()?, &dir.0, &[], None)?;
        assert_eq!(report["planned_case_count"], 1);
        assert_eq!(report["families"][0]["current_ops"], 200_000);
        Ok(())
    }

    #[test]
    fn additional_run_fills_missing_group() -> Result<()> {
        let jobs: Vec<String> = [1, 2, 3]
            .into_iter()
            .flat_map(|trial| {
                [1, 8]
                    .into_iter()
                    .map(move |clients| format!("primary|fast|sync|point-read|{clients}|{trial}"))
            })
            .collect();
        let stock = make_run(&jobs, &[row("point-read", 1, 1, 1.0, 200_000.0)])?;
        let extra = TestDir::new("audit-extra")?;
        fs::create_dir_all(extra.0.join("cases"))?;
        fs::write(
            extra.0.join("cases/c8.json"),
            serde_json::to_vec(&row("point-read", 8, 2, 0.5, 400_000.0))?,
        )?;
        let report = audit(&repo()?, &stock.0, std::slice::from_ref(&extra.0), None)?;
        assert_eq!(report["missing_probe_case_count"], 0);
        assert_eq!(report["observed_client_group_count"], 2);
        Ok(())
    }

    #[test]
    fn missing_problems_truthiness_helper_is_consistent() {
        assert!(!json_truthy(None));
        assert!(!json_truthy(Some(&json!([]))));
        assert!(json_truthy(Some(&json!(["x"]))));
    }
}
