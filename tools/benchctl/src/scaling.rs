use crate::json_io::read_json;
use anyhow::{Context, Result, bail};
use serde_json::{Map, Number, Value, json};
use std::{collections::BTreeMap, fs, path::Path};

const IDENTITY_FIELDS: [&str; 24] = [
    "format_version",
    "lane",
    "scenario",
    "engine",
    "engine_version",
    "durability",
    "workload",
    "records",
    "ops_requested",
    "value_bytes",
    "value_pattern",
    "key_bytes",
    "key_shape",
    "access_pattern",
    "miss_percent",
    "write_pattern",
    "state_evolution",
    "bounded_churn_slots",
    "txn_size",
    "read_materialization",
    "write_materialization",
    "scan_len",
    "settle_ms",
    "payload_pattern",
];

fn value_i64(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    if let Some(integer) = value.as_i64() {
        return Some(integer);
    }
    if let Some(integer) = value.as_u64() {
        return i64::try_from(integer).ok();
    }
    value.as_str()?.parse::<i64>().ok()
}

fn value_f64(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    if let Some(number) = value.as_f64() {
        return Some(number);
    }
    value.as_str()?.parse::<f64>().ok()
}

fn required_i64(value: &Value, key: &str) -> Result<i64> {
    value_i64(value.get(key)).with_context(|| format!("missing or invalid {key}"))
}

fn required_f64(value: &Value, key: &str) -> Result<f64> {
    value_f64(value.get(key)).with_context(|| format!("missing or invalid {key}"))
}

fn required_str<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("missing or invalid {key}"))
}

fn number(value: f64) -> Result<Value> {
    Number::from_f64(value)
        .map(Value::Number)
        .with_context(|| format!("cannot encode non-finite float {value}"))
}

fn parse_clients(value: &Value) -> Result<Vec<i64>> {
    if let Some(values) = value.as_array() {
        return values
            .iter()
            .map(|item| value_i64(Some(item)).context("invalid client value"))
            .collect();
    }
    let raw = value
        .as_str()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| value.to_string());
    raw.split_whitespace()
        .filter(|item| !item.trim().is_empty())
        .map(|item| {
            item.parse::<i64>()
                .with_context(|| format!("invalid client value {item:?}"))
        })
        .collect()
}

fn expected_clients(support: &Value, scenario: &str) -> Result<Vec<i64>> {
    let lane = support.get("lane").and_then(Value::as_str);
    let key = match lane {
        Some("kv-concurrency") => match scenario {
            "concurrency-primary" => Some("clients"),
            "concurrency-range" => Some("range_clients"),
            "concurrency-delete" => Some("delete_clients"),
            "concurrency-relaxed" => Some("relaxed_clients"),
            _ => None,
        },
        Some("record-concurrency") => match scenario {
            "record-concurrency-core" => Some("clients"),
            "record-concurrency-relaxed" => Some("relaxed_clients"),
            "record-concurrency-large-payload" | "record-concurrency-hotset" => {
                Some("stress_clients")
            }
            "record-concurrency-txn-1" | "record-concurrency-txn-1000" => Some("tx_clients"),
            _ => None,
        },
        _ => bail!("unsupported concurrency lane: {lane:?}"),
    };
    let key = key
        .with_context(|| format!("no client policy for scenario {scenario:?} in lane {lane:?}"))?;
    parse_clients(
        support.get(key).with_context(|| {
            format!("no client policy for scenario {scenario:?} in lane {lane:?}")
        })?,
    )
}

fn identity_key(row: &Value) -> Result<String> {
    let values = IDENTITY_FIELDS
        .iter()
        .map(|field| row.get(*field).cloned().unwrap_or(Value::Null))
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&values)?)
}

