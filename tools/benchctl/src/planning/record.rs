use super::{
    READ_MATERIALIZATION, RECORD_MAX_SLOW_SECONDS, RECORD_PLAN_VERSION, TARGET_SECONDS,
    WRITE_MATERIALIZATION, canonical_identity_sha256, case_paths, case_set_sha256, clone_field,
    get_f64, get_i64, get_str, json_truthy, read_json, sha256, support_identity, write_json,
};
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use std::{collections::BTreeMap, path::Path};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct FamilyKey {
    scenario: String,
    engine: String,
    durability: String,
    workload: String,
    records: i64,
    payload_bytes: i64,
    txn_size: i64,
    state_evolution: String,
    read_materialization: String,
    write_materialization: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ClientKey {
    family: FamilyKey,
    clients: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ResultKey {
    client: ClientKey,
    trial: i64,
}

fn family_key(value: &Value) -> Result<FamilyKey> {
    Ok(FamilyKey {
        scenario: get_str(value, "scenario")?.to_owned(),
        engine: get_str(value, "engine")?.to_owned(),
        durability: get_str(value, "durability")?.to_owned(),
        workload: get_str(value, "workload")?.to_owned(),
        records: get_i64(value, "records")?,
        payload_bytes: get_i64(value, "payload_bytes")?,
        txn_size: get_i64(value, "txn_size")?,
        state_evolution: get_str(value, "state_evolution")?.to_owned(),
        read_materialization: get_str(value, "read_materialization")?.to_owned(),
        write_materialization: get_str(value, "write_materialization")?.to_owned(),
    })
}

fn client_key(value: &Value) -> Result<ClientKey> {
    Ok(ClientKey {
        family: family_key(value)?,
        clients: get_i64(value, "clients")?,
    })
}

fn result_key(value: &Value) -> Result<ResultKey> {
    Ok(ResultKey {
        client: client_key(value)?,
        trial: get_i64(value, "trial")?,
    })
}

struct CalibrationResults {
    plan: Value,
    results: BTreeMap<ResultKey, Value>,
    provenance: Map<String, Value>,
}

fn load_results(repo: &Path, plan_path: &Path, run_dir: &Path) -> Result<CalibrationResults> {
    let plan = read_json(plan_path)?;
    if get_i64(&plan, "record_concurrency_plan_version")? != RECORD_PLAN_VERSION
        || get_str(&plan, "kind")? != "semantic-calibration"
    {
        bail!("not a record semantic calibration plan");
    }
    if get_str(&plan, "read_materialization")? != READ_MATERIALIZATION {
        bail!("unexpected calibration read materialization");
    }
    if get_str(&plan, "write_materialization")? != WRITE_MATERIALIZATION {
        bail!("unexpected calibration write materialization");
    }
    let support_path = run_dir.join("support.json");
    let summary_path = run_dir.join("summary.json");
    if !support_path.is_file() || !summary_path.is_file() {
        bail!("calibration run incomplete");
    }
    let support = read_json(&support_path)?;
    let summary = read_json(&summary_path)?;
    let plan_sha = sha256(plan_path)?;
    if support.get("plan_sha256").and_then(Value::as_str) != Some(plan_sha.as_str()) {
        bail!("calibration support plan SHA mismatch");
    }
    let measurement_identity = support_identity(
        &support,
        "record-concurrency",
        Some(repo),
        Some("run-record-concurrency-plan.sh"),
        crate::support::CONTINUOUS_ADMISSION,
        &support_path.display().to_string(),
    )?;
    let paths = case_paths(run_dir)?;
    let expected = get_i64(&plan, "case_count")?;
    if i64::try_from(paths.len()).context("case count overflow")? != expected
        || get_i64(&summary, "row_count")? != expected
        || get_i64(&summary, "group_count")? != expected
        || json_truthy(summary.get("problems"))
    {
        bail!("calibration case/summary count mismatch");
    }
    let failures = run_dir.join("failures.ndjson");
    if failures.is_file() && failures.metadata()?.len() != 0 {
        bail!("calibration run has failures");
    }
    let cases = plan
        .get("cases")
        .and_then(Value::as_array)
        .context("calibration plan cases must be an array")?;
    let mut planned = BTreeMap::new();
    for item in cases {
        if planned.insert(result_key(item)?, item.clone()).is_some() {
            bail!("duplicate calibration plan identity");
        }
    }
    if i64::try_from(planned.len()).context("planned count overflow")? != expected {
        bail!("duplicate calibration plan identity");
    }
    let mut results = BTreeMap::new();
    for path in &paths {
        let row = read_json(path)?;
        if get_str(&row, "read_materialization")? != READ_MATERIALIZATION {
            bail!(
                "unexpected calibration read materialization: {}",
                path.display()
            );
        }
        if get_str(&row, "write_materialization")? != WRITE_MATERIALIZATION {
            bail!(
                "unexpected calibration write materialization: {}",
                path.display()
            );
        }
        let key = result_key(&row)?;
        if !planned.contains_key(&key) {
            bail!("result outside calibration plan: {}", path.display());
        }
        if results.insert(key.clone(), row.clone()).is_some() {
            bail!("duplicate calibration result: {key:?}");
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
        bail!("calibration results do not exactly cover plan");
    }
    let mut provenance = Map::new();
    provenance.insert("calibration_plan_sha256".to_owned(), json!(plan_sha));
    provenance.insert(
        "calibration_support_sha256".to_owned(),
        json!(sha256(&support_path)?),
    );
    provenance.insert(
        "calibration_summary_sha256".to_owned(),
        json!(sha256(&summary_path)?),
    );
    provenance.insert(
        "calibration_case_set_sha256".to_owned(),
        json!(case_set_sha256(&paths)?),
    );
    provenance.insert(
        "expected_measurement_identity".to_owned(),
        measurement_identity.clone(),
    );
    provenance.insert(
        "expected_measurement_identity_sha256".to_owned(),
        json!(canonical_identity_sha256(&measurement_identity)?),
    );
    Ok(CalibrationResults {
        plan,
        results,
        provenance,
    })
}

fn choose_ops(
    cases: &[Value],
    results: &BTreeMap<ResultKey, Value>,
) -> Result<(i64, Map<String, Value>)> {
    let current: BTreeMap<i64, ()> = cases
        .iter()
        .map(|item| Ok((get_i64(item, "ops")?, ())))
        .collect::<Result<_>>()?;
    if current.len() != 1 {
        bail!("family calibration ops mismatch");
    }
    let current_ops = *current.keys().next().context("missing calibration ops")?;
    let mut fastest = f64::NEG_INFINITY;
    let mut slowest = f64::INFINITY;
    for item in cases {
        let rate = get_f64(
            results
                .get(&result_key(item)?)
                .context("missing calibration result")?,
            "ops_per_s",
        )?;
        fastest = fastest.max(rate);
        slowest = slowest.min(rate);
    }
    let mut desired = (fastest * TARGET_SECONDS).ceil() as i64;
    let mut cap = (slowest * RECORD_MAX_SLOW_SECONDS).floor() as i64;
    let workload = get_str(&cases[0], "workload")?;
    let txn = get_i64(&cases[0], "txn_size")?;
    if txn < 1 {
        bail!("calibration txn_size must be positive");
    }
    let max_clients = cases
        .iter()
        .map(|item| get_i64(item, "clients"))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .max()
        .context("empty calibration family")?;
    let minimum = if workload == "write-burst" {
        if current_ops % txn != 0 {
            bail!("calibration write-burst ops are not transaction aligned");
        }
        desired = ((desired + txn - 1) / txn) * txn;
        cap = (cap / txn) * txn;
        max_clients * txn
    } else {
        max_clients
    };
    let selected = if cap >= minimum {
        minimum.max(desired.min(cap))
    } else {
        minimum
    };
    let mut analysis = Map::new();
    analysis.insert("calibration_ops".to_owned(), json!(current_ops));
    analysis.insert("fastest_ops_per_s".to_owned(), json!(fastest));
    analysis.insert("slowest_ops_per_s".to_owned(), json!(slowest));
    analysis.insert("desired_ops_for_target".to_owned(), json!(desired));
    analysis.insert("max_ops_for_slow_window".to_owned(), json!(cap));
    analysis.insert("minimum_ops".to_owned(), json!(minimum));
    analysis.insert(
        "projected_fastest_s".to_owned(),
        json!(selected as f64 / fastest),
    );
    analysis.insert(
        "projected_slowest_s".to_owned(),
        json!(selected as f64 / slowest),
    );
    analysis.insert("window_limited".to_owned(), json!(selected < desired));
    analysis.insert(
        "reduced_from_calibration".to_owned(),
        json!(selected < current_ops),
    );
    Ok((selected, analysis))
}

fn build_final(repo: &Path, plan_path: &Path, run_dir: &Path, trials: i64) -> Result<Value> {
    if trials < 1 {
        bail!("trials must be positive");
    }
    let calibration = load_results(repo, plan_path, run_dir)?;
    let mut families: BTreeMap<FamilyKey, Vec<Value>> = BTreeMap::new();
    for case in calibration
        .plan
        .get("cases")
        .and_then(Value::as_array)
        .context("calibration plan cases must be array")?
    {
        families
            .entry(family_key(case)?)
            .or_default()
            .push(case.clone());
    }
    let mut final_cases = Vec::new();
    let mut selected = Vec::new();
    for (family_key, cases) in families {
        let mut clients: Vec<i64> = cases
            .iter()
            .map(|item| get_i64(item, "clients"))
            .collect::<Result<_>>()?;
        clients.sort_unstable();
        if clients.windows(2).any(|window| window[0] == window[1]) {
            bail!("duplicate client group in family: {family_key:?}");
        }
        let (ops, analysis) = choose_ops(&cases, &calibration.results)?;
        let template = cases.first().context("empty calibration family")?;
        let mut family = Map::new();
        family.insert("scenario".to_owned(), clone_field(template, "scenario")?);
        family.insert("engine".to_owned(), clone_field(template, "engine")?);
        family.insert(
            "durability".to_owned(),
            clone_field(template, "durability")?,
        );
        family.insert("workload".to_owned(), clone_field(template, "workload")?);
        family.insert("records".to_owned(), json!(get_i64(template, "records")?));
        family.insert(
            "payload_bytes".to_owned(),
            json!(get_i64(template, "payload_bytes")?),
        );
        family.insert("txn_size".to_owned(), json!(get_i64(template, "txn_size")?));
        family.insert(
            "state_evolution".to_owned(),
            clone_field(template, "state_evolution")?,
        );
        family.insert(
            "read_materialization".to_owned(),
            clone_field(template, "read_materialization")?,
        );
        family.insert(
            "write_materialization".to_owned(),
            clone_field(template, "write_materialization")?,
        );
        family.insert("clients".to_owned(), json!(clients));
        family.insert("final_ops".to_owned(), json!(ops));
        for (key, value) in analysis {
            family.insert(key, value);
        }
        selected.push(Value::Object(family));
        for trial in 1..=trials {
            for base in &cases {
                let mut item = base.clone();
                item["ops"] = json!(ops);
                item["trial"] = json!(trial);
                final_cases.push(item);
            }
        }
    }
    let mut out = Map::new();
    out.insert(
        "record_concurrency_plan_version".to_owned(),
        json!(RECORD_PLAN_VERSION),
    );
    out.insert("kind".to_owned(), json!("steady-scaling-final"));
    out.insert(
        "read_materialization".to_owned(),
        json!(READ_MATERIALIZATION),
    );
    out.insert(
        "write_materialization".to_owned(),
        json!(WRITE_MATERIALIZATION),
    );
    out.insert("expect_trials".to_owned(), json!(trials));
    out.insert("target_seconds".to_owned(), json!(TARGET_SECONDS));
    out.insert(
        "max_slow_seconds".to_owned(),
        json!(RECORD_MAX_SLOW_SECONDS),
    );
    out.insert(
        "source_calibration_plan_sha256".to_owned(),
        json!(sha256(plan_path)?),
    );
    for (key, value) in calibration.provenance {
        out.insert(key, value);
    }
    out.insert("family_count".to_owned(), json!(selected.len()));
    out.insert("families".to_owned(), Value::Array(selected));
    out.insert("case_count".to_owned(), json!(final_cases.len()));
    out.insert("cases".to_owned(), Value::Array(final_cases));
    Ok(Value::Object(out))
}

pub(crate) struct RecordFinalArgs<'a> {
    pub(crate) repo: &'a Path,
    pub(crate) calibration_plan: &'a Path,
    pub(crate) calibration_run: &'a Path,
    pub(crate) out: &'a Path,
    pub(crate) trials: i64,
}

pub(crate) fn run_record_final(args: RecordFinalArgs<'_>) -> Result<()> {
    let plan = build_final(
        args.repo,
        args.calibration_plan,
        args.calibration_run,
        args.trials,
    )?;
    write_json(args.out, &plan)?;
    let window_limited = plan
        .get("families")
        .and_then(Value::as_array)
        .context("missing output families")?
        .iter()
        .filter(|family| family.get("window_limited").and_then(Value::as_bool) == Some(true))
        .count();
    println!(
        "{}",
        serde_json::to_string(&json!({
            "family_count": get_i64(&plan, "family_count")?,
            "case_count": get_i64(&plan, "case_count")?,
            "window_limited_families": window_limited,
        }))?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::build_final;
    use crate::planning::{
        READ_MATERIALIZATION, TestDir, WRITE_MATERIALIZATION, record_support, repo, sha256,
        write_json,
    };
    use anyhow::{Context, Result};
    use serde_json::{Value, json};
    use std::fs;

    fn case(workload: &str, state: &str, txn: i64, clients: i64, ops: i64) -> Value {
        json!({
            "scenario": "record-concurrency-core",
            "engine": "sqlite",
            "durability": "sync",
            "workload": workload,
            "clients": clients,
            "records": 10_000,
            "ops": ops,
            "payload_bytes": 512,
            "txn_size": txn,
            "trial": 1,
            "state_evolution": state,
            "read_materialization": READ_MATERIALIZATION,
            "write_materialization": WRITE_MATERIALIZATION,
        })
    }

    fn plan(cases: Vec<Value>) -> Value {
        json!({
            "record_concurrency_plan_version": 2,
            "read_materialization": READ_MATERIALIZATION,
            "write_materialization": WRITE_MATERIALIZATION,
            "kind": "semantic-calibration",
            "expect_trials": 1,
            "case_count": cases.len(),
            "cases": cases,
        })
    }

    fn write_run(
        dir: &TestDir,
        source_plan: &Value,
        cases: &[Value],
        rates: &[f64],
    ) -> Result<(std::path::PathBuf, std::path::PathBuf)> {
        let plan_path = dir.0.join("plan.json");
        write_json(&plan_path, source_plan)?;
        let run = dir.0.join("run");
        fs::create_dir_all(run.join("cases"))?;
        for (index, (item, rate)) in cases.iter().zip(rates).enumerate() {
            let mut row = item.clone();
            let ops = row["ops"].as_i64().context("missing ops")?;
            row.as_object_mut().context("case object")?.remove("ops");
            row["ops_requested"] = json!(ops);
            row["ops_completed"] = json!(ops);
            row["elapsed_s"] = json!(ops as f64 / rate);
            row["ops_per_s"] = json!(rate);
            fs::write(
                run.join("cases").join(format!("{index}.json")),
                serde_json::to_vec(&row)?,
            )?;
        }
        let mut support = record_support(&repo()?)?;
        support["plan_sha256"] = json!(sha256(&plan_path)?);
        fs::write(run.join("support.json"), serde_json::to_vec(&support)?)?;
        fs::write(
            run.join("summary.json"),
            serde_json::to_vec(&json!({
                "row_count": cases.len(), "group_count": cases.len(), "problems": []
            }))?,
        )?;
        Ok((plan_path, run))
    }

    #[test]
    fn final_plan_is_three_trial_and_transaction_aligned() -> Result<()> {
        let dir = TestDir::new("record-final")?;
        let mut cases = Vec::new();
        for (workload, state, txn) in [
            ("point-read", "growth", 100),
            ("write-burst", "bounded", 100),
        ] {
            for clients in [1, 8] {
                cases.push(case(workload, state, txn, clients, 1000));
            }
        }
        let rates: Vec<f64> = cases
            .iter()
            .map(|item| if item["clients"] == 1 { 1000.0 } else { 100.0 })
            .collect();
        let source = plan(cases.clone());
        let (plan_path, run) = write_run(&dir, &source, &cases, &rates)?;
        let out = build_final(&repo()?, &plan_path, &run, 3)?;
        assert_eq!(out["record_concurrency_plan_version"], 2);
        assert_eq!(out["family_count"], 2);
        assert_eq!(out["case_count"], 12);
        assert_eq!(out["expect_trials"], 3);
        for family in out["families"].as_array().context("families")? {
            assert!(
                family["projected_slowest_s"]
                    .as_f64()
                    .is_some_and(|value| value <= 15.0)
            );
            if family["workload"] == "write-burst" {
                assert_eq!(family["final_ops"].as_i64().context("final ops")? % 100, 0);
            }
        }
        let point = out["families"]
            .as_array()
            .and_then(|items| items.iter().find(|item| item["workload"] == "point-read"))
            .context("missing point family")?;
        assert_eq!(point["final_ops"], 1500);
        assert_eq!(point["reduced_from_calibration"], false);
        Ok(())
    }

    #[test]
    fn final_plan_can_reduce_overlong_calibration() -> Result<()> {
        let dir = TestDir::new("record-reduce")?;
        let cases = vec![
            case("point-read", "growth", 100, 1, 10_000),
            case("point-read", "growth", 100, 8, 10_000),
        ];
        let source = plan(cases.clone());
        let (plan_path, run) = write_run(&dir, &source, &cases, &[1000.0, 500.0])?;
        let out = build_final(&repo()?, &plan_path, &run, 3)?;
        let family = &out["families"][0];
        assert_eq!(family["final_ops"], 3000);
        assert_eq!(family["reduced_from_calibration"], true);
        assert_eq!(family["projected_fastest_s"], 3.0);
        assert_eq!(family["projected_slowest_s"], 6.0);
        Ok(())
    }

    #[test]
    fn v1_and_wrong_materialization_are_rejected() -> Result<()> {
        let dir = TestDir::new("record-reject")?;
        let cases = vec![case("point-read", "growth", 100, 1, 1000)];
        let mut source = plan(cases.clone());
        source["record_concurrency_plan_version"] = json!(1);
        let (plan_path, run) = write_run(&dir, &source, &cases, &[1000.0])?;
        assert!(build_final(&repo()?, &plan_path, &run, 3).is_err());

        source["record_concurrency_plan_version"] = json!(2);
        write_json(&plan_path, &source)?;
        let mut support = record_support(&repo()?)?;
        support["plan_sha256"] = json!(sha256(&plan_path)?);
        fs::write(run.join("support.json"), serde_json::to_vec(&support)?)?;
        let result_path = run.join("cases/0.json");
        let mut result: Value = serde_json::from_str(&fs::read_to_string(&result_path)?)?;
        result["read_materialization"] = json!("legacy-read-v0");
        fs::write(&result_path, serde_json::to_vec(&result)?)?;
        assert!(build_final(&repo()?, &plan_path, &run, 3).is_err());
        Ok(())
    }
}
