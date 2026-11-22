use super::kv_identity::{FamilyKey, WorkSpec, result_key};
use super::{
    get_f64, get_i64, get_str, median, object, read_json, sha256, support_identity, value_i64,
    write_json,
};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json};
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

#[derive(Clone, Copy)]
pub struct AuditArgs<'a> {
    pub(crate) repo: &'a Path,
    pub(crate) run_dir: &'a Path,
    pub(crate) additional_runs: &'a [PathBuf],
    pub(crate) json_out: Option<&'a Path>,
    pub(crate) markdown_out: Option<&'a Path>,
    pub(crate) probe_plan_out: Option<&'a Path>,
    pub(crate) expected_admission_policy: Option<&'a str>,
}

struct PlannedFamily<'a> {
    work: WorkSpec,
    clients: BTreeMap<i64, &'a Value>,
}

type PlannedMatrix<'a> = BTreeMap<FamilyKey, PlannedFamily<'a>>;
type ObservedMatrix<'a> = BTreeMap<FamilyKey, BTreeMap<i64, Vec<&'a Value>>>;

fn planned_matrix(jobs: &[Value]) -> Result<PlannedMatrix<'_>> {
    let mut families = PlannedMatrix::new();
    let mut identities = BTreeSet::new();
    for job in jobs {
        let key = result_key(job)?;
        if key.clients < 1
            || key.trial < 1
            || key.work.records < 1
            || key.work.ops < 1
            || key.work.bounded_churn_slots < 0
        {
            bail!("invalid planned case dimensions: {key:?}");
        }
        if !identities.insert(key.clone()) {
            bail!("duplicate planned case identity: {key:?}");
        }
        let family = families
            .entry(key.family.clone())
            .or_insert_with(|| PlannedFamily {
                work: key.work.clone(),
                clients: BTreeMap::new(),
            });
        if family.work != key.work {
            bail!(
                "family total-work or state mismatch: {:?}: {:?} != {:?}",
                key.family,
                family.work,
                key.work
            );
        }
        family.clients.entry(key.clients).or_insert(job);
    }
    Ok(families)
}

fn observed_matrix<'a>(
    rows: &'a [Value],
    planned: &PlannedMatrix<'_>,
) -> Result<ObservedMatrix<'a>> {
    let mut observed = ObservedMatrix::new();
    let mut identities = BTreeSet::new();
    for row in rows {
        let key = result_key(row)?;
        if key.trial < 1 {
            bail!("invalid observed trial: {key:?}");
        }
        let family = planned
            .get(&key.family)
            .filter(|family| family.clients.contains_key(&key.clients))
            .with_context(|| {
                format!(
                    "result outside planned matrix: {:?}: {key:?}",
                    row.get("_case_file")
                )
            })?;
        if get_i64(row, "ops_requested")? != key.work.ops || key.work != family.work {
            bail!(
                "stock result work identity mismatch for {:?}: {:?} != {:?}",
                row.get("_case_file"),
                key.work,
                family.work
            );
        }
        if get_i64(row, "ops_completed")? != key.work.ops {
            bail!("partial result: {:?}", row.get("_case_file"));
        }
        let elapsed = get_f64(row, "elapsed_s")?;
        let rate = get_f64(row, "ops_per_s")?;
        if !elapsed.is_finite() || elapsed <= 0.0 || !rate.is_finite() || rate <= 0.0 {
            bail!("invalid timing: {:?}", row.get("_case_file"));
        }
        if !identities.insert(key.clone()) {
            bail!("duplicate result identity across runs: {key:?}");
        }
        observed
            .entry(key.family)
            .or_default()
            .entry(key.clients)
            .or_default()
            .push(row);
    }
    Ok(observed)
}

#[derive(Serialize)]
struct ClientStats {
    median_elapsed_s: f64,
    #[serde(rename = "median_ops_per_s")]
    median_rate: f64,
    min_elapsed_s: f64,
    max_elapsed_s: f64,
}

struct ClientEvidence {
    observed_trials: Vec<i64>,
    stats: Option<ClientStats>,
}

impl ClientEvidence {
    fn from_samples(samples: &[&Value]) -> Result<Self> {
        let mut observed_trials = samples
            .iter()
            .map(|row| get_i64(row, "trial"))
            .collect::<Result<Vec<_>>>()?;
        observed_trials.sort_unstable();
        let stats = if samples.is_empty() {
            None
        } else {
            let mut elapsed = samples
                .iter()
                .map(|row| get_f64(row, "elapsed_s"))
                .collect::<Result<Vec<_>>>()?;
            let mut rates = samples
                .iter()
                .map(|row| get_f64(row, "ops_per_s"))
                .collect::<Result<Vec<_>>>()?;
            Some(ClientStats {
                min_elapsed_s: elapsed.iter().copied().fold(f64::INFINITY, f64::min),
                max_elapsed_s: elapsed.iter().copied().fold(f64::NEG_INFINITY, f64::max),
                median_elapsed_s: median(&mut elapsed)?,
                median_rate: median(&mut rates)?,
            })
        };
        Ok(Self {
            observed_trials,
            stats,
        })
    }