fn rejected_pressure_attempts(run_dir: &Path) -> Result<i64> {
    let root = run_dir.join("rejected-pressure");
    if !root.is_dir() {
        return Ok(0);
    }
    let mut count = 0_i64;
    for case in fs::read_dir(&root).with_context(|| format!("failed to list {}", root.display()))? {
        let case = case?.path();
        if !case.is_dir() {
            continue;
        }
        for attempt in
            fs::read_dir(&case).with_context(|| format!("failed to list {}", case.display()))?
        {
            let attempt = attempt?.path();
            if attempt.is_dir()
                && attempt
                    .file_name()
                    .and_then(|value| value.to_str())
                    .is_some_and(|name| name.starts_with("attempt-"))
                && attempt.join("rejection.json").is_file()
            {
                count += 1;
            }
        }
    }
    Ok(count)
}

fn optional_clone(row: &Value, key: &str) -> Value {
    row.get(key).cloned().unwrap_or(Value::Null)
}

fn optional_number(row: Option<&Value>, key: &str) -> Result<Value> {
    match row.and_then(|item| value_f64(item.get(key))) {
        Some(value) => number(value),
        None => Ok(Value::Null),
    }
}

fn build_report(
    summary: &Value,
    support: &Value,
    run_dir: Option<&Path>,
    allow_incomplete: bool,
) -> Result<Value> {
    let lane = support
        .get("lane")
        .and_then(Value::as_str)
        .context("unsupported concurrency lane: None")?;
    if !matches!(lane, "kv-concurrency" | "record-concurrency") {
        bail!("unsupported concurrency lane: {lane:?}");
    }
    let trials = required_i64(support, "trials")?;
    let expected_rows = required_i64(support, "case_count")?;
    let expected_group_count = expected_rows / trials;
    let observed_rows = value_i64(summary.get("row_count")).unwrap_or(0);
    let observed_group_count = value_i64(summary.get("group_count")).unwrap_or(0);
    let mut problems: Vec<String> = summary
        .get("problems")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(ToOwned::to_owned)
                        .unwrap_or_else(|| item.to_string())
                })
                .collect()
        })
        .unwrap_or_default();
    if !allow_incomplete {
        if observed_rows != expected_rows {
            problems.push(format!(
                "row_count={observed_rows}, expected={expected_rows}"
            ));
        }
        if observed_group_count != expected_group_count {
            problems.push(format!(
                "group_count={observed_group_count}, expected={expected_group_count}"
            ));
        }
    }

    let mut grouped: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for row in summary
        .get("groups")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        if row.get("lane").and_then(Value::as_str) != Some(lane) {
            problems.push(format!(
                "unexpected lane in summary group: {:?}",
                row.get("lane")
            ));
            continue;
        }
        grouped
            .entry(identity_key(row)?)
            .or_default()
            .push(row.clone());
    }

    let mut families = Vec::new();
    for mut rows in grouped.into_values() {
        rows.sort_by_key(|row| value_i64(row.get("clients")).unwrap_or(i64::MIN));
        let first = rows.first().context("empty concurrency family")?;
        let expected = expected_clients(support, required_str(first, "scenario")?)?;
        let actual: Vec<i64> = rows
            .iter()
            .map(|row| required_i64(row, "clients"))
            .collect::<Result<_>>()?;
        let expected_set = expected
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        let actual_set = actual
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        let missing = expected_set
            .difference(&actual_set)
            .copied()
            .collect::<Vec<_>>();
        let extra = actual_set
            .difference(&expected_set)
            .copied()
            .collect::<Vec<_>>();
        let duplicate = actual.len() != actual_set.len();
        let complete = missing.is_empty() && extra.is_empty() && !duplicate;
        if !complete && !allow_incomplete {
            problems.push(format!(
                "{}/{}/{}/{}: clients={actual:?}, expected={expected:?}",
                required_str(first, "engine")?,
                required_str(first, "scenario")?,
                required_str(first, "workload")?,
                required_str(first, "durability")?,
            ));
        }
        let mut by_client = BTreeMap::new();
        for row in &rows {
            by_client.insert(required_i64(row, "clients")?, row);
        }
        let base = by_client.get(&1).copied();
        let max_client = *expected
            .iter()
            .max()
            .context("expected client set is empty")?;
        let max_row = by_client.get(&max_client).copied();
        let mut rates = Map::new();
        for (client, row) in &by_client {
            rates.insert(
                client.to_string(),
                number(required_f64(row, "ops_per_s_median")?)?,
            );
        }
        let family = json!({
            "scenario": optional_clone(first, "scenario"),
            "engine": optional_clone(first, "engine"),
            "engine_version": optional_clone(first, "engine_version"),
            "durability": optional_clone(first, "durability"),
            "workload": optional_clone(first, "workload"),
            "records": optional_clone(first, "records"),
            "ops_requested": optional_clone(first, "ops_requested"),
            "value_bytes": optional_clone(first, "value_bytes"),
            "state_evolution": first.get("state_evolution").cloned().unwrap_or_else(|| json!("growth")),
            "bounded_churn_slots": first.get("bounded_churn_slots").cloned().unwrap_or_else(|| json!(0)),
            "txn_size": optional_clone(first, "txn_size"),
            "scan_len": optional_clone(first, "scan_len"),
            "expected_clients": expected,
            "observed_clients": actual,
            "missing_clients": missing,
            "complete": complete,
            "ops_per_s_by_client": rates,
            "c1_ops_per_s": optional_number(base, "ops_per_s_median")?,
            "max_clients": max_client,
            "max_client_ops_per_s": optional_number(max_row, "ops_per_s_median")?,
            "speedup_at_max_clients": max_row.and_then(|row| row.get("speedup_vs_c1")).cloned().unwrap_or(Value::Null),
            "parallel_efficiency_at_max_clients": max_row.and_then(|row| row.get("parallel_efficiency_vs_c1")).cloned().unwrap_or(Value::Null),
            "p99_read_or_op_multiplier_at_max_clients": max_row.and_then(|row| row.get("p99_read_multiplier_vs_c1")).cloned().unwrap_or(Value::Null),
            "p99_write_txn_multiplier_at_max_clients": max_row.and_then(|row| row.get("p99_write_txn_multiplier_vs_c1")).cloned().unwrap_or(Value::Null),
            "client_max_min_ratio_at_max_clients": max_row.and_then(|row| row.get("client_throughput_max_min_ratio_median")).cloned().unwrap_or(Value::Null),
            "cpu_cores_at_max_clients": max_row.and_then(|row| row.get("cpu_cores_median")).cloned().unwrap_or(Value::Null),
            "conflict_retries_per_k_write_ops_at_max_clients": max_row.and_then(|row| row.get("write_conflict_retries_per_k_write_ops_median")).cloned().unwrap_or(Value::Null),
        });
        families.push(family);
    }
    families.sort_by(|left, right| {
        ["scenario", "workload", "durability", "engine"]
            .into_iter()
            .map(|key| left.get(key).and_then(Value::as_str).unwrap_or(""))
            .cmp(
                ["scenario", "workload", "durability", "engine"]
                    .into_iter()
                    .map(|key| right.get(key).and_then(Value::as_str).unwrap_or("")),
            )
    });
    let complete_count = families
        .iter()
        .filter(|family| family.get("complete").and_then(Value::as_bool) == Some(true))
        .count();
    Ok(json!({
        "lane": lane,
        "profile": support.get("profile").cloned().unwrap_or(Value::Null),
        "trials": trials,
        "expected_rows": expected_rows,
        "observed_rows": observed_rows,
        "expected_group_count": expected_group_count,
        "observed_group_count": observed_group_count,
        "family_count": families.len(),
        "complete_family_count": complete_count,
        "incomplete_family_count": families.len() - complete_count,
        "rejected_pressure_attempts": match run_dir { Some(path) => json!(rejected_pressure_attempts(path)?), None => Value::Null },
        "problems": problems,
        "families": families,
    }))
}

