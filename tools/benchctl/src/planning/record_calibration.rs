use super::{
    READ_MATERIALIZATION, RECORD_PLAN_VERSION, WRITE_MATERIALIZATION, get_i64, get_str, read_json,
    write_json,
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    process::Command,
};

const ENGINES: [&str; 4] = ["surrealdb", "turso", "sqlite", "surrealdb-rocksdb"];
const CORE_CLIENTS: [i64; 4] = [1, 2, 4, 8];
const SIDE_CLIENTS: [i64; 3] = [1, 4, 8];
const RECORDS: i64 = 10_000;
const PAYLOAD: i64 = 512;
const TXN: i64 = 100;
const DEFAULT_OPS: i64 = 24_000;
const STRESS_PAYLOAD: i64 = 4_096;
const HOT_RECORDS: i64 = 64;
const HOT_DEFAULT_OPS: i64 = 24_000;
const TX1_DEFAULT_OPS: i64 = 24_000;
const TX1000_DEFAULT_OPS: i64 = 24_000;
const BOUNDED_WORKLOADS: [&str; 2] = ["tiny-txn", "write-burst"];
const ALLOWED_DURABILITY: [&str; 2] = ["sync", "relaxed"];
const ALLOWED_WORKLOADS: [&str; 5] = [
    "point-read",
    "indexed-read",
    "read-heavy",
    "tiny-txn",
    "write-burst",
];
const ALLOWED_STATE: [&str; 2] = ["growth", "bounded"];
const REQUIRED: [&str; 13] = [
    "scenario",
    "engine",
    "durability",
    "workload",
    "clients",
    "records",
    "ops",
    "payload_bytes",
    "txn_size",
    "trial",
    "state_evolution",
    "read_materialization",
    "write_materialization",
];

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct FamilyKey {
    scenario: String,
    engine: String,
    durability: String,
    workload: String,
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
struct CaseKey {
    client: ClientKey,
    trial: i64,
}

fn family_key(item: &Value) -> Result<FamilyKey> {
    Ok(FamilyKey {
        scenario: get_str(item, "scenario")?.to_owned(),
        engine: get_str(item, "engine")?.to_owned(),
        durability: get_str(item, "durability")?.to_owned(),
        workload: get_str(item, "workload")?.to_owned(),
        payload_bytes: get_i64(item, "payload_bytes")?,
        txn_size: get_i64(item, "txn_size")?,
        state_evolution: get_str(item, "state_evolution")?.to_owned(),
        read_materialization: get_str(item, "read_materialization")?.to_owned(),
        write_materialization: get_str(item, "write_materialization")?.to_owned(),
    })
}

fn client_key(item: &Value) -> Result<ClientKey> {
    Ok(ClientKey {
        family: family_key(item)?,
        clients: get_i64(item, "clients")?,
    })
}

fn case_key(item: &Value) -> Result<CaseKey> {
    Ok(CaseKey {
        client: client_key(item)?,
        trial: get_i64(item, "trial")?,
    })
}

fn policy_ops(repo: &Path, scenario: &str, workload: &str, default_ops: i64) -> Result<i64> {
    let policy = repo.join("scripts/concurrency-matrix-policy.sh");
    let output = Command::new("bash")
        .arg("-lc")
        .arg("source \"$1\"; record_concurrency_ops quick \"$2\" \"$3\" \"$4\"")
        .arg("_")
        .arg(&policy)
        .arg(scenario)
        .arg(workload)
        .arg(default_ops.to_string())
        .current_dir(repo)
        .output()
        .with_context(|| {
            format!(
                "failed to invoke record concurrency policy {}",
                policy.display()
            )
        })?;
    if !output.status.success() {
        bail!(
            "record_concurrency_ops failed for {scenario}/{workload}: rc={:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let raw =
        String::from_utf8(output.stdout).context("record_concurrency_ops output was not UTF-8")?;
    let value = raw
        .trim()
        .parse::<i64>()
        .with_context(|| format!("invalid policy op count: {raw:?}"))?;
    if value <= 0 {
        bail!("non-positive policy op count: {value}");
    }
    Ok(value)
}

struct RecordCase<'a> {
    scenario: &'a str,
    engine: &'a str,
    durability: &'a str,
    workload: &'a str,
    clients: i64,
    records: i64,
    default_ops: i64,
    payload_bytes: i64,
    txn_size: i64,
}

fn add_case(cases: &mut Vec<Value>, repo: &Path, case: RecordCase<'_>) -> Result<()> {
    let RecordCase {
        scenario,
        engine,
        durability,
        workload,
        clients,
        records,
        default_ops,
        payload_bytes,
        txn_size,
    } = case;
    let ops = policy_ops(repo, scenario, workload, default_ops)?;
    let state = if BOUNDED_WORKLOADS.contains(&workload) {
        "bounded"
    } else {
        "growth"
    };
    cases.push(json!({
        "scenario": scenario,
        "engine": engine,
        "durability": durability,
        "workload": workload,
        "read_materialization": READ_MATERIALIZATION,
        "write_materialization": WRITE_MATERIALIZATION,
        "clients": clients,
        "records": records,
        "ops": ops,
        "payload_bytes": payload_bytes,
        "txn_size": txn_size,
        "trial": 1,
        "state_evolution": state,
    }));
    Ok(())
}

fn build_plan(repo: &Path) -> Result<Value> {
    let mut cases = Vec::new();
    for engine in ENGINES {
        for clients in CORE_CLIENTS {
            for workload in [
                "point-read",
                "indexed-read",
                "read-heavy",
                "tiny-txn",
                "write-burst",
            ] {
                add_case(
                    &mut cases,
                    repo,
                    RecordCase {
                        scenario: "record-concurrency-core",
                        engine: engine,
                        durability: "sync",
                        workload: workload,
                        clients: clients,
                        records: RECORDS,
                        default_ops: DEFAULT_OPS,
                        payload_bytes: PAYLOAD,
                        txn_size: TXN,
                    },
                )?;
            }
        }
        for clients in SIDE_CLIENTS {
            for workload in ["read-heavy", "write-burst"] {
                add_case(
                    &mut cases,
                    repo,
                    RecordCase {
                        scenario: "record-concurrency-relaxed",
                        engine: engine,
                        durability: "relaxed",
                        workload: workload,
                        clients: clients,
                        records: RECORDS,
                        default_ops: DEFAULT_OPS,
                        payload_bytes: PAYLOAD,
                        txn_size: TXN,
                    },
                )?;
            }
        }
        for clients in SIDE_CLIENTS {
            for workload in ["read-heavy", "write-burst"] {
                add_case(
                    &mut cases,
                    repo,
                    RecordCase {
                        scenario: "record-concurrency-large-payload",
                        engine: engine,
                        durability: "sync",
                        workload: workload,
                        clients: clients,
                        records: RECORDS,
                        default_ops: DEFAULT_OPS,
                        payload_bytes: STRESS_PAYLOAD,
                        txn_size: TXN,
                    },
                )?;
            }
        }
        for clients in SIDE_CLIENTS {
            add_case(
                &mut cases,
                repo,
                RecordCase {
                    scenario: "record-concurrency-hotset",
                    engine: engine,
                    durability: "sync",
                    workload: "read-heavy",
                    clients: clients,
                    records: HOT_RECORDS,
                    default_ops: HOT_DEFAULT_OPS,
                    payload_bytes: PAYLOAD,
                    txn_size: TXN,
                },
            )?;
        }
        for clients in SIDE_CLIENTS {
            add_case(
                &mut cases,
                repo,
                RecordCase {
                    scenario: "record-concurrency-txn-1",
                    engine: engine,
                    durability: "sync",
                    workload: "write-burst",
                    clients: clients,
                    records: RECORDS,
                    default_ops: TX1_DEFAULT_OPS,
                    payload_bytes: PAYLOAD,
                    txn_size: 1,
                },
            )?;
            add_case(
                &mut cases,
                repo,
                RecordCase {
                    scenario: "record-concurrency-txn-1000",
                    engine: engine,
                    durability: "sync",
                    workload: "write-burst",
                    clients: clients,
                    records: RECORDS,
                    default_ops: TX1000_DEFAULT_OPS,
                    payload_bytes: PAYLOAD,
                    txn_size: 1000,
                },
            )?;
        }
    }
    Ok(json!({
        "record_concurrency_plan_version": RECORD_PLAN_VERSION,
        "read_materialization": READ_MATERIALIZATION,
        "write_materialization": WRITE_MATERIALIZATION,
        "kind": "semantic-calibration",
        "expect_trials": 1,
        "case_count": cases.len(),
        "profile": "quick",
        "cases": cases,
    }))
}

fn validate_delimiters(values: &[String], index: usize) -> Result<()> {
    if values
        .iter()
        .any(|value| value.contains('|') || value.contains('\n'))
    {
        bail!("invalid delimiter in case {index}");
    }
    Ok(())
}

fn materialize(plan: &Value) -> Result<(Vec<Vec<String>>, Value)> {
    let version = plan
        .get("record_concurrency_plan_version")
        .and_then(super::value_i64)
        .unwrap_or(-1);
    if version != RECORD_PLAN_VERSION {
        bail!("unsupported record concurrency plan version: {version}");
    }
    if plan.get("read_materialization").and_then(Value::as_str) != Some(READ_MATERIALIZATION) {
        bail!(
            "invalid plan read_materialization: {:?}",
            plan.get("read_materialization")
        );
    }
    if plan.get("write_materialization").and_then(Value::as_str) != Some(WRITE_MATERIALIZATION) {
        bail!(
            "invalid plan write_materialization: {:?}",
            plan.get("write_materialization")
        );
    }
    let cases = plan
        .get("cases")
        .and_then(Value::as_array)
        .context("plan case_count mismatch")?;
    if i64::try_from(cases.len()).context("plan case count overflow")?
        != plan
            .get("case_count")
            .and_then(super::value_i64)
            .unwrap_or(-1)
    {
        bail!("plan case_count mismatch");
    }
    let expect_trials = plan
        .get("expect_trials")
        .and_then(super::value_i64)
        .unwrap_or(0);
    if expect_trials < 1 {
        bail!("expect_trials must be positive");
    }

    let mut seen = BTreeSet::new();
    let mut families: BTreeMap<FamilyKey, (i64, i64)> = BTreeMap::new();
    let mut trials_by_client: BTreeMap<ClientKey, BTreeSet<i64>> = BTreeMap::new();
    let mut rows = Vec::new();

    for (index, item) in cases.iter().enumerate() {
        let missing: Vec<&str> = REQUIRED
            .iter()
            .copied()
            .filter(|field| item.get(*field).is_none())
            .collect();
        if !missing.is_empty() {
            bail!("plan case {index} missing {missing:?}");
        }
        let engine = get_str(item, "engine")?;
        if !ENGINES.contains(&engine) {
            bail!("invalid engine in case {index}: {engine}");
        }
        let durability = get_str(item, "durability")?;
        if !ALLOWED_DURABILITY.contains(&durability) {
            bail!("invalid durability in case {index}: {durability}");
        }
        let workload = get_str(item, "workload")?;
        if !ALLOWED_WORKLOADS.contains(&workload) {
            bail!("invalid workload in case {index}: {workload}");
        }
        let state = get_str(item, "state_evolution")?;
        if !ALLOWED_STATE.contains(&state) {
            bail!("invalid state_evolution in case {index}: {state}");
        }
        if get_str(item, "read_materialization")? != READ_MATERIALIZATION {
            bail!(
                "invalid read_materialization in case {index}: {:?}",
                item.get("read_materialization")
            );
        }
        if get_str(item, "write_materialization")? != WRITE_MATERIALIZATION {
            bail!(
                "invalid write_materialization in case {index}: {:?}",
                item.get("write_materialization")
            );
        }

        let clients = get_i64(item, "clients")?;
        let records = get_i64(item, "records")?;
        let ops = get_i64(item, "ops")?;
        let payload = get_i64(item, "payload_bytes")?;
        let txn = get_i64(item, "txn_size")?;
        let trial = get_i64(item, "trial")?;
        if [clients, records, ops, payload, txn, trial]
            .into_iter()
            .min()
            .is_some_and(|value| value < 1)
        {
            bail!("non-positive numeric field in case {index}");
        }
        if clients > ops {
            bail!("clients > ops in case {index}");
        }
        if state == "bounded" && !BOUNDED_WORKLOADS.contains(&workload) {
            bail!("bounded state unsupported for {workload} in case {index}");
        }
        if workload == "write-burst" {
            if ops % txn != 0 {
                bail!("write-burst ops must be divisible by txn_size in case {index}");
            }
            if ops < clients * txn {
                bail!("write-burst must provide a whole txn per client in case {index}");
            }
            if state == "bounded" && txn > records {
                bail!("bounded write-burst txn_size > records in case {index}");
            }
        }

        let identity = case_key(item)?;
        if !seen.insert(identity.clone()) {
            bail!("duplicate case identity: {identity:?}");
        }
        trials_by_client
            .entry(identity.client.clone())
            .or_default()
            .insert(trial);
        let fk = identity.client.family;
        let prior = families.entry(fk.clone()).or_insert((records, ops));
        if *prior != (records, ops) {
            bail!("family total-work mismatch: {fk:?}: {prior:?} != ({records}, {ops})");
        }

        let values = vec![
            get_str(item, "scenario")?.to_owned(),
            engine.to_owned(),
            durability.to_owned(),
            workload.to_owned(),
            clients.to_string(),
            records.to_string(),
            ops.to_string(),
            payload.to_string(),
            txn.to_string(),
            trial.to_string(),
            state.to_owned(),
            READ_MATERIALIZATION.to_owned(),
            WRITE_MATERIALIZATION.to_owned(),
        ];
        validate_delimiters(&values, index)?;
        rows.push(values);
    }

    let expected_trials: BTreeSet<i64> = (1..=expect_trials).collect();
    for (key, trials) in trials_by_client {
        if trials != expected_trials {
            bail!(
                "client group {key:?} has trials {:?}, expected {:?}",
                trials.iter().copied().collect::<Vec<_>>(),
                expected_trials.iter().copied().collect::<Vec<_>>()
            );
        }
    }
    if !rows.is_empty()
        && i64::try_from(rows.len()).context("row count overflow")? % expect_trials != 0
    {
        bail!("case count is not divisible by expect_trials");
    }
    Ok((
        rows,
        json!({
            "plan_version": version,
            "expect_trials": expect_trials,
            "case_count": cases.len(),
            "kind": plan.get("kind").and_then(Value::as_str).unwrap_or("record-concurrency-plan"),
            "read_materialization": READ_MATERIALIZATION,
            "write_materialization": WRITE_MATERIALIZATION,
        }),
    ))
}

pub(crate) struct RecordCalibrationArgs<'a> {
    pub(crate) repo: &'a Path,
    pub(crate) out: &'a Path,
}

pub(crate) fn run_calibration(args: RecordCalibrationArgs<'_>) -> Result<()> {
    let plan = build_plan(args.repo)?;
    if get_i64(&plan, "case_count")? != 164 {
        bail!(
            "unexpected record calibration cardinality: {} != 164",
            get_i64(&plan, "case_count")?
        );
    }
    write_json(args.out, &plan)?;
    println!(
        "{}",
        serde_json::to_string(&json!({
            "case_count": get_i64(&plan, "case_count")?,
            "expect_trials": 1,
        }))?
    );
    Ok(())
}

pub(crate) struct MaterializeArgs<'a> {
    pub(crate) plan: &'a Path,
    pub(crate) tsv_out: Option<&'a Path>,
    pub(crate) meta_out: Option<&'a Path>,
}

