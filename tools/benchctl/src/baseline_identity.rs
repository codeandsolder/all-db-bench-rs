use crate::json_value::exact_i64;
use anyhow::{Context, Result, bail};
use serde_json::Value;

fn slug(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

pub fn run_id(group: &Value, prefix: &str) -> Result<String> {
    let field = |key: &str| -> Result<String> {
        let value = group
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .with_context(|| format!("missing followup identity: {key}"))?;
        Ok(slug(value))
    };
    let positive = |key: &str| -> Result<i64> {
        group
            .get(key)
            .and_then(exact_i64)
            .filter(|&value| value > 0)
            .with_context(|| format!("invalid positive followup count: {key}"))
    };
    if prefix.is_empty() || prefix.contains('/') {
        bail!("invalid followup run prefix: {prefix}");
    }
    Ok(format!(
        "{prefix}-{}-{}-{}-e{}-t{}",
        field("engine")?,
        field("durability")?,
        field("workload")?,
        positive("suggested_effective_ops")?,
        positive("suggested_trials")?,
    ))
}

/// Validate the fields a sizing plan actually specifies, along with the
/// required workload and dataset identity and exact planned logical work.
pub fn matches_followup(group: &Value, measured: &Value) -> bool {
    for field in ["engine", "durability", "workload", "records"] {
        if group.get(field).is_none_or(Value::is_null) || measured.get(field) != group.get(field) {
            return false;
        }
    }
    if let Some(version) = group.get("engine_version")
        && measured.get("engine_version") != Some(version)
    {
        return false;
    }
    for (field, legacy) in [
        ("read_materialization", "legacy-read-v0"),
        ("write_materialization", "legacy-return-v0"),
    ] {
        if let Some(expected) = group.get(field) {
            match measured.get(field) {
                Some(actual) if actual == expected => {}
                None if expected.as_str() == Some(legacy) => {}
                _ => return false,
            }
        }
    }
    let Some(expected) = group
        .get("suggested_effective_ops")
        .and_then(exact_i64)
        .filter(|&value| value > 0)
    else {
        return false;
    };
    measured.get("ops_requested").and_then(exact_i64) == Some(expected)
        && measured
            .get("ops_completed")
            .is_none_or(|value| exact_i64(value) == Some(expected))
}

/// The sorted observed IDs must be exactly 1 through the expected positive count.
pub fn matches_trials(trials: &[i64], expected: i64) -> bool {
    expected > 0
        && usize::try_from(expected).ok() == Some(trials.len())
        && trials
            .iter()
            .enumerate()
            .all(|(index, trial)| i64::try_from(index + 1).ok() == Some(*trial))
}
