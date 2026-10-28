use crate::json_value::py_string;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

const BASE_REQUIRED: [&str; 8] = [
    "scenario",
    "engine",
    "durability",
    "workload",
    "clients",
    "records",
    "ops",
    "trial",
];
const STATE_FIELDS: [&str; 3] = ["state_evolution", "write_pattern", "bounded_churn_slots"];

#[derive(Debug)]
pub(crate) struct KvMaterializeArgs<'a> {
    pub(crate) plan: &'a Path,
    pub(crate) tsv_out: &'a Path,
    pub(crate) meta_out: &'a Path,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct FamilyKey {
    scenario: String,
    engine: String,
    durability: String,
    workload: String,
    state: String,
    write: String,
    slots: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ClientKey {
    family: FamilyKey,
    clients: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct IdentityKey {
    client: ClientKey,
    trial: i64,
}

fn py_int(value: &Value) -> Option<i64> {
    match value {
        Value::Bool(value) => Some(i64::from(*value)),
        Value::Number(number) => {
            if let Some(value) = number.as_i64() {
                Some(value)
            } else if let Some(value) = number.as_u64() {
                i64::try_from(value).ok()
            } else {
                let value = number.as_f64()?;
                if value.is_finite() && value >= i64::MIN as f64 && value <= i64::MAX as f64 {
                    Some(value.trunc() as i64)
                } else {
                    None
                }
            }
        }
        Value::String(value) => value.trim().parse::<i64>().ok(),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

fn required<'a>(item: &'a Value, key: &str, index: usize) -> Result<&'a Value> {
    item.get(key)
        .with_context(|| format!("plan case {index} missing ['{key}']"))
}

fn int_field(item: &Value, key: &str, index: usize) -> Result<i64> {
    let raw = required(item, key, index)?;
    py_int(raw).with_context(|| format!("invalid integer in case {index}: {key}={raw}"))
}

fn string_field(item: &Value, key: &str, index: usize) -> Result<String> {
    Ok(py_string(required(item, key, index)?))
}

fn missing_fields(item: &Value, fields: &[&str]) -> Vec<String> {
    fields
        .iter()
        .filter(|key| item.get(**key).is_none())
        .map(|key| (*key).to_owned())
        .collect()
}

fn python_list_repr(values: &[String]) -> String {
    let body = values
        .iter()
        .map(|value| format!("'{value}'"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{body}]")
}

fn materialize(plan: &Value) -> Result<(Vec<Vec<String>>, Value)> {
    let version = plan
        .get("concurrency_probe_plan_version")
        .and_then(py_int)
        .unwrap_or(-1);
    if !matches!(version, 1 | 2) {
        bail!("unsupported plan version: {version}");
    }
    let cases = plan
        .get("cases")
        .and_then(Value::as_array)
        .context("plan case_count mismatch")?;
    let case_count = plan.get("case_count").and_then(py_int).unwrap_or(-1);
    if i64::try_from(cases.len()).ok() != Some(case_count) {
        bail!("plan case_count mismatch");
    }
    let expect_trials = plan.get("expect_trials").and_then(py_int).unwrap_or(1);
    if expect_trials < 1 {
        bail!("expect_trials must be positive");
    }

    let mut seen = BTreeSet::new();
    let mut families = BTreeMap::<FamilyKey, (i64, i64)>::new();
    let mut client_trials = BTreeMap::<ClientKey, BTreeSet<i64>>::new();
    let mut rows = Vec::with_capacity(cases.len());

    for (index, item) in cases.iter().enumerate() {
        let missing = missing_fields(item, &BASE_REQUIRED);
        if !missing.is_empty() {
            bail!("plan case {index} missing {}", python_list_repr(&missing));
        }

        let state = item
            .get("state_evolution")
            .map_or_else(|| "growth".to_owned(), py_string);
        let write = item
            .get("write_pattern")
            .map_or_else(|| "append".to_owned(), py_string);
        let slots = item
            .get("bounded_churn_slots")
            .and_then(py_int)
            .unwrap_or(0);

        if version == 2 {
            let missing = missing_fields(item, &STATE_FIELDS);
            if !missing.is_empty() {
                bail!(
                    "v2 plan case {index} missing {}",
                    python_list_repr(&missing)
                );
            }
        }
        if !matches!(state.as_str(), "growth" | "bounded") {
            bail!("invalid state_evolution in case {index}: {state}");
        }
        if !matches!(write.as_str(), "append" | "update-uniform" | "update-hot") {
            bail!("invalid write_pattern in case {index}: {write}");
        }
        if slots < 0 {
            bail!("negative bounded_churn_slots in case {index}");
        }
        if state == "growth" && slots != 0 {
            bail!("growth case {index} must use bounded_churn_slots=0");
        }

        let clients = int_field(item, "clients", index)?;
        let records = int_field(item, "records", index)?;
        let ops = int_field(item, "ops", index)?;
        let trial = int_field(item, "trial", index)?;
        if clients.min(records).min(ops).min(trial) < 1 {
            bail!("non-positive numeric field in case {index}");
        }
        if clients > ops {
            bail!("clients > ops in case {index}");
        }
        let workload = string_field(item, "workload", index)?;
        if workload == "delete-burst" && ops > records {
            bail!("delete-burst ops > records in case {index}");
        }
        if state == "bounded" {
            if workload == "delete-burst" {
                bail!("bounded delete-burst unsupported in case {index}");
            }
            if matches!(workload.as_str(), "tiny-txn" | "write-burst") && write == "append" {
                bail!("bounded {workload} requires update write pattern in case {index}");
            }
            if matches!(workload.as_str(), "balanced" | "churn") {
                if write != "update-uniform" {
                    bail!("bounded {workload} requires canonical update-uniform in case {index}");
                }
                if slots < clients * 300 {
                    bail!("bounded_churn_slots too small for client count in case {index}");
                }
            }
        }

        let family = FamilyKey {
            scenario: string_field(item, "scenario", index)?,
            engine: string_field(item, "engine", index)?,
            durability: string_field(item, "durability", index)?,
            workload,
            state: state.clone(),
            write: write.clone(),
            slots,
        };
        let client = ClientKey {
            family: family.clone(),
            clients,
        };
        let identity = IdentityKey {
            client: client.clone(),
            trial,
        };
        if !seen.insert(identity) {
            bail!("duplicate case identity");
        }
        client_trials.entry(client).or_default().insert(trial);
        if let Some(previous) = families.get(&family) {
            if *previous != (records, ops) {
                bail!(
                    "family total-work mismatch: {:?}: {:?} != {:?}",
                    family,
                    previous,
                    (records, ops)
                );
            }
        } else {
            families.insert(family, (records, ops));
        }

        let mut values = Vec::with_capacity(BASE_REQUIRED.len() + 3);
        for key in BASE_REQUIRED {
            values.push(py_string(required(item, key, index)?));
        }
        values.push(state);
        values.push(write);
        values.push(slots.to_string());
        if values
            .iter()
            .any(|value| value.contains('|') || value.contains('\n'))
        {
            bail!("invalid delimiter in case {index}");
        }
        rows.push(values);
    }

    for (client, trials) in client_trials {
        let observed = i64::try_from(trials.len()).context("trial count overflow")?;
        if observed != expect_trials {
            bail!(
                "client group {:?} has {observed} trials, expected {expect_trials}",
                client
            );
        }
    }
    if !rows.is_empty() {
        let divisor = usize::try_from(expect_trials).context("expect_trials overflow")?;
        if !rows.len().is_multiple_of(divisor) {
            bail!("plan case count is not divisible by expect_trials");
        }
    }

    Ok((
        rows,
        json!({
            "plan_version": version,
            "expect_trials": expect_trials,
            "case_count": cases.len(),
        }),
    ))
}

pub(crate) fn run_materialize(args: KvMaterializeArgs<'_>) -> Result<()> {
    let plan: Value = serde_json::from_str(
        &fs::read_to_string(args.plan)
            .with_context(|| format!("failed to read {}", args.plan.display()))?,
    )
    .with_context(|| format!("invalid JSON in {}", args.plan.display()))?;
    let (rows, meta) = materialize(&plan)?;

    let mut tsv = String::new();
    for row in rows {
        tsv.push_str(&row.join("|"));
        tsv.push('\n');
    }
    fs::write(args.tsv_out, tsv)
        .with_context(|| format!("failed to write {}", args.tsv_out.display()))?;
    fs::write(args.meta_out, serde_json::to_string(&meta)? + "\n")
        .with_context(|| format!("failed to write {}", args.meta_out.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::materialize;
    use anyhow::Result;
    use serde_json::{Value, json};

    fn case(clients: i64, trial: i64) -> Value {
        json!({
            "scenario": "concurrency-primary",
            "engine": "redb",
            "durability": "relaxed",
            "workload": "balanced",
            "clients": clients,
            "records": 100_000,
            "ops": 50_000,
            "trial": trial,
        })
    }

    #[test]
    fn v1_defaults_to_growth_append() -> Result<()> {
        let (rows, meta) = materialize(&json!({
            "concurrency_probe_plan_version": 1,
            "case_count": 1,
            "cases": [case(1, 1)],
        }))?;
        assert_eq!(
            meta,
            json!({"plan_version":1,"expect_trials":1,"case_count":1})
        );
        assert_eq!(&rows[0][8..], ["growth", "append", "0"]);
        Ok(())
    }

    #[test]
    fn v2_multitrial_bounded_family() -> Result<()> {
        let mut cases = Vec::new();
        for clients in [1, 8] {
            for trial in [1, 2, 3] {
                let mut item = case(clients, trial);
                item["state_evolution"] = json!("bounded");
                item["write_pattern"] = json!("update-uniform");
                item["bounded_churn_slots"] = json!(2400);
                cases.push(item);
            }
        }
        let (rows, meta) = materialize(&json!({
            "concurrency_probe_plan_version": 2,
            "expect_trials": 3,
            "case_count": cases.len(),
            "cases": cases,
        }))?;
        assert_eq!(rows.len(), 6);
        assert_eq!(meta["expect_trials"], 3);
        Ok(())
    }

    #[test]
    fn missing_trial_rejected() {
        let mut cases = Vec::new();
        for trial in [1, 2] {
            let mut item = case(1, trial);
            item["state_evolution"] = json!("growth");
            item["write_pattern"] = json!("append");
            item["bounded_churn_slots"] = json!(0);
            cases.push(item);
        }
        let result = materialize(&json!({
            "concurrency_probe_plan_version": 2,
            "expect_trials": 3,
            "case_count": 2,
            "cases": cases,
        }));
        assert!(result.is_err_and(|error| error.to_string().contains("expected 3")));
    }

    #[test]
    fn bounded_append_rejected() {
        let mut item = case(1, 1);
        item["workload"] = json!("write-burst");
        item["state_evolution"] = json!("bounded");
        item["write_pattern"] = json!("append");
        item["bounded_churn_slots"] = json!(0);
        let result = materialize(&json!({
            "concurrency_probe_plan_version": 2,
            "case_count": 1,
            "cases": [item],
        }));
        assert!(result.is_err_and(|error| error.to_string().contains("requires update")));
    }

    #[test]
    fn family_total_work_must_match_clients() {
        let mut first = case(1, 1);
        first["state_evolution"] = json!("growth");
        first["write_pattern"] = json!("append");
        first["bounded_churn_slots"] = json!(0);
        let mut second = case(8, 1);
        second["state_evolution"] = json!("growth");
        second["write_pattern"] = json!("append");
        second["bounded_churn_slots"] = json!(0);
        second["ops"] = json!(60_000);
        let result = materialize(&json!({
            "concurrency_probe_plan_version": 2,
            "case_count": 2,
            "cases": [first, second],
        }));
        assert!(
            result.is_err_and(|error| error.to_string().contains("family total-work mismatch"))
        );
    }

    #[test]
    fn v2_requires_explicit_state_fields() {
        let result = materialize(&json!({
            "concurrency_probe_plan_version": 2,
            "case_count": 1,
            "cases": [case(1, 1)],
        }));
        assert!(result.is_err_and(|error| error.to_string().contains("v2 plan case 0 missing")));
    }

    #[test]
    fn trial_values_are_count_based_for_compatibility() -> Result<()> {
        let mut cases = Vec::new();
        for trial in [2, 3, 4] {
            let mut item = case(1, trial);
            item["state_evolution"] = json!("growth");
            item["write_pattern"] = json!("append");
            item["bounded_churn_slots"] = json!(0);
            cases.push(item);
        }
        let (rows, _) = materialize(&json!({
            "concurrency_probe_plan_version": 2,
            "expect_trials": 3,
            "case_count": 3,
            "cases": cases,
        }))?;
        assert_eq!(rows.len(), 3);
        Ok(())
    }
}