pub(crate) fn run_materialize(args: MaterializeArgs<'_>) -> Result<()> {
    let plan = read_json(args.plan)?;
    let (rows, meta) = materialize(&plan)?;
    if let Some(path) = args.tsv_out {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut text = String::new();
        for row in rows {
            text.push_str(&row.join("|"));
            text.push('\n');
        }
        fs::write(path, text)?;
    }
    if let Some(path) = args.meta_out {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, serde_json::to_string(&meta)? + "\n")?;
    }
    if args.tsv_out.is_none() && args.meta_out.is_none() {
        println!("{}", serde_json::to_string(&meta)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{READ_MATERIALIZATION, WRITE_MATERIALIZATION, build_plan, materialize};
    use crate::planning::repo;
    use anyhow::{Context, Result};
    use serde_json::{Value, json};
    use std::fs;

    #[test]
    fn calibration_cardinality_and_semantics() -> Result<()> {
        let plan = build_plan(&repo()?)?;
        assert_eq!(plan["record_concurrency_plan_version"], 2);
        assert_eq!(plan["read_materialization"], READ_MATERIALIZATION);
        assert_eq!(plan["write_materialization"], WRITE_MATERIALIZATION);
        assert_eq!(plan["case_count"], 164);
        assert_eq!(plan["expect_trials"], 1);
        let cases = plan["cases"].as_array().context("cases")?;
        assert!(
            cases
                .iter()
                .all(|case| case["read_materialization"] == READ_MATERIALIZATION)
        );
        assert!(
            cases
                .iter()
                .all(|case| case["write_materialization"] == WRITE_MATERIALIZATION)
        );
        assert!(
            cases
                .iter()
                .filter(|case| case["state_evolution"] == "bounded")
                .all(|case| matches!(case["workload"].as_str(), Some("tiny-txn" | "write-burst")))
        );
        assert!(
            cases
                .iter()
                .filter(|case| matches!(
                    case["workload"].as_str(),
                    Some("tiny-txn" | "write-burst")
                ))
                .all(|case| case["state_evolution"] == "bounded")
        );
        let (rows, meta) = materialize(&plan)?;
        assert_eq!(rows.len(), 164);
        assert_eq!(meta["expect_trials"], 1);
        assert!(
            rows.iter()
                .all(|row| row[row.len() - 2] == READ_MATERIALIZATION)
        );
        assert!(
            rows.iter()
                .all(|row| row[row.len() - 1] == WRITE_MATERIALIZATION)
        );
        Ok(())
    }

    #[test]
    fn policy_budgets_match_expected_quick_caps() -> Result<()> {
        let plan = build_plan(&repo()?)?;
        let cases = plan["cases"].as_array().context("cases")?;
        let find = |scenario: &str, workload: &str, txn: i64| -> Result<&Value> {
            cases
                .iter()
                .find(|case| {
                    case["scenario"] == scenario
                        && case["workload"] == workload
                        && case["txn_size"] == txn
                })
                .context("missing expected case")
        };
        assert_eq!(
            find("record-concurrency-core", "point-read", 100)?["ops"],
            6000
        );
        assert_eq!(
            find("record-concurrency-core", "tiny-txn", 100)?["ops"],
            2000
        );
        assert_eq!(
            find("record-concurrency-txn-1", "write-burst", 1)?["ops"],
            1400
        );
        assert_eq!(
            find("record-concurrency-txn-1000", "write-burst", 1000)?["ops"],
            8000
        );
        Ok(())
    }

    fn multitrial_plan() -> Value {
        let mut cases = Vec::new();
        for clients in [1, 2, 4, 8] {
            for trial in [1, 2, 3] {
                cases.push(json!({
                    "scenario": "record-concurrency-core",
                    "engine": "sqlite",
                    "durability": "sync",
                    "workload": "write-burst",
                    "read_materialization": READ_MATERIALIZATION,
                    "write_materialization": WRITE_MATERIALIZATION,
                    "clients": clients,
                    "records": 10000,
                    "ops": 12000,
                    "payload_bytes": 512,
                    "txn_size": 100,
                    "trial": trial,
                    "state_evolution": "bounded",
                }));
            }
        }
        json!({
            "record_concurrency_plan_version": 2,
            "read_materialization": READ_MATERIALIZATION,
            "write_materialization": WRITE_MATERIALIZATION,
            "kind": "steady-final",
            "expect_trials": 3,
            "case_count": cases.len(),
            "cases": cases,
        })
    }

    #[test]
    fn final_multitrial_family_validates() -> Result<()> {
        let plan = multitrial_plan();
        let (rows, meta) = materialize(&plan)?;
        assert_eq!(rows.len(), 12);
        assert_eq!(meta["expect_trials"], 3);
        Ok(())
    }

    #[test]
    fn invalid_versions_and_materialization_are_rejected() -> Result<()> {
        let mut plan = build_plan(&repo()?)?;
        plan["record_concurrency_plan_version"] = json!(1);
        assert!(materialize(&plan).is_err());
        let mut plan = build_plan(&repo()?)?;
        plan["read_materialization"] = json!("legacy-read-v0");
        assert!(materialize(&plan).is_err());
        let mut plan = build_plan(&repo()?)?;
        plan["cases"][0]["write_materialization"] = json!("legacy-return-v0");
        assert!(materialize(&plan).is_err());
        Ok(())
    }

    #[test]
    fn bounded_nonwrite_and_bad_trials_are_rejected() -> Result<()> {
        let mut plan = build_plan(&repo()?)?;
        plan["cases"][0]["state_evolution"] = json!("bounded");
        assert!(materialize(&plan).is_err());
        let mut plan = multitrial_plan();
        plan["cases"].as_array_mut().context("cases")?.pop();
        plan["case_count"] = json!(11);
        assert!(materialize(&plan).is_err());
        Ok(())
    }

    #[test]
    fn runner_uses_native_materializer_and_pinned_hashes() -> Result<()> {
        let runner = fs::read_to_string(repo()?.join("scripts/run-record-concurrency-plan.sh"))?;
        assert!(!runner.contains("actual=$(sha256sum \"$BIN\""));
        assert!(!runner.contains("actual=$(sha256sum \"$ROCKS_BIN\""));
        assert!(runner.contains("BIN_SHA=\"$BENCH_BIN_SHA256\""));
        assert!(runner.contains("ROCKS_BIN_SHA=\"$ROCKS_BENCH_BIN_SHA256\""));
        assert!(runner.contains("record-concurrency-plan-materialize"));
        Ok(())
    }
}
