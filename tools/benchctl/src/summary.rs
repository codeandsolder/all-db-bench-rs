mod aggregate;
mod model;
mod render;

use aggregate::summarize_rows;
use anyhow::{Context, Result, bail};
use render::markdown;
use serde_json::{Value, json};
use std::{fs, path::Path};

fn read_rows(input: &Path) -> Result<Vec<Value>> {
    let content =
        fs::read_to_string(input).with_context(|| format!("failed to read {}", input.display()))?;
    let mut rows = Vec::new();
    for (index, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        rows.push(
            serde_json::from_str(line)
                .with_context(|| format!("invalid JSON at {}:{}", input.display(), index + 1))?,
        );
    }
    if rows.is_empty() {
        bail!("no measurements");
    }
    Ok(rows)
}

pub(crate) fn summarize_file(
    input: &Path,
    json_out: Option<&Path>,
    markdown_out: Option<&Path>,
    expect_trials: Option<usize>,
) -> Result<()> {
    let rows = read_rows(input)?;
    let (summary, problems) = summarize_rows(&rows, expect_trials)?;
    let text = markdown(&summary, &problems);
    if let Some(path) = markdown_out {
        fs::write(path, format!("{text}\n"))
            .with_context(|| format!("failed to write {}", path.display()))?;
    } else {
        println!("{text}");
    }
    if let Some(path) = json_out {
        let payload = json!({
            "source": input.to_string_lossy(),
            "row_count": rows.len(),
            "group_count": summary.len(),
            "problems": problems,
            "groups": summary,
        });
        fs::write(path, serde_json::to_string_pretty(&payload)? + "\n")
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    if !problems.is_empty() {
        bail!("summary has {} completeness problem(s)", problems.len());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{aggregate::summarize_rows, model::group_key};
    use anyhow::{Context, Result};
    use serde_json::{Value, json};

    fn kv_row(clients: u64, state: &str, slots: u64, trial: u64, rate: f64) -> Value {
        json!({
            "format_version": 6,
            "lane": "kv-concurrency",
            "scenario": "concurrency-primary",
            "engine": "redb",
            "engine_version": "4.3.0",
            "durability": "relaxed",
            "durability_mapping": "relaxed",
            "workload": "balanced",
            "records": 100_000,
            "ops_requested": 50_000,
            "ops_completed": 50_000,
            "clients": clients,
            "value_bytes": 256,
            "value_pattern": "pseudo-random",
            "key_bytes": 8,
            "key_shape": "sequential",
            "access_pattern": "auto",
            "miss_percent": 0,
            "write_pattern": "update-uniform",
            "state_evolution": state,
            "bounded_churn_slots": slots,
            "txn_size": 100,
            "scan_len": 100,
            "trial": trial,
            "ops_per_s": rate,
            "db_bytes": 1024,
            "elapsed_s": 1.0,
            "read_latency": {"count": 1, "p99_us": 10},
            "write_txn_latency": {"count": 1, "p99_us": 20}
        })
    }

    fn record_row(pattern: Option<&str>, read: Option<&str>, write: Option<&str>) -> Value {
        let mut row = json!({
            "format_version": 8,
            "lane": "record",
            "scenario": "baseline-core",
            "engine": "sqlite",
            "engine_version": "rusqlite / SQLite",
            "durability": "sync",
            "durability_mapping": "sync",
            "workload": "point-read",
            "records": 10_000,
            "ops_requested": 10_000,
            "ops_completed": 10_000,
            "payload_bytes": 512,
            "txn_size": 100,
            "trial": 1,
            "ops_per_s": 100.0,
            "db_bytes": 4096,
            "elapsed_s": 1.0
        });
        if let Some(pattern) = pattern {
            row["payload_pattern"] = json!(pattern);
        }
        if let Some(read) = read {
            row["read_materialization"] = json!(read);
        }
        if let Some(write) = write {
            row["write_materialization"] = json!(write);
        }
        row
    }

    #[test]
    fn state_and_pool_are_group_identity() -> Result<()> {
        let growth = kv_row(8, "growth", 0, 1, 100.0);
        let bounded = kv_row(8, "bounded", 8192, 1, 100.0);
        assert_ne!(group_key(&growth)?, group_key(&bounded)?);
        Ok(())
    }

    #[test]
    fn missing_state_defaults_to_growth_without_pool() -> Result<()> {
        let explicit = kv_row(8, "growth", 0, 1, 100.0);
        let mut legacy = explicit.clone();
        legacy
            .as_object_mut()
            .context("row object")?
            .remove("state_evolution");
        legacy
            .as_object_mut()
            .context("row object")?
            .remove("bounded_churn_slots");
        assert_eq!(group_key(&legacy)?, group_key(&explicit)?);
        Ok(())
    }

    #[test]
    fn record_payload_and_materialization_are_identity() -> Result<()> {
        let baseline = record_row(
            Some("hex-random"),
            Some("full-record-v1"),
            Some("no-return-v1"),
        );
        let shape = record_row(Some("zeros"), Some("full-record-v1"), Some("no-return-v1"));
        let read = record_row(
            Some("hex-random"),
            Some("legacy-read-v0"),
            Some("no-return-v1"),
        );
        let write = record_row(
            Some("hex-random"),
            Some("full-record-v1"),
            Some("legacy-return-v0"),
        );
        assert_ne!(group_key(&baseline)?, group_key(&shape)?);
        assert_ne!(group_key(&baseline)?, group_key(&read)?);
        assert_ne!(group_key(&baseline)?, group_key(&write)?);
        Ok(())
    }

    #[test]
    fn missing_record_markers_use_legacy_defaults() -> Result<()> {
        let missing = record_row(None, None, None);
        let explicit = record_row(
            Some("hex-random"),
            Some("legacy-read-v0"),
            Some("legacy-return-v0"),
        );
        assert_eq!(group_key(&missing)?, group_key(&explicit)?);
        Ok(())
    }

    #[test]
    fn concurrency_scaling_matches_c1_with_same_state() -> Result<()> {
        let rows = vec![
            kv_row(1, "growth", 0, 1, 100.0),
            kv_row(8, "growth", 0, 1, 400.0),
            kv_row(1, "bounded", 8192, 1, 200.0),
            kv_row(8, "bounded", 8192, 1, 600.0),
        ];
        let (groups, problems) = summarize_rows(&rows, Some(1))?;
        assert!(problems.is_empty());
        let growth = groups
            .iter()
            .find(|group| group.clients == 8 && group.state_evolution == "growth")
            .context("growth c8")?;
        let bounded = groups
            .iter()
            .find(|group| group.clients == 8 && group.state_evolution == "bounded")
            .context("bounded c8")?;
        assert_eq!(growth.speedup_vs_c1, Some(4.0));
        assert_eq!(growth.parallel_efficiency_vs_c1, Some(0.5));
        assert_eq!(bounded.speedup_vs_c1, Some(3.0));
        Ok(())
    }

    #[test]
    fn concurrency_baselines_never_cross_lanes() -> Result<()> {
        let kv = kv_row(1, "growth", 0, 1, 100.0);
        let mut record = kv.clone();
        record["lane"] = json!("record-concurrency");
        record["ops_per_s"] = json!(500.0);
        let mut record_c8 = record.clone();
        record_c8["clients"] = json!(8);
        record_c8["ops_per_s"] = json!(1000.0);
        let (groups, _) = summarize_rows(&[kv, record, record_c8], Some(1))?;
        let c8 = groups
            .iter()
            .find(|group| group.lane == "record-concurrency" && group.clients == 8)
            .context("record c8")?;
        assert_eq!(c8.speedup_vs_c1, Some(2.0));
        Ok(())
    }

    #[test]
    fn partially_present_optional_metrics_ignore_missing_trials() -> Result<()> {
        let mut first = kv_row(1, "growth", 0, 1, 100.0);
        first["open_s"] = json!(0.25);
        first["post_workload_settle"] = json!({
            "requested_ms": 0,
            "process": {"cpu_runtime_ns": 100, "write_bytes": 200},
            "db_bytes_before": 1_000,
            "db_bytes_after": 1_100
        });
        let second = kv_row(1, "growth", 0, 2, 110.0);
        let (groups, problems) = summarize_rows(&[first, second], Some(2))?;
        assert!(problems.is_empty());
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].open_s_median, Some(0.25));
        assert_eq!(groups[0].settle_cpu_ns_median, Some(100.0));
        assert_eq!(groups[0].settle_write_bytes_median, Some(200.0));
        assert_eq!(groups[0].settle_db_delta_bytes_median, Some(100.0));
        Ok(())
    }

    #[test]
    fn expected_trial_mismatch_is_reported() -> Result<()> {
        let (groups, problems) = summarize_rows(&[kv_row(1, "growth", 0, 1, 100.0)], Some(3))?;
        assert_eq!(groups.len(), 1);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("1 trials, expected 3"));
        Ok(())
    }
}