    fn report(&self, template: &Value) -> Result<Value> {
        let mut entry = object(template)?.clone();
        entry.remove("trial");
        entry.insert("observed_trials".to_owned(), json!(self.observed_trials));
        entry.insert(
            "observed_count".to_owned(),
            json!(self.observed_trials.len()),
        );
        if let Some(stats) = &self.stats {
            let stats = serde_json::to_value(stats)?;
            entry.extend(object(&stats)?.clone());
        }
        Ok(Value::Object(entry))
    }
}

#[derive(Serialize)]
struct FamilyStats {
    fastest_observed_median_elapsed_s: f64,
    max_observed_median_ops_per_s: f64,
}

impl FamilyStats {
    fn from_clients<'a>(stats: impl Iterator<Item = &'a ClientStats>) -> Option<Self> {
        stats
            .map(|stats| Self {
                fastest_observed_median_elapsed_s: stats.median_elapsed_s,
                max_observed_median_ops_per_s: stats.median_rate,
            })
            .reduce(|left, right| Self {
                fastest_observed_median_elapsed_s: left
                    .fastest_observed_median_elapsed_s
                    .min(right.fastest_observed_median_elapsed_s),
                max_observed_median_ops_per_s: left
                    .max_observed_median_ops_per_s
                    .max(right.max_observed_median_ops_per_s),
            })
    }
}

#[derive(Serialize)]
struct SizingDecision {
    status: &'static str,
    recommendation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    suggested_ops: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    suggested_trials: Option<i64>,
}

#[derive(Serialize)]
struct StatefulRisk {
    provisional_stateful_trials_needed: i64,
    provisional_stateful_risk: &'static str,
}

fn stateful_risk(fastest: f64, trials: i64) -> StatefulRisk {
    let needed = trials.max(f64_to_i64!((TARGET_SECONDS / fastest).ceil()));
    StatefulRisk {
        provisional_stateful_trials_needed: needed,
        provisional_stateful_risk: if fastest < SEVERE_SECONDS || needed > STATEFUL_MAX_TRIALS {
            "redesign-required"
        } else if fastest < FLOOR_SECONDS {
            "more-fresh-trials"
        } else {
            "sized"
        },
    }
}

fn sizing_decision(
    fixed: bool,
    complete: bool,
    evidence: Option<&FamilyStats>,
    current_ops: i64,
    trials: i64,
) -> Result<SizingDecision> {
    let Some(evidence) = evidence.filter(|_| complete) else {
        return Ok(SizingDecision {
            status: "needs-probe",
            recommendation: "collect-stock-probe",
            suggested_ops: None,
            suggested_trials: None,
        });
    };
    let fastest = evidence.fastest_observed_median_elapsed_s;
    if fixed {
        let (status, recommendation, ops) = if fastest < FLOOR_SECONDS {
            (
                "undersized",
                "more-ops",
                current_ops.max(super::rounded_ops(
                    evidence.max_observed_median_ops_per_s * TARGET_SECONDS,
                )?),
            )
        } else {
            ("sized", "keep-stock", current_ops)
        };
        return Ok(SizingDecision {
            status,
            recommendation,
            suggested_ops: Some(ops),
            suggested_trials: Some(trials),
        });
    }
    let risk = stateful_risk(fastest, trials);
    let (status, recommendation) = if fastest >= FLOOR_SECONDS {
        ("sized", "keep-stock")
    } else if risk.provisional_stateful_risk == "more-fresh-trials" {
        ("undersized", "more-fresh-trials")
    } else {
        ("redesign-required", "state-preserving-longer-window")
    };
    Ok(SizingDecision {
        status,
        recommendation,
        suggested_ops: Some(current_ops),
        suggested_trials: Some(risk.provisional_stateful_trials_needed),
    })
}

#[derive(Serialize)]
struct FamilyReport<'a> {
    #[serde(flatten)]
    key: &'a FamilyKey,
    clients: Vec<i64>,
    missing_clients: Vec<i64>,
    current_records: i64,
    current_ops: i64,
    observed_client_groups: usize,
    planned_client_groups: usize,
    semantics: &'static str,
    #[serde(flatten)]
    stats: Option<FamilyStats>,
    #[serde(flatten)]
    decision: SizingDecision,
    #[serde(flatten)]
    provisional: Option<StatefulRisk>,
}

