use super::{
    DEFAULT_BOUNDED_CHURN_SLOTS, KV_PLAN_VERSION, canonical_identity_sha256, case_paths,
    case_set_sha256, clone_field, get_f64, get_i64, get_str, json_truthy, read_json, rounded_ops,
    sha256, support_identity, write_json,
};
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use std::{collections::BTreeMap, path::Path};

const FIXED_WORKLOADS: [&str; 3] = ["point-read", "range-scan", "read-heavy"];
const BOUNDED_WORKLOADS: [&str; 4] = ["balanced", "tiny-txn", "write-burst", "churn"];
const DELETE_WORKLOAD: &str = "delete-burst";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct FamilyKey {
    scenario: String,
    engine: String,
    durability: String,
    workload: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ResultKey {
    family: FamilyKey,
    clients: i64,
    records: i64,
    ops: i64,
    trial: i64,
    state_evolution: String,
    write_pattern: String,
    bounded_churn_slots: i64,
}

fn family_key(value: &Value) -> Result<FamilyKey> {
    Ok(FamilyKey {
        scenario: get_str(value, "scenario")?.to_owned(),
        engine: get_str(value, "engine")?.to_owned(),
        durability: get_str(value, "durability")?.to_owned(),
        workload: get_str(value, "workload")?.to_owned(),
    })
}

fn result_key(value: &Value) -> Result<ResultKey> {
    let ops = value
        .get("ops")
        .or_else(|| value.get("ops_requested"))
        .and_then(super::value_i64)
        .context("missing or invalid ops/ops_requested")?;
    Ok(ResultKey {
        family: family_key(value)?,
        clients: get_i64(value, "clients")?,
        records: get_i64(value, "records")?,
        ops,
        trial: get_i64(value, "trial")?,
        state_evolution: value
            .get("state_evolution")
            .and_then(Value::as_str)
            .unwrap_or("growth")
            .to_owned(),
        write_pattern: value
            .get("write_pattern")
            .and_then(Value::as_str)
            .unwrap_or("append")
            .to_owned(),
        bounded_churn_slots: value
            .get("bounded_churn_slots")
            .and_then(super::value_i64)
            .unwrap_or(0),
    })
}

fn load_complete_audit(path: &Path) -> Result<Value> {
    let audit = read_json(path)?;
    if audit
        .get("concurrency_sizing_policy_version")
        .and_then(Value::as_i64)
        != Some(1)
    {
        bail!(
            "unsupported sizing audit version: {:?}",
            audit.get("concurrency_sizing_policy_version")
        );
    }
    if get_i64(&audit, "missing_probe_case_count")? != 0 {
        bail!(
            "sizing audit still has {} missing client groups",
            get_i64(&audit, "missing_probe_case_count")?
        );
    }
    let families = audit
        .get("families")
        .and_then(Value::as_array)
        .context("sizing audit families must be an array")?;
    if i64::try_from(families.len()).context("family count overflow")?
        != get_i64(&audit, "planned_family_count")?
    {
        bail!("sizing audit family count mismatch");
    }
    let pending: Vec<FamilyKey> = families
        .iter()
        .filter(|family| {
            family.get("status").and_then(Value::as_str) == Some("needs-probe")
                || json_truthy(family.get("missing_clients"))
        })
        .map(family_key)
        .collect::<Result<_>>()?;
    if !pending.is_empty() {
        bail!(
            "sizing audit contains incomplete families: {:?}",
            &pending[..pending.len().min(5)]
        );
    }
    if !audit
        .get("measurement_identity")
        .is_some_and(Value::is_object)
    {
        bail!("sizing audit is missing measurement identity");
    }
    Ok(audit)
}

fn target_seconds(audit: &Value) -> Result<f64> {
    let thresholds = audit.get("thresholds").context("missing thresholds")?;
    let value = get_f64(thresholds, "target_seconds")?;
    if !value.is_finite() || value <= 0.0 {
        bail!("invalid target_seconds={value}");
    }
    Ok(value)
}

fn estimated_ops(family: &Value, target_s: f64) -> Result<i64> {
    let rate = get_f64(family, "max_observed_median_ops_per_s")?;
    let current = get_i64(family, "current_ops")?;
    Ok(current.max(rounded_ops(rate * target_s)?))
}

fn case_template(
    family: &Value,
    ops: i64,
    clients: i64,
    trial: i64,
    state_evolution: &str,
    write_pattern: &str,
    bounded_churn_slots: i64,
) -> Result<Value> {
    Ok(json!({
        "scenario": clone_field(family, "scenario")?,
        "engine": clone_field(family, "engine")?,
        "durability": clone_field(family, "durability")?,
        "workload": clone_field(family, "workload")?,
        "clients": clients,
        "records": get_i64(family, "current_records")?,
        "ops": ops,
        "trial": trial,
        "state_evolution": state_evolution,
        "write_pattern": write_pattern,
        "bounded_churn_slots": bounded_churn_slots,
    }))
}

fn build_calibration_plan(audit_path: &Path) -> Result<Value> {
    let audit = load_complete_audit(audit_path)?;
    let target_s = target_seconds(&audit)?;
    let mut cases = Vec::new();
    let mut calibrated_families = Vec::new();
    for family in audit
        .get("families")
        .and_then(Value::as_array)
        .context("missing families")?
    {
        let workload = get_str(family, "workload")?;
        let (ops, state, write_pattern, slots, reason) = if FIXED_WORKLOADS.contains(&workload) {
            if get_str(family, "status")? != "undersized" {
                continue;
            }
            (
                get_i64(family, "suggested_ops")?,
                "growth",
                "append",
                0,
                "fixed-keyspace-resize",
            )
        } else if BOUNDED_WORKLOADS.contains(&workload) {
            (
                estimated_ops(family, target_s)?,
                "bounded",
                "update-uniform",
                DEFAULT_BOUNDED_CHURN_SLOTS,
                "bounded-steady-state",
            )
        } else if workload == DELETE_WORKLOAD {
            continue;
        } else {
            bail!("unclassified workload: {workload}");
        };
        let clients = family
            .get("clients")
            .and_then(Value::as_array)
            .context("family clients must be an array")?;
        calibrated_families.push(json!({
            "scenario": clone_field(family, "scenario")?,
            "engine": clone_field(family, "engine")?,
            "durability": clone_field(family, "durability")?,
            "workload": workload,
            "reason": reason,
            "stock_ops": get_i64(family, "current_ops")?,
            "probe_ops": ops,
            "clients": clients.iter().map(|value| value.as_i64().context("invalid family client")).collect::<Result<Vec<_>>>()?,
            "state_evolution": state,
            "write_pattern": write_pattern,
            "bounded_churn_slots": slots,
        }));
        for client in clients {
            cases.push(case_template(
                family,
                ops,
                client.as_i64().context("invalid family client")?,
                1,
                state,
                write_pattern,
                slots,
            )?);
        }
    }
    let identity = audit
        .get("measurement_identity")
        .cloned()
        .context("missing measurement identity")?;
    Ok(json!({
        "concurrency_probe_plan_version": KV_PLAN_VERSION,
        "kind": "steady-sizing-calibration",
        "expect_trials": 1,
        "source_sizing_audit_sha256": sha256(audit_path)?,
        "source_sizing_policy_version": clone_field(&audit, "concurrency_sizing_policy_version")?,
        "expected_measurement_identity": identity,
        "expected_measurement_identity_sha256": canonical_identity_sha256(audit.get("measurement_identity").context("missing measurement identity")?)?,
        "target_seconds": target_s,
        "bounded_churn_slots": DEFAULT_BOUNDED_CHURN_SLOTS,
        "family_count": calibrated_families.len(),
        "families": calibrated_families,
        "case_count": cases.len(),
        "cases": cases,
    }))
}

struct CalibrationResults {
    plan: Value,
    results: BTreeMap<ResultKey, Value>,
    provenance: Map<String, Value>,
}

fn load_calibration_results(
    repo: &Path,
    plan_path: &Path,
    run_dir: &Path,
) -> Result<CalibrationResults> {
    let plan = read_json(plan_path)?;
    if get_i64(&plan, "concurrency_probe_plan_version")? != KV_PLAN_VERSION
        || get_str(&plan, "kind")? != "steady-sizing-calibration"
    {
        bail!("not a steady-sizing calibration plan");
    }
    let support_path = run_dir.join("support.json");
    if !support_path.is_file() {
        bail!("missing calibration support: {}", support_path.display());
    }
    let support = read_json(&support_path)?;
    let plan_sha = sha256(plan_path)?;
    if support.get("plan_sha256").and_then(Value::as_str) != Some(plan_sha.as_str()) {
        bail!(
            "calibration support plan SHA mismatch: {:?} != {plan_sha}",
            support.get("plan_sha256")
        );
    }
    let expected_identity = plan
        .get("expected_measurement_identity")
        .filter(|value| value.is_object())
        .context("calibration plan is missing measurement identity")?;
    let actual_identity = support_identity(
        &support,
        "kv-concurrency",
        Some(repo),
        Some("run-kv-concurrency-plan.sh"),
        crate::support::CONTINUOUS_ADMISSION,
        &support_path.display().to_string(),
    )?;
    if &actual_identity != expected_identity {
        bail!(
            "calibration measurement identity differs from sizing source: {} != {}",
            canonical_identity_sha256(&actual_identity)?,
            canonical_identity_sha256(expected_identity)?
        );
    }
    let paths = case_paths(run_dir)?;
    if i64::try_from(paths.len()).context("case count overflow")? != get_i64(&plan, "case_count")? {
        bail!(
            "calibration incomplete: {} / {} cases",
            paths.len(),
            get_i64(&plan, "case_count")?
        );
    }
    let planned_cases = plan
        .get("cases")
        .and_then(Value::as_array)
        .context("calibration plan cases must be an array")?;
    let mut planned = BTreeMap::new();
    for item in planned_cases {
        if planned.insert(result_key(item)?, item.clone()).is_some() {
            bail!("duplicate calibration plan identity");
        }
    }
    let mut results = BTreeMap::new();
    for path in &paths {
        let row = read_json(path)?;
        let identity = result_key(&row)?;
        if !planned.contains_key(&identity) {
            bail!(
                "calibration result outside plan: {}: {identity:?}",
                path.display()
            );
        }
        if results.insert(identity.clone(), row.clone()).is_some() {
            bail!("duplicate calibration result: {identity:?}");
        }
        if get_i64(&row, "ops_completed")? != get_i64(&row, "ops_requested")? {
            bail!("partial calibration result: {}", path.display());
        }
        let elapsed = get_f64(&row, "elapsed_s")?;
        let rate = get_f64(&row, "ops_per_s")?;
        if !elapsed.is_finite() || elapsed <= 0.0 || !rate.is_finite() || rate <= 0.0 {
            bail!("invalid calibration timing: {}", path.display());
        }
    }
    if planned.keys().ne(results.keys()) {
        bail!("missing calibration results");
    }
    let mut provenance = Map::new();
    provenance.insert("calibration_plan_sha256".to_owned(), json!(plan_sha));
    provenance.insert(
        "calibration_support_sha256".to_owned(),
        json!(sha256(&support_path)?),
    );
    provenance.insert(
        "calibration_case_set_sha256".to_owned(),
        json!(case_set_sha256(&paths)?),
    );
    provenance.insert(
        "expected_measurement_identity".to_owned(),
        expected_identity.clone(),
    );
    provenance.insert(
        "expected_measurement_identity_sha256".to_owned(),
        json!(canonical_identity_sha256(expected_identity)?),
    );
    Ok(CalibrationResults {
        plan,
        results,
        provenance,
    })
}

fn calibrated_family_ops(
    family: &Value,
    calibration_plan: &Value,
    results: &BTreeMap<ResultKey, Value>,
    target_s: f64,
) -> Result<i64> {
    let key = family_key(family)?;
    let mut candidates = Vec::new();
    for item in calibration_plan
        .get("cases")
        .and_then(Value::as_array)
        .context("calibration cases missing")?
    {
        if family_key(item)? == key {
            candidates.push(item);
        }
    }
    if candidates.is_empty() {
        bail!("family missing from calibration plan: {key:?}");
    }
    let mut max_rate = f64::NEG_INFINITY;
    for item in candidates {
        let result = results
            .get(&result_key(item)?)
            .context("missing calibration result")?;
        max_rate = max_rate.max(get_f64(result, "ops_per_s")?);
    }
    Ok(get_i64(family, "current_ops")?.max(rounded_ops(max_rate * target_s)?))
}

fn merge_object(target: &mut Map<String, Value>, source: &Map<String, Value>) {
    for (key, value) in source {
        target.insert(key.clone(), value.clone());
    }
}

fn build_final_plans(
    repo: &Path,
    audit_path: &Path,
    calibration_plan_path: &Path,
    calibration_run_dir: &Path,
    final_trials: i64,
) -> Result<(Value, Value)> {
    if final_trials < 1 {
        bail!("final_trials must be positive");
    }
    let audit = load_complete_audit(audit_path)?;
    let target_s = target_seconds(&audit)?;
    let calibration = load_calibration_results(repo, calibration_plan_path, calibration_run_dir)?;
    let audit_sha = sha256(audit_path)?;
    if calibration
        .plan
        .get("source_sizing_audit_sha256")
        .and_then(Value::as_str)
        != Some(audit_sha.as_str())
    {
        bail!("calibration plan was generated from a different sizing audit");
    }
    let mut final_cases = Vec::new();
    let mut delete_cases = Vec::new();
    let mut selected = Vec::new();
    let mut delete_families = Vec::new();
    for family in audit
        .get("families")
        .and_then(Value::as_array)
        .context("missing audit families")?
    {
        let workload = get_str(family, "workload")?;
        let clients = family
            .get("clients")
            .and_then(Value::as_array)
            .context("family clients must be array")?;
        if workload == DELETE_WORKLOAD {
            let ops = get_i64(family, "current_ops")?;
            delete_families.push(json!({
                "scenario": clone_field(family, "scenario")?,
                "engine": clone_field(family, "engine")?,
                "durability": clone_field(family, "durability")?,
                "workload": workload,
                "ops": ops,
            }));
            for trial in 1..=final_trials {
                for client in clients {
                    delete_cases.push(case_template(
                        family,
                        ops,
                        client.as_i64().context("invalid family client")?,
                        trial,
                        "growth",
                        "append",
                        0,
                    )?);
                }
            }
            continue;
        }
        let (ops, source, state, write_pattern, slots) = if FIXED_WORKLOADS.contains(&workload) {
            if get_str(family, "status")? == "undersized" {
                (
                    calibrated_family_ops(
                        family,
                        &calibration.plan,
                        &calibration.results,
                        target_s,
                    )?,
                    "fixed-calibration",
                    "growth",
                    "append",
                    0,
                )
            } else {
                (
                    get_i64(family, "current_ops")?,
                    "stock-sized",
                    "growth",
                    "append",
                    0,
                )
            }
        } else if BOUNDED_WORKLOADS.contains(&workload) {
            (
                calibrated_family_ops(family, &calibration.plan, &calibration.results, target_s)?,
                "bounded-calibration",
                "bounded",
                "update-uniform",
                DEFAULT_BOUNDED_CHURN_SLOTS,
            )
        } else {
            bail!("unclassified workload: {workload}");
        };
        selected.push(json!({
            "scenario": clone_field(family, "scenario")?,
            "engine": clone_field(family, "engine")?,
            "durability": clone_field(family, "durability")?,
            "workload": workload,
            "ops": ops,
            "source": source,
            "state_evolution": state,
            "write_pattern": write_pattern,
            "bounded_churn_slots": slots,
        }));
        for trial in 1..=final_trials {
            for client in clients {
                final_cases.push(case_template(
                    family,
                    ops,
                    client.as_i64().context("invalid family client")?,
                    trial,
                    state,
                    write_pattern,
                    slots,
                )?);
            }
        }
    }

    let mut common = Map::new();
    common.insert(
        "concurrency_probe_plan_version".to_owned(),
        json!(KV_PLAN_VERSION),
    );
    common.insert("expect_trials".to_owned(), json!(final_trials));
    common.insert(
        "source_sizing_audit_sha256".to_owned(),
        json!(sha256(audit_path)?),
    );
    common.insert(
        "source_sizing_policy_version".to_owned(),
        clone_field(&audit, "concurrency_sizing_policy_version")?,
    );
    common.insert("target_seconds".to_owned(), json!(target_s));
    merge_object(&mut common, &calibration.provenance);

    let mut steady = common.clone();
    steady.insert("kind".to_owned(), json!("steady-scaling-final"));
    steady.insert("family_count".to_owned(), json!(selected.len()));
    steady.insert("families".to_owned(), Value::Array(selected));
    steady.insert("case_count".to_owned(), json!(final_cases.len()));
    steady.insert("cases".to_owned(), Value::Array(final_cases));

    let mut delete = common;
    delete.insert("kind".to_owned(), json!("finite-delete-diagnostic"));
    delete.insert("family_count".to_owned(), json!(delete_families.len()));
    delete.insert("families".to_owned(), Value::Array(delete_families));
    delete.insert("case_count".to_owned(), json!(delete_cases.len()));
    delete.insert("cases".to_owned(), Value::Array(delete_cases));
    Ok((Value::Object(steady), Value::Object(delete)))
}

pub(crate) struct KvCalibrationArgs<'a> {
    pub(crate) sizing_audit: &'a Path,
    pub(crate) out: &'a Path,
}