fn fmt(value: Option<&Value>, digits: usize) -> String {
    let Some(value) = value.and_then(Value::as_f64) else {
        return "—".to_owned();
    };
    format!("{value:.digits$}")
}

fn render_markdown(report: &Value) -> Result<String> {
    let problems = report
        .get("problems")
        .and_then(Value::as_array)
        .context("report problems must be an array")?;
    let families = report
        .get("families")
        .and_then(Value::as_array)
        .context("report families must be an array")?;
    let mut lines = vec![
        format!("# {} scaling report", required_str(report, "lane")?),
        String::new(),
        format!(
            "Rows: **{}/{}**; summary groups: **{}/{}**; families observed: **{}**; complete client-set families: **{}**.",
            required_i64(report, "observed_rows")?,
            required_i64(report, "expected_rows")?,
            required_i64(report, "observed_group_count")?,
            required_i64(report, "expected_group_count")?,
            required_i64(report, "family_count")?,
            required_i64(report, "complete_family_count")?,
        ),
    ];
    if !report
        .get("rejected_pressure_attempts")
        .is_none_or(Value::is_null)
    {
        lines.push(format!(
            "Archived transient-pressure attempts: **{}**.",
            required_i64(report, "rejected_pressure_attempts")?
        ));
    }
    if !problems.is_empty() {
        lines.extend([String::new(), "## Problems".to_owned(), String::new()]);
        for problem in problems {
            lines.push(format!(
                "- {}",
                problem
                    .as_str()
                    .unwrap_or_else(|| problem.as_str().unwrap_or(""))
            ));
        }
    }
    lines.extend([
        String::new(),
        "## Scaling families".to_owned(),
        String::new(),
        "| scenario | workload | durability | engine | clients | c1 ops/s | max-c ops/s | speedup | efficiency | p99 read/op x | p99 write-txn x | client max/min | CPU cores | conflict retries/k write ops |".to_owned(),
        "| --- | --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |".to_owned(),
    ]);
    for family in families {
        let clients = family
            .get("observed_clients")
            .and_then(Value::as_array)
            .context("observed_clients must be an array")?
            .iter()
            .map(|value| value_i64(Some(value)).context("invalid observed client"))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join(",");
        lines.push(format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            required_str(family, "scenario")?,
            required_str(family, "workload")?,
            required_str(family, "durability")?,
            required_str(family, "engine")?,
            clients,
            fmt(family.get("c1_ops_per_s"), 1),
            fmt(family.get("max_client_ops_per_s"), 1),
            fmt(family.get("speedup_at_max_clients"), 2),
            fmt(family.get("parallel_efficiency_at_max_clients"), 2),
            fmt(family.get("p99_read_or_op_multiplier_at_max_clients"), 2),
            fmt(family.get("p99_write_txn_multiplier_at_max_clients"), 2),
            fmt(family.get("client_max_min_ratio_at_max_clients"), 2),
            fmt(family.get("cpu_cores_at_max_clients"), 2),
            fmt(
                family.get("conflict_retries_per_k_write_ops_at_max_clients"),
                2
            ),
        ));
    }
    Ok(lines.join("\n") + "\n")
}