fn family_report<'a>(
    key: &'a FamilyKey,
    plan: &PlannedFamily<'_>,
    evidence: &BTreeMap<i64, ClientEvidence>,
    trials: i64,
) -> Result<FamilyReport<'a>> {
    let clients: Vec<_> = plan.clients.keys().copied().collect();
    let missing_clients: Vec<_> = evidence
        .iter()
        .filter(|(_, item)| item.stats.is_none())
        .map(|(count, _)| *count)
        .collect();
    let stats =
        FamilyStats::from_clients(evidence.values().filter_map(|client| client.stats.as_ref()));
    let fixed = SAFE_OP_SCALE_WORKLOADS.contains(&key.workload.as_str());
    let decision = sizing_decision(
        fixed,
        missing_clients.is_empty(),
        stats.as_ref(),
        plan.work.ops,
        trials,
    )?;
    let provisional = stats
        .as_ref()
        .filter(|_| STATE_CHANGING_WORKLOADS.contains(&key.workload.as_str()))
        .map(|stats| stateful_risk(stats.fastest_observed_median_elapsed_s, trials));
    let observed_client_groups = clients.len() - missing_clients.len();
    Ok(FamilyReport {
        key,
        planned_client_groups: clients.len(),
        clients,
        missing_clients,
        current_records: plan.work.records,
        current_ops: plan.work.ops,
        observed_client_groups,
        semantics: if fixed {
            "fixed-keyspace"
        } else {
            "state-changing"
        },
        stats,
        decision,
        provisional,
    })
}

#[derive(Default)]
struct MatrixSummary<'a> {
    client_groups: Vec<Value>,
    families: Vec<FamilyReport<'a>>,
    missing_probe_cases: Vec<Value>,
    counts: BTreeMap<&'static str, i64>,
    planned_clients: usize,
    observed_clients: usize,
    observed_families: usize,
    severe_stateful: usize,
}

fn summarize_matrix<'a>(
    planned: &'a PlannedMatrix<'_>,
    observed: &ObservedMatrix<'_>,
    trials: i64,
) -> Result<MatrixSummary<'a>> {
    let mut summary = MatrixSummary::default();
    for (key, plan) in planned {
        let mut evidence = BTreeMap::new();
        let family_observed = observed.get(key);
        for (&clients, &template) in &plan.clients {
            let samples = family_observed
                .and_then(|family| family.get(&clients))
                .map_or(&[][..], Vec::as_slice);
            let client = ClientEvidence::from_samples(samples)?;
            if client.stats.is_none() {
                let mut probe = template.clone();
                probe
                    .as_object_mut()
                    .context("planned group must be object")?
                    .insert("trial".to_owned(), json!(1));
                summary.missing_probe_cases.push(probe);
            }
            summary.client_groups.push(client.report(template)?);
            evidence.insert(clients, client);
        }
        let family = family_report(key, plan, &evidence, trials)?;
        summary.planned_clients += family.planned_client_groups;
        summary.observed_clients += family.observed_client_groups;
        summary.observed_families += usize::from(family.observed_client_groups != 0);
        summary.severe_stateful += usize::from(
            family
                .provisional
                .as_ref()
                .is_some_and(|risk| risk.provisional_stateful_risk == "redesign-required"),
        );
        *summary.counts.entry(family.decision.status).or_default() += 1;
        summary.families.push(family);
    }
    Ok(summary)
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

    let planned = planned_matrix(&jobs)?;
    let observed = observed_matrix(&rows, &planned)?;
    let summary = summarize_matrix(&planned, &observed, get_i64(&support, "trials")?)?;
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
        "planned_client_group_count": summary.planned_clients,
        "observed_client_group_count": summary.observed_clients,
        "planned_family_count": planned.len(),
        "observed_family_count": summary.observed_families,
        "missing_probe_case_count": summary.missing_probe_cases.len(),
        "observed_stateful_redesign_risk_count": summary.severe_stateful,
        "counts": summary.counts,
        "client_groups": summary.client_groups,
        "families": summary.families,
        "missing_probe_cases": summary.missing_probe_cases,
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

pub fn run_concurrency_sizing_audit(args: AuditArgs<'_>) -> Result<()> {
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
    fn results_must_match_the_planned_work_and_state() -> Result<()> {
        let jobs = vec![
            "concurrency-primary|fast|sync|point-read|1|100000|200000|1|growth|append|0".to_owned(),
        ];
        for (field, value) in [
            ("records", json!(99_999)),
            ("state_evolution", json!("bounded")),
            ("state_evolution", json!(42)),
            ("write_pattern", json!("update-uniform")),
            ("bounded_churn_slots", json!(8192)),
            ("bounded_churn_slots", json!("malformed")),
            ("ops", json!(100)),
        ] {
            let mut bad = row("point-read", 1, 1, 1.0, 200_000.0);
            bad[field] = value;
            let dir = make_run(&jobs, &[bad])?;
            assert!(audit(&repo()?, &dir.0, &[], None).is_err(), "{field}");
        }
        let mismatched = [
            jobs[0].clone(),
            "concurrency-primary|fast|sync|point-read|8|100000|200000|1|bounded|update-uniform|8192".to_owned(),
        ];
        let dir = make_run(&mismatched, &[])?;
        assert!(audit(&repo()?, &dir.0, &[], None).is_err());
        let duplicate = make_run(&[jobs[0].clone(), jobs[0].clone()], &[])?;
        assert!(audit(&repo()?, &duplicate.0, &[], None).is_err());
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
