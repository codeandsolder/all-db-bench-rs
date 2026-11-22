use super::{get_i64, get_str, value_i64};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub(super) struct FamilyKey {
    pub(super) scenario: String,
    pub(super) engine: String,
    pub(super) durability: String,
    pub(super) workload: String,
}

/// Work held constant across every client count of a sizing family.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct WorkSpec {
    pub(super) records: i64,
    pub(super) ops: i64,
    pub(super) state_evolution: String,
    pub(super) write_pattern: String,
    pub(super) bounded_churn_slots: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ResultKey {
    pub(super) family: FamilyKey,
    pub(super) clients: i64,
    pub(super) work: WorkSpec,
    pub(super) trial: i64,
}

pub(super) fn family_key(value: &Value) -> Result<FamilyKey> {
    Ok(FamilyKey {
        scenario: get_str(value, "scenario")?.to_owned(),
        engine: get_str(value, "engine")?.to_owned(),
        durability: get_str(value, "durability")?.to_owned(),
        workload: get_str(value, "workload")?.to_owned(),
    })
}

fn string_or_default(value: &Value, field: &str, default: &str) -> Result<String> {
    if value.get(field).is_some() {
        Ok(get_str(value, field)?.to_owned())
    } else {
        Ok(default.to_owned())
    }
}

pub(super) fn result_key(value: &Value) -> Result<ResultKey> {
    let ops = value
        .get("ops")
        .or_else(|| value.get("ops_requested"))
        .and_then(value_i64)
        .context("missing or invalid ops/ops_requested")?;
    if value.get("ops").is_some()
        && value.get("ops_requested").is_some()
        && get_i64(value, "ops_requested")? != ops
    {
        bail!("conflicting ops and ops_requested");
    }
    let bounded_churn_slots = if value.get("bounded_churn_slots").is_some() {
        get_i64(value, "bounded_churn_slots")?
    } else {
        0
    };
    Ok(ResultKey {
        family: family_key(value)?,
        clients: get_i64(value, "clients")?,
        trial: get_i64(value, "trial")?,
        work: WorkSpec {
            records: get_i64(value, "records")?,
            ops,
            state_evolution: string_or_default(value, "state_evolution", "growth")?,
            write_pattern: string_or_default(value, "write_pattern", "append")?,
            bounded_churn_slots,
        },
    })
}