pub(crate) struct ScalingArgs<'a> {
    pub(crate) run_dir: &'a Path,
    pub(crate) json_out: Option<&'a Path>,
    pub(crate) markdown_out: Option<&'a Path>,
    pub(crate) allow_incomplete: bool,
}

pub(crate) fn run(args: ScalingArgs<'_>) -> Result<i32> {
    let summary_path = args.run_dir.join("summary.json");
    let support_path = args.run_dir.join("support.json");
    if !summary_path.is_file() || !support_path.is_file() {
        bail!("run_dir must contain summary.json and support.json");
    }
    let summary = read_json(&summary_path)?;
    let support = read_json(&support_path)?;
    let report = build_report(
        &summary,
        &support,
        Some(args.run_dir),
        args.allow_incomplete,
    )?;
    let markdown = render_markdown(&report)?;
    if let Some(path) = args.json_out {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, serde_json::to_string_pretty(&report)? + "\n")?;
    }
    if let Some(path) = args.markdown_out {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, &markdown)?;
    } else {
        print!("{markdown}");
    }
    let has_problems = report
        .get("problems")
        .and_then(Value::as_array)
        .is_some_and(|items| !items.is_empty());
    Ok(if has_problems && !args.allow_incomplete {
        1
    } else {
        0
    })
}