pub(crate) struct KvFinalArgs<'a> {
    pub(crate) repo: &'a Path,
    pub(crate) sizing_audit: &'a Path,
    pub(crate) calibration_plan: &'a Path,
    pub(crate) calibration_run: &'a Path,
    pub(crate) out: &'a Path,
    pub(crate) delete_out: &'a Path,
    pub(crate) trials: i64,
}

pub(crate) fn run_kv_calibration(args: KvCalibrationArgs<'_>) -> Result<()> {
    let plan = build_calibration_plan(args.sizing_audit)?;
    write_json(args.out, &plan)?;
    println!(
        "{}",
        serde_json::to_string(&json!({
            "kind": clone_field(&plan, "kind")?,
            "family_count": get_i64(&plan, "family_count")?,
            "case_count": get_i64(&plan, "case_count")?,
        }))?
    );
    Ok(())
}

pub(crate) fn run_kv_final(args: KvFinalArgs<'_>) -> Result<()> {
    let (steady, delete) = build_final_plans(
        args.repo,
        args.sizing_audit,
        args.calibration_plan,
        args.calibration_run,
        args.trials,
    )?;
    write_json(args.out, &steady)?;
    write_json(args.delete_out, &delete)?;
    println!(
        "{}",
        serde_json::to_string(&json!({
            "steady_family_count": get_i64(&steady, "family_count")?,
            "steady_case_count": get_i64(&steady, "case_count")?,
            "delete_family_count": get_i64(&delete, "family_count")?,
            "delete_case_count": get_i64(&delete, "case_count")?,
        }))?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{build_calibration_plan, build_final_plans};
    use crate::planning::{
        TestDir, canonical_identity_sha256, get_i64, kv_support, repo, sha256, support_identity,
        write_json,
    };
    use anyhow::{Context, Result};
    use serde_json::{Value, json};
    use std::fs;

    fn family(workload: &str, status: &str, rate: f64, ops: i64) -> Result<Value> {
        let fixed = matches!(workload, "point-read" | "range-scan" | "read-heavy");
        Ok(json!({
            "scenario": if workload == "range-scan" { "concurrency-range" } else { "concurrency-primary" },
            "engine": "x", "durability": "sync", "workload": workload,
            "clients": [1, 8], "missing_clients": [], "current_records": 100_000,
            "current_ops": ops, "observed_client_groups": 2, "planned_client_groups": 2,
            "semantics": if fixed { "fixed-keyspace" } else { "state-changing" },
            "status": status,
            "recommendation": if fixed && status == "undersized" { "more-ops" } else { "keep-stock" },
            "suggested_ops": if fixed { ops.max(super::rounded_ops(rate * 3.0)?) } else { ops },
            "suggested_trials": 3,
            "fastest_observed_median_elapsed_s": ops as f64 / rate,
            "max_observed_median_ops_per_s": rate,
        }))
    }

    fn write_audit(path: &std::path::Path, families: Vec<Value>, missing: i64) -> Result<()> {
        let repo = repo()?;
        let support = kv_support(&repo)?;
        let identity = support_identity(
            &support,
            "kv-concurrency",
            Some(&repo),
            Some("run-kv-concurrency-plan.sh"),
            crate::support::CONTINUOUS_ADMISSION,
            "test",
        )?;
        write_json(
            path,
            &json!({
                "concurrency_sizing_policy_version": 1,
                "missing_probe_case_count": missing,
                "planned_family_count": families.len(),
                "thresholds": {"target_seconds": 3.0},
                "families": families,
                "measurement_identity": identity,
            }),
        )
    }

    #[test]
    fn incomplete_audit_refuses_plan() -> Result<()> {
        let dir = TestDir::new("kv-incomplete")?;
        let path = dir.0.join("audit.json");
        write_audit(
            &path,
            vec![family("point-read", "undersized", 1000.0, 50_000)?],
            1,
        )?;
        assert!(build_calibration_plan(&path).is_err());
        Ok(())
    }

    #[test]
    fn calibration_selects_fixed_undersized_and_bounded_only() -> Result<()> {
        let dir = TestDir::new("kv-calibration")?;
        let path = dir.0.join("audit.json");
        write_audit(
            &path,
            vec![
                family("point-read", "undersized", 100_000.0, 200_000)?,
                family("range-scan", "sized", 300.0, 1_000)?,
                family("balanced", "sized", 200_000.0, 50_000)?,
                family("delete-burst", "sized", 20_000.0, 50_000)?,
            ],
            0,
        )?;
        let plan = build_calibration_plan(&path)?;
        assert_eq!(plan["family_count"], 2);
        assert_eq!(plan["case_count"], 4);
        let point = plan["cases"]
            .as_array()
            .and_then(|items| items.iter().find(|item| item["workload"] == "point-read"))
            .context("missing point-read case")?;
        let balanced = plan["cases"]
            .as_array()
            .and_then(|items| items.iter().find(|item| item["workload"] == "balanced"))
            .context("missing balanced case")?;
        assert_eq!(point["ops"], 500_000);
        assert_eq!(point["state_evolution"], "growth");
        assert_eq!(balanced["state_evolution"], "bounded");
        assert_eq!(balanced["bounded_churn_slots"], 8192);
        Ok(())
    }

    #[test]
    fn final_plan_uses_calibration_and_splits_delete() -> Result<()> {
        let dir = TestDir::new("kv-final")?;
        let audit = dir.0.join("audit.json");
        write_audit(
            &audit,
            vec![
                family("point-read", "undersized", 100_000.0, 200_000)?,
                family("range-scan", "sized", 300.0, 1_000)?,
                family("balanced", "sized", 200_000.0, 50_000)?,
                family("delete-burst", "sized", 20_000.0, 50_000)?,
            ],
            0,
        )?;
        let calibration = build_calibration_plan(&audit)?;
        let plan_path = dir.0.join("plan.json");
        write_json(&plan_path, &calibration)?;
        let run = dir.0.join("run");
        fs::create_dir_all(run.join("cases"))?;
        let mut support = kv_support(&repo()?)?;
        support["plan_sha256"] = json!(sha256(&plan_path)?);
        fs::write(run.join("support.json"), serde_json::to_vec(&support)?)?;
        for (index, case) in calibration["cases"]
            .as_array()
            .context("missing cases")?
            .iter()
            .enumerate()
        {
            let mut row = case.clone();
            let ops = get_i64(case, "ops")?;
            row.as_object_mut().context("case object")?.remove("ops");
            row["ops_requested"] = json!(ops);
            row["ops_completed"] = json!(ops);
            let rate = if row["workload"] == "point-read" {
                150_000.0
            } else {
                250_000.0
            };
            row["ops_per_s"] = json!(rate);
            row["elapsed_s"] = json!(ops as f64 / rate);
            fs::write(
                run.join("cases").join(format!("c{index}.json")),
                serde_json::to_vec(&row)?,
            )?;
        }
        let (steady, delete) = build_final_plans(&repo()?, &audit, &plan_path, &run, 3)?;
        assert_eq!(steady["family_count"], 3);
        assert_eq!(delete["family_count"], 1);
        assert_eq!(delete["case_count"], 6);
        let cases = steady["cases"].as_array().context("steady cases")?;
        let point = cases
            .iter()
            .find(|item| item["workload"] == "point-read")
            .context("point")?;
        let balanced = cases
            .iter()
            .find(|item| item["workload"] == "balanced")
            .context("balanced")?;
        let scan = cases
            .iter()
            .find(|item| item["workload"] == "range-scan")
            .context("scan")?;
        assert_eq!(point["ops"], 500_000);
        assert_eq!(balanced["ops"], 1_000_000);
        assert_eq!(balanced["state_evolution"], "bounded");
        assert_eq!(scan["ops"], 1_000);
        Ok(())
    }

    #[test]
    fn calibration_support_must_bind_identity_and_plan() -> Result<()> {
        let dir = TestDir::new("kv-bind")?;
        let audit = dir.0.join("audit.json");
        write_audit(
            &audit,
            vec![family("balanced", "sized", 1_000.0, 50_000)?],
            0,
        )?;
        let plan = build_calibration_plan(&audit)?;
        let plan_path = dir.0.join("plan.json");
        write_json(&plan_path, &plan)?;
        let run = dir.0.join("run");
        fs::create_dir_all(run.join("cases"))?;
        let mut support = kv_support(&repo()?)?;
        support["hostname"] = json!("other-host");
        support["plan_sha256"] = json!(sha256(&plan_path)?);
        fs::write(run.join("support.json"), serde_json::to_vec(&support)?)?;
        assert!(super::load_calibration_results(&repo()?, &plan_path, &run).is_err());
        support["hostname"] = json!("test-host");
        support["plan_sha256"] = json!("bad");
        fs::write(run.join("support.json"), serde_json::to_vec(&support)?)?;
        assert!(super::load_calibration_results(&repo()?, &plan_path, &run).is_err());
        Ok(())
    }

    #[test]
    fn stored_identity_digest_is_canonical() -> Result<()> {
        let dir = TestDir::new("kv-digest")?;
        let audit = dir.0.join("audit.json");
        write_audit(
            &audit,
            vec![family("balanced", "sized", 1_000.0, 50_000)?],
            0,
        )?;
        let plan = build_calibration_plan(&audit)?;
        assert_eq!(
            plan["expected_measurement_identity_sha256"],
            json!(canonical_identity_sha256(
                &plan["expected_measurement_identity"]
            )?),
        );
        Ok(())
    }
}
