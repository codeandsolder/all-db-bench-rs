use crate::json_io::read_json;
use crate::json_value::py_string;
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

pub(crate) struct RecoveryArgs<'a> {
    pub(crate) run_dir: &'a Path,
    pub(crate) json_out: Option<&'a Path>,
    pub(crate) markdown_out: Option<&'a Path>,
}

fn read_ndjson(path: &Path) -> Result<Vec<Value>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut rows = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let row = serde_json::from_str(line)
            .with_context(|| format!("{}:{}: invalid JSON", path.display(), index + 1))?;
        rows.push(row);
    }
    Ok(rows)
}

fn sorted_json_files(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut paths = fs::read_dir(dir)
        .with_context(|| format!("failed to list {}", dir.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

fn text_field(value: &Value, key: &str, default: &str) -> String {
    value.get(key).map_or_else(|| default.to_owned(), py_string)
}

fn int_field(value: &Value, key: &str, default: i64) -> i64 {
    match value.get(key) {
        Some(Value::Number(number)) => number.as_i64().unwrap_or(default),
        Some(Value::String(raw)) => raw.parse::<i64>().unwrap_or(default),
        _ => default,
    }
}

fn verification(row: &Value) -> Map<String, Value> {
    row.get("verification")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

fn verification_ok(value: &Map<String, Value>) -> bool {
    value
        .get("verification_ok")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn structurally_valid(value: &Map<String, Value>) -> bool {
    value
        .get("prefix_present_after_gap")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        == 0
        && value
            .get("tail_present_after_gap")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            == 0
        && value
            .get("transaction_atomic_tail")
            .and_then(Value::as_bool)
            .unwrap_or(true)
}

fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    Ok(())
}

pub(crate) fn run(args: RecoveryArgs<'_>) -> Result<()> {
    let run_dir = args
        .run_dir
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", args.run_dir.display()))?;

    let mut cases = BTreeMap::<String, Value>::new();
    for path in sorted_json_files(&run_dir.join("cases"))? {
        let case_id = path
            .file_stem()
            .and_then(|value| value.to_str())
            .context("recovery case filename is not UTF-8")?
            .to_owned();
        if cases.insert(case_id.clone(), read_json(&path)?).is_some() {
            bail!("duplicate case id: {case_id}");
        }
    }

    let failures = read_ndjson(&run_dir.join("failures.ndjson"))?;
    let relaxed_losses = read_ndjson(&run_dir.join("relaxed-ack-losses.ndjson"))?;
    let sidecar_paths = sorted_json_files(&run_dir.join("powerloss"))?;
    let mut sidecars = BTreeMap::<String, Value>::new();
    for path in sidecar_paths {
        let stem = path
            .file_stem()
            .and_then(|value| value.to_str())
            .context("powerloss sidecar filename is not UTF-8")?
            .to_owned();
        sidecars.insert(stem, read_json(&path)?);
    }

    let failure_case_ids = failures
        .iter()
        .map(|row| {
            row.get("case_id").map_or_else(
                || "None".to_owned(),
                |value| match value {
                    Value::String(value) => value.clone(),
                    Value::Null => "None".to_owned(),
                    other => other.to_string(),
                },
            )
        })
        .collect::<BTreeSet<_>>();
    let mut failure_stages = BTreeMap::<String, usize>::new();
    for row in &failures {
        let stage = row
            .get("stage")
            .map_or_else(|| "unknown".to_owned(), py_string);
        *failure_stages.entry(stage).or_default() += 1;
    }

    let mut grouped = BTreeMap::<(String, String), Vec<(&String, &Value)>>::new();
    for (case_id, row) in &cases {
        grouped
            .entry((
                text_field(row, "engine", "unknown"),
                text_field(row, "durability", "unknown"),
            ))
            .or_default()
            .push((case_id, row));
    }

    let mut groups = Vec::<Value>::new();
    for ((engine, durability), rows) in grouped {
        let verifications = rows
            .iter()
            .map(|(_, row)| verification(row))
            .collect::<Vec<_>>();
        let completed_cases = rows.len();
        let ok = verifications
            .iter()
            .filter(|value| verification_ok(value))
            .count();
        let structural_failures = verifications
            .iter()
            .filter(|value| !structurally_valid(value))
            .count();
        let max_field = |key: &str| {
            verifications
                .iter()
                .map(|value| match value.get(key) {
                    Some(Value::Number(number)) => number.as_i64().unwrap_or(0),
                    Some(Value::String(raw)) => raw.parse::<i64>().unwrap_or(0),
                    _ => 0,
                })
                .max()
                .unwrap_or(0)
        };
        groups.push(json!({
            "engine": engine,
            "durability": durability,
            "completed_cases": completed_cases,
            "verification_ok": ok,
            "verification_failed": completed_cases - ok,
            "structural_failures": structural_failures,
            "max_missing_prefix_records": max_field("missing_prefix_records"),
            "max_prefix_present_after_gap": max_field("prefix_present_after_gap"),
            "max_tail_present_after_gap": max_field("tail_present_after_gap"),
            "max_unreported_tail_records": max_field("tail_prefix_present"),
            "harness_failures": rows.iter().filter(|(case_id, _)| failure_case_ids.contains(case_id.as_str())).count(),
        }));
    }

    let support_path = run_dir.join("support.json");
    let support = if support_path.exists() {
        read_json(&support_path)?
    } else {
        Value::Null
    };
    let completed_cases = cases.len();
    let all_completed_verifications_ok = if cases.is_empty() {
        false
    } else {
        cases
            .values()
            .all(|row| verification_ok(&verification(row)))
    };
    let all_sync_verifications_ok = if cases.is_empty() {
        false
    } else {
        cases.values().all(|row| {
            text_field(row, "durability", "None") != "sync" || verification_ok(&verification(row))
        })
    };
    let all_structurally_valid = if cases.is_empty() {
        false
    } else {
        cases
            .values()
            .all(|row| structurally_valid(&verification(row)))
    };

    let summary = json!({
        "run_dir": run_dir.to_string_lossy(),
        "support": support,
        "completed_cases": completed_cases,
        "failure_records": failures.len(),
        "failure_stages": failure_stages,
        "relaxed_ack_loss_cases": relaxed_losses.len(),
        "powerloss_sidecars": sidecars.len(),
        "all_completed_verifications_ok": all_completed_verifications_ok,
        "all_sync_verifications_ok": all_sync_verifications_ok,
        "all_structurally_valid": all_structurally_valid,
        "groups": groups,
    });

    if let Some(path) = args.json_out {
        ensure_parent(path)?;
        fs::write(path, serde_json::to_string_pretty(&summary)? + "\n")
            .with_context(|| format!("failed to write {}", path.display()))?;
    }

    let mut lines = vec![
        "# Recovery campaign summary".to_owned(),
        String::new(),
        format!("- Completed case results: {completed_cases}"),
        format!("- Harness/failure records: {}", failures.len()),
        format!("- Relaxed acknowledged-loss observations: {}", relaxed_losses.len()),
        format!("- Power-loss sidecars: {}", sidecars.len()),
        String::new(),
        "| Engine | Durability | Cases | OK | Failed | Structural | Max missing prefix | Max tail | Harness failures |".to_owned(),
        "|---|---|---:|---:|---:|---:|---:|---:|---:|".to_owned(),
    ];
    if let Some(groups) = summary.get("groups").and_then(Value::as_array) {
        for group in groups {
            lines.push(format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                group.get("engine").and_then(Value::as_str).unwrap_or(""),
                group
                    .get("durability")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                int_field(group, "completed_cases", 0),
                int_field(group, "verification_ok", 0),
                int_field(group, "verification_failed", 0),
                int_field(group, "structural_failures", 0),
                int_field(group, "max_missing_prefix_records", 0),
                int_field(group, "max_unreported_tail_records", 0),
                int_field(group, "harness_failures", 0),
            ));
        }
    }
    if !failure_stages.is_empty() {
        lines.push(String::new());
        lines.push("## Failure stages".to_owned());
        for (stage, count) in failure_stages {
            lines.push(format!("- `{stage}`: {count}"));
        }
    }
    let markdown = lines.join("\n") + "\n";
    if let Some(path) = args.markdown_out {
        ensure_parent(path)?;
        fs::write(path, &markdown)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    if args.json_out.is_none() && args.markdown_out.is_none() {
        print!("{markdown}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{structurally_valid, verification_ok};
    use serde_json::{Map, json};

    #[test]
    fn verification_defaults_match_python_contract() {
        let empty = Map::new();
        assert!(!verification_ok(&empty));
        assert!(structurally_valid(&empty));
        let bad = json!({"prefix_present_after_gap":1,"transaction_atomic_tail":false})
            .as_object()
            .cloned()
            .unwrap_or_default();
        assert!(!structurally_valid(&bad));
    }
}