#[cfg(test)]
mod tests {
    use super::{build_report, render_markdown};
    use anyhow::Result;
    use serde_json::{Value, json};

    fn support(lane: &str) -> Value {
        json!({
            "lane": lane,
            "profile": "quick",
            "trials": 3,
            "case_count": 6,
            "clients": [1, 8],
            "range_clients": [1, 8],
            "delete_clients": [1, 8],
            "relaxed_clients": [1, 8],
            "stress_clients": [1, 8],
            "tx_clients": [1, 8],
        })
    }

    fn group(lane: &str, scenario: &str, clients: i64, rate: f64, speedup: Option<f64>) -> Value {
        let mut value = json!({
            "format_version": 8,
            "lane": lane,
            "scenario": scenario,
            "engine": "sqlite",
            "engine_version": "3",
            "durability": "sync",
            "workload": "point-read",
            "records": 10000,
            "ops_requested": 6000,
            "value_bytes": 512,
            "value_pattern": "hex-random",
            "key_bytes": 16,
            "key_shape": "u64",
            "access_pattern": "uniform",
            "miss_percent": 0,
            "write_pattern": "append",
            "state_evolution": "growth",
            "bounded_churn_slots": 0,
            "txn_size": 100,
            "read_materialization": "full-record-v1",
            "write_materialization": "no-return-v1",
            "scan_len": 0,
            "settle_ms": 0,
            "payload_pattern": "hex-random",
            "clients": clients,
            "ops_per_s_median": rate,
            "p99_read_multiplier_vs_c1": if clients == 8 { json!(1.5) } else { Value::Null },
            "p99_write_txn_multiplier_vs_c1": Value::Null,
            "client_throughput_max_min_ratio_median": 1.1,
            "cpu_cores_median": 2.0,
            "write_conflict_retries_per_k_write_ops_median": Value::Null,
        });
        if let Some(speedup) = speedup {
            value["speedup_vs_c1"] = json!(speedup);
            value["parallel_efficiency_vs_c1"] = json!(speedup / clients as f64);
        }
        value
    }

    #[test]
    fn complete_family_report() -> Result<()> {
        let summary = json!({
            "row_count": 6,
            "group_count": 2,
            "problems": [],
            "groups": [
                group("record-concurrency", "record-concurrency-core", 1, 100.0, None),
                group("record-concurrency", "record-concurrency-core", 8, 500.0, Some(5.0)),
            ],
        });
        let report = build_report(&summary, &support("record-concurrency"), None, false)?;
        assert_eq!(report["complete_family_count"], 1);
        assert_eq!(report["families"][0]["speedup_at_max_clients"], 5.0);
        assert_eq!(
            report["families"][0]["parallel_efficiency_at_max_clients"],
            0.625
        );
        assert!(render_markdown(&report)?.contains("500.0 | 5.00 | 0.62"));
        Ok(())
    }

    #[test]
    fn missing_client_is_problem_unless_allowed() -> Result<()> {
        let summary = json!({
            "row_count": 3,
            "group_count": 1,
            "problems": [],
            "groups": [group("kv-concurrency", "concurrency-primary", 1, 100.0, None)],
        });
        let report = build_report(&summary, &support("kv-concurrency"), None, false)?;
        assert_eq!(report["incomplete_family_count"], 1);
        assert!(
            report["problems"]
                .as_array()
                .is_some_and(|items| items.len() >= 3)
        );
        let allowed = build_report(&summary, &support("kv-concurrency"), None, true)?;
        assert!(allowed["problems"].as_array().is_some_and(Vec::is_empty));
        Ok(())
    }

    #[test]
    fn wrong_lane_and_unknown_scenario_are_rejected() {
        let summary = json!({"row_count": 0, "group_count": 0, "groups": []});
        assert!(build_report(&summary, &support("other"), None, false).is_err());
        let summary = json!({
            "row_count": 3,
            "group_count": 1,
            "groups": [group("kv-concurrency", "unknown", 1, 100.0, None)],
        });
        assert!(build_report(&summary, &support("kv-concurrency"), None, true).is_err());
    }
}
