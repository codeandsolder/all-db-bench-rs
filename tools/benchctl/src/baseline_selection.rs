use crate::baseline_identity::{matches_trials, run_id};
use crate::baseline_sizing::TrialStatistics;
use crate::json_io::{read_json, read_ndjson};
use crate::json_value::json_truthy;
use crate::json_value::py_string;
use crate::summary;
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Component, Path, PathBuf},
};

const IDENTITY_FIELDS: [&str; 30] = [
    "lane",
    "scenario",
    "engine",
    "engine_version",
    "durability",
    "workload",
    "read_materialization",
    "write_materialization",
    "records",
    "clients",
    "value_bytes",
    "value_pattern",
    "key_bytes",
    "key_shape",
    "access_pattern",
    "miss_percent",
    "write_pattern",
    "txn_size",
    "scan_len",
    "settle_ms",
    "configuration",
    "payload_bytes",
    "payload_pattern",
    "engine_compression",
    "state_evolution",
    "bounded_churn_slots",
    "format_version",
    "seed",
    "warmup_reads",
    "durability_mapping",
];
const AUDIT_FIELDS: [&str; 8] = [
    "engine",
    "engine_version",
    "durability",
    "workload",
    "read_materialization",
    "write_materialization",
    "records",
    "ops_requested",
];
const FINAL_CONFIRMATION_KEY_FIELDS: [&str; 4] = ["engine", "durability", "workload", "records"];
const FINAL_CONFIRMATION_STRATEGIES: [&str; 5] = [
    "final-five-trial-common-work-v3",
    "final-common-work-v3",
    "final-five-trial-common-work-v4",
    "final-five-trial-common-work-v5",
    "final-five-trial-common-work-v6",
];

#[derive(Clone, Copy)]
pub struct SelectArgs<'a> {
    pub(crate) stock_results: &'a Path,
    pub(crate) audit: &'a Path,
    pub(crate) resize_root: &'a Path,
    pub(crate) ndjson_out: &'a Path,
    pub(crate) manifest_out: &'a Path,
    pub(crate) resize_prefix: &'a str,
    pub(crate) quality_repair_plan: Option<&'a Path>,
    pub(crate) allow_missing_resize: bool,
}

#[derive(Clone, Copy)]
pub enum FinalizeKind {
    Kv,
    Record,
}

impl FinalizeKind {
    const fn name(self) -> &'static str {
        match self {
            Self::Kv => "kv",
            Self::Record => "record",
        }
    }

    const fn expected_groups(self) -> usize {
        match self {
            Self::Kv => 180,
            Self::Record => 40,
        }
    }
}

#[derive(Clone, Copy)]
pub struct FinalizeInputs<'a> {
    pub(crate) stock_results: Option<&'a Path>,
    pub(crate) audit: Option<&'a Path>,
    pub(crate) resize_prefix: &'a str,
    pub(crate) out_dir: Option<&'a Path>,
    pub(crate) quality_repair_plan: Option<&'a Path>,
    pub(crate) skip_quality_repairs: bool,
}

#[derive(Clone, Copy)]
pub struct FinalizeLane<'a> {
    kind: FinalizeKind,
    stock_results: &'a Path,
    audit: &'a Path,
    resize_prefix: &'a str,
    out_dir: &'a Path,
    quality_repair_plan: Option<&'a Path>,
}

impl<'a> FinalizeLane<'a> {
    pub fn resolve(kind: FinalizeKind, inputs: FinalizeInputs<'a>) -> Result<Self> {
        let name = kind.name();
        Ok(Self {
            kind,
            stock_results: inputs.stock_results.with_context(|| {
                format!("--{name}-stock-results is required for the selected lane")
            })?,
            audit: inputs
                .audit
                .with_context(|| format!("--{name}-audit is required for the selected lane"))?,
            resize_prefix: inputs.resize_prefix,
            out_dir: inputs
                .out_dir
                .with_context(|| format!("--{name}-out-dir is required for the selected lane"))?,
            quality_repair_plan: if inputs.skip_quality_repairs {
                None
            } else {
                inputs.quality_repair_plan
            },
        })
    }
}

#[derive(Clone, Copy)]
struct SelectionSource<'a> {
    stock_results: &'a Path,
    audit: &'a Path,
    resize_root: &'a Path,
    resize_prefix: &'a str,
    quality_repair_plan: Option<&'a Path>,
    allow_missing_resize: bool,
}

fn value_i64(value: Option<&Value>) -> Result<i64> {
    match value.context("missing integer value")? {
        Value::Number(value) => value.as_i64().context("integer is outside i64 range"),
        Value::String(raw) => raw
            .parse::<i64>()
            .with_context(|| format!("invalid integer {raw:?}")),
        other => bail!("expected integer-compatible value, got {other}"),
    }
}

fn value_f64(value: Option<&Value>) -> Result<f64> {
    match value.context("missing numeric value")? {
        Value::Number(value) => value.as_f64().context("number is outside f64 range"),
        Value::String(raw) => raw
            .parse::<f64>()
            .with_context(|| format!("invalid number {raw:?}")),
        other => bail!("expected numeric-compatible value, got {other}"),
    }
}

fn identity_value(row: &Value, field: &str) -> Value {
    if let Some(value) = row.get(field) {
        return value.clone();
    }
    match field {
        "read_materialization" => json!("legacy-read-v0"),
        "write_materialization" => json!("legacy-return-v0"),
        _ => Value::Null,
    }
}

fn key_for(row: &Value, fields: &[&str]) -> Vec<Value> {
    fields
        .iter()
        .map(|field| identity_value(row, field))
        .collect()
}

fn identity(row: &Value) -> Vec<Value> {
    key_for(row, &IDENTITY_FIELDS)
}

fn audit_key(row: &Value) -> Vec<Value> {
    key_for(row, &AUDIT_FIELDS)
}

fn final_confirmation_key(row: &Value) -> Vec<Value> {
    key_for(row, &FINAL_CONFIRMATION_KEY_FIELDS)
}

fn encoded_key(key: &[Value]) -> Result<String> {
    serde_json::to_string(key).context("failed to encode baseline selection identity")
}

fn sort_key(key: &[Value]) -> Vec<String> {
    key.iter().map(py_string).collect()
}

fn semantic_manifest_fields(row: &Value) -> Map<String, Value> {
    let mut fields = Map::new();
    if row.get("lane").and_then(Value::as_str) == Some("record") {
        fields.insert(
            "read_materialization".to_owned(),
            identity_value(row, "read_materialization"),
        );
        fields.insert(
            "write_materialization".to_owned(),
            identity_value(row, "write_materialization"),
        );
    }
    fields
}

fn rejected_pressure_attempts(run_dir: &Path) -> Result<i64> {
    let root = run_dir.join("rejected-pressure");
    if !root.is_dir() {
        return Ok(0);
    }
    let mut count = 0_i64;
    for case in fs::read_dir(&root).with_context(|| format!("failed to read {}", root.display()))? {
        let case = case?.path();
        if !case.is_dir() {
            continue;
        }
        for attempt in
            fs::read_dir(&case).with_context(|| format!("failed to read {}", case.display()))?
        {
            let attempt = attempt?.path();
            let name = attempt
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or_default();
            if attempt.is_dir()
                && name.starts_with("attempt-")
                && attempt.join("rejection.json").is_file()
            {
                count += 1;
            }
        }
    }
    Ok(count)
}

fn validate_trials(rows: &[Value], label: &str, expected_count: i64) -> Result<()> {
    let mut trials = rows
        .iter()
        .map(|row| value_i64(row.get("trial")))
        .collect::<Result<Vec<_>>>()?;
    trials.sort_unstable();
    if !matches_trials(&trials, expected_count) {
        bail!("{label}: expected trials 1..={expected_count}, got {trials:?}");
    }
    Ok(())
}

pub fn final_quality(rows: &[Value], thresholds: &Value) -> Result<Value> {
    let evidence = TrialStatistics::from_rows(&rows.iter().collect::<Vec<_>>())?;
    let median_elapsed = evidence.median_elapsed;
    let total_elapsed = evidence.measured_total;
    let throughput_cv = evidence.throughput_cv;
    let relative_spread = evidence.throughput_relative_spread;
    let workload = rows
        .first()
        .and_then(|row| row.get("workload"))
        .map_or_else(|| "None".to_owned(), py_string);
    let read_only = thresholds
        .get("read_only_workloads")
        .and_then(Value::as_array)
        .context("missing read_only_workloads")?
        .iter()
        .any(|value| value.as_str() == Some(workload.as_str()));
    let undersized = if read_only {
        median_elapsed < value_f64(thresholds.get("read_only_min_seconds"))?
    } else {
        total_elapsed < value_f64(thresholds.get("stateful_min_total_seconds"))?
    };
    let variable = throughput_cv > value_f64(thresholds.get("max_cv"))?
        || relative_spread > value_f64(thresholds.get("max_relative_spread"))?;
    let status = if undersized {
        "undersized"
    } else if variable {
        "variable"
    } else {
        "accepted"
    };
    Ok(json!({
        "final_status": status,
        "median_elapsed_s": median_elapsed,
        "total_elapsed_s": total_elapsed,
        "throughput_cv": throughput_cv,
        "throughput_relative_spread": relative_spread,
    }))
}

#[derive(Clone, Copy)]
struct SourceSpec<'a> {
    source: &'a str,
    resize_strategy: Option<&'a str>,
    run_id: Option<&'a str>,
    ops_requested: i64,
    trials: i64,
    rejected: Option<i64>,
}

fn source_manifest(first: &Value, spec: SourceSpec<'_>, quality: &Value) -> Result<Value> {
    let mut object = Map::new();
    object.insert(
        "engine".to_owned(),
        first.get("engine").cloned().unwrap_or(Value::Null),
    );
    object.insert(
        "durability".to_owned(),
        first.get("durability").cloned().unwrap_or(Value::Null),
    );
    object.insert(
        "workload".to_owned(),
        first.get("workload").cloned().unwrap_or(Value::Null),
    );
    object.extend(semantic_manifest_fields(first));
    object.insert("source".to_owned(), json!(spec.source));
    if let Some(strategy) = spec.resize_strategy {
        object.insert("resize_strategy".to_owned(), json!(strategy));
    }
    if let Some(run_id) = spec.run_id {
        object.insert("run_id".to_owned(), json!(run_id));
    }
    object.insert("ops_requested".to_owned(), json!(spec.ops_requested));
    object.insert("trials".to_owned(), json!(spec.trials));
    if let Some(rejected) = spec.rejected {
        object.insert("rejected_pressure_attempts".to_owned(), json!(rejected));
    }
    for key in [
        "final_status",
        "median_elapsed_s",
        "total_elapsed_s",
        "throughput_cv",
        "throughput_relative_spread",
    ] {
        object.insert(
            key.to_owned(),
            quality.get(key).cloned().context("quality field missing")?,
        );
    }
    Ok(Value::Object(object))
}

struct StockGroup {
    key: Vec<Value>,
    rows: Vec<Value>,
}
fn stock_groups(stock_rows: &[Value]) -> Result<Vec<StockGroup>> {
    let mut groups = HashMap::<String, StockGroup>::new();
    for row in stock_rows {
        let key = identity(row);
        groups
            .entry(encoded_key(&key)?)
            .or_insert_with(|| StockGroup {
                key,
                rows: Vec::new(),
            })
            .rows
            .push(row.clone());
    }
    let mut ordered = groups.into_values().collect::<Vec<_>>();
    ordered.sort_by_key(|group| sort_key(&group.key));
    Ok(ordered)
}

fn index_groups<'a>(
    groups: &'a Value,
    key_for: fn(&Value) -> Vec<Value>,
    label: &str,
) -> Result<HashMap<String, &'a Value>> {
    let mut index = HashMap::new();
    for group in groups
        .get("groups")
        .and_then(Value::as_array)
        .with_context(|| format!("{label} groups missing"))?
    {
        let key = key_for(group);
        if index.insert(encoded_key(&key)?, group).is_some() {
            bail!("duplicate {label} identity: {key:?}");
        }
    }
    Ok(index)
}

fn validate_ops(rows: &[Value], expected: i64, label: &str) -> Result<()> {
    if expected <= 0 {
        bail!("{label}: operation count must be positive");
    }
    for row in rows {
        if value_i64(row.get("ops_requested"))? != expected {
            bail!("{label}: operation count mismatch, expected {expected}");
        }
        if let Some(completed) = row.get("ops_completed")
            && value_i64(Some(completed))? != expected
        {
            bail!("{label}: incomplete operations");
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Selection<'a> {
    Stock,
    Resize(&'a Value),
    QualityRepair(&'a Value),
}

impl Selection<'_> {
    const fn source(self) -> &'static str {
        match self {
            Self::Stock => "stock",
            Self::Resize(_) => "resize",
            Self::QualityRepair(_) => "quality-repair",
        }
    }
}

struct SelectedGroup {
    rows: Vec<Value>,
    manifest: Value,
}

fn select_stock(
    stock: StockGroup,
    group: &Value,
    thresholds: &Value,
    trials: i64,
) -> Result<SelectedGroup> {
    let first = stock.rows.first().context("empty stock group")?;
    let quality = final_quality(&stock.rows, thresholds)?;
    let final_status = quality.get("final_status").and_then(Value::as_str);
    let audit_status = group.get("status").and_then(Value::as_str);
    if final_status != audit_status {
        bail!(
            "stock quality no longer matches audit: audit={audit_status:?} recomputed={final_status:?}"
        );
    }
    let manifest = source_manifest(
        first,
        SourceSpec {
            source: "stock",
            resize_strategy: None,
            run_id: None,
            ops_requested: value_i64(first.get("ops_requested"))?,
            trials,
            rejected: None,
        },
        &quality,
    )?;
    Ok(SelectedGroup {
        rows: stock.rows,
        manifest,
    })
}

fn select_followup(
    stock: &StockGroup,
    selection: Selection<'_>,
    run_dir: &Path,
    rid: &str,
    thresholds: &Value,
) -> Result<SelectedGroup> {
    let (group, strategy) = match selection {
        Selection::Resize(group) => (group, group.get("resize_strategy").and_then(Value::as_str)),
        Selection::QualityRepair(group) => (group, Some("quality-repair")),
        Selection::Stock => bail!("stock is not a followup"),
    };
    let source = selection.source();
    let expected_trials = value_i64(group.get("suggested_trials"))?;
    let run_summary = read_json(&run_dir.join("summary.json"))?;
    if value_i64(run_summary.get("row_count"))? != expected_trials
        || value_i64(run_summary.get("group_count"))? != 1
        || json_truthy(run_summary.get("problems"))
    {
        bail!("invalid {source} summary: {rid}");
    }
    let rows = read_ndjson(&run_dir.join("results.ndjson"))?;
    validate_trials(&rows, rid, expected_trials)?;
    if rows.iter().any(|row| identity(row) != stock.key) {
        bail!("{source} identity differs from stock group: {rid}");
    }
    let expected_ops = value_i64(group.get("suggested_effective_ops"))?;
    validate_ops(&rows, expected_ops, rid)?;
    let quality = final_quality(&rows, thresholds)?;
    if quality.get("final_status").and_then(Value::as_str) == Some("undersized") {
        bail!("{source} result remains undersized: {rid}");
    }
    let first = stock.rows.first().context("empty stock group")?;
    let manifest = source_manifest(
        first,
        SourceSpec {
            source,
            resize_strategy: strategy,
            run_id: Some(rid),
            ops_requested: expected_ops,
            trials: expected_trials,
            rejected: Some(rejected_pressure_attempts(run_dir)?),
        },
        &quality,
    )?;
    Ok(SelectedGroup { rows, manifest })
}

struct SelectionPlan<'a> {
    audit: HashMap<String, &'a Value>,
    quality: HashMap<String, &'a Value>,
    confirmation: bool,
    matched_confirmation: HashSet<String>,
}

impl<'a> SelectionPlan<'a> {
    fn new(audit: &'a Value, quality: Option<&'a Value>) -> Result<Self> {
        let audit = index_groups(audit, audit_key, "audit")?;
        let confirmation = quality
            .and_then(|plan| plan.get("strategy"))
            .and_then(Value::as_str)
            .is_some_and(|strategy| FINAL_CONFIRMATION_STRATEGIES.contains(&strategy));
        let quality = if let Some(plan) = quality {
            if value_i64(plan.get("quality_policy_version"))? != 1 {
                bail!(
                    "unsupported stock quality policy: {:?}",
                    plan.get("quality_policy_version")
                );
            }
            index_groups(
                plan,
                if confirmation {
                    final_confirmation_key
                } else {
                    audit_key
                },
                "quality-repair",
            )?
        } else {
            HashMap::new()
        };
        Ok(Self {
            audit,
            quality,
            confirmation,
            matched_confirmation: HashSet::new(),
        })
    }

    fn take(&mut self, row: &Value) -> Result<(Selection<'a>, &'a Value)> {
        let key = audit_key(row);
        let audit = self
            .audit
            .remove(&encoded_key(&key)?)
            .with_context(|| format!("stock group missing or ambiguous in audit: {key:?}"))?;
        let quality_key = if self.confirmation {
            final_confirmation_key(row)
        } else {
            key
        };
        let quality_id = encoded_key(&quality_key)?;
        if self.matched_confirmation.contains(&quality_id) {
            bail!("ambiguous final-confirmation stock identity: {quality_key:?}");
        }
        let selection = if let Some(repair) = self.quality.remove(&quality_id) {
            if self.confirmation {
                self.matched_confirmation.insert(quality_id);
            }
            Selection::QualityRepair(repair)
        } else if audit.get("status").and_then(Value::as_str) == Some("undersized") {
            Selection::Resize(audit)
        } else {
            Selection::Stock
        };
        Ok((selection, audit))
    }
}

pub fn select_rows(
    stock_rows: &[Value],
    audit: &Value,
    resize_root: &Path,
    resize_prefix: &str,
    allow_missing_resize: bool,
    quality_repair_plan: Option<&Value>,
) -> Result<(Vec<Value>, Value)> {
    let policy_version = value_i64(audit.get("sizing_policy_version"))?;
    if !matches!(policy_version, 2 | 3) {
        bail!(
            "unsupported sizing policy: {:?}",
            audit.get("sizing_policy_version").unwrap_or(&Value::Null)
        );
    }
    if policy_version >= 3
        && stock_rows.iter().any(|row| {
            row.get("lane").and_then(Value::as_str) == Some("record")
                && (row
                    .get("read_materialization")
                    .and_then(Value::as_str)
                    .is_none()
                    || row
                        .get("write_materialization")
                        .and_then(Value::as_str)
                        .is_none())
        })
    {
        bail!("sizing policy v3 record rows require explicit read/write materialization");
    }
    let thresholds = audit
        .get("thresholds")
        .context("sizing audit thresholds missing")?;
    let stock_trial_count = thresholds
        .get("expect_trials")
        .map_or(Ok(3), |value| value_i64(Some(value)))?;

    let ordered_stock = stock_groups(stock_rows)?;
    let stock_group_count = i64::try_from(ordered_stock.len())?;
    let mut plan = SelectionPlan::new(audit, quality_repair_plan)?;
    let expected_audit_groups = audit.get("group_count").map_or_else(
        || i64::try_from(plan.audit.len()).context("too many audit groups"),
        |value| value_i64(Some(value)),
    )?;
    if stock_group_count != expected_audit_groups || ordered_stock.len() != plan.audit.len() {
        bail!(
            "stock/audit group count mismatch: stock={stock_group_count} audit={expected_audit_groups}"
        );
    }
    let mut selected = SelectionResults::default();
    let mut run_ids = HashSet::new();
    for stock in ordered_stock {
        let first = stock.rows.first().context("empty stock group")?;
        validate_trials(&stock.rows, "stock", stock_trial_count)?;
        validate_ops(&stock.rows, value_i64(first.get("ops_requested"))?, "stock")?;
        let (selection, audit_group) = plan.take(first)?;
        let result = match selection {
            Selection::Stock => {
                selected.stock += 1;
                select_stock(stock, audit_group, thresholds, stock_trial_count)?
            }
            Selection::Resize(group) | Selection::QualityRepair(group) => {
                let rid = run_id(group, resize_prefix)?;
                if !run_ids.insert(rid.clone()) {
                    bail!("ambiguous followup run ID: {rid}");
                }
                let run_dir = resize_root.join(&rid);
                if !run_dir.join("results.ndjson").is_file()
                    || !run_dir.join("summary.json").is_file()
                {
                    if !allow_missing_resize {
                        bail!("missing {} result: {rid}", selection.source());
                    }
                    selected.pending.push(rid);
                    continue;
                }
                match selection {
                    Selection::Resize(_) => selected.resize += 1,
                    Selection::QualityRepair(_) => selected.quality_repair += 1,
                    Selection::Stock => {}
                }
                select_followup(&stock, selection, &run_dir, &rid, thresholds)?
            }
        };
        selected.rows.extend(result.rows);
        selected.sources.push(result.manifest);
    }
    if !plan.audit.is_empty() || !plan.quality.is_empty() {
        bail!(
            "unmatched selection plan groups: audit={}, quality={}",
            plan.audit.len(),
            plan.quality.len()
        );
    }
    Ok(selected.finish(
        audit.get("sizing_policy_version").unwrap_or(&Value::Null),
        stock_rows.len(),
        stock_group_count,
    ))
}

#[derive(Default)]
struct SelectionResults {
    rows: Vec<Value>,
    sources: Vec<Value>,
    pending: Vec<String>,
    stock: usize,
    resize: usize,
    quality_repair: usize,
}

impl SelectionResults {
    fn finish(
        self,
        policy_version: &Value,
        stock_rows: usize,
        stock_group_count: i64,
    ) -> (Vec<Value>, Value) {
        let status_count = |status: &str| -> i64 {
            i64::try_from(
                self.sources
                    .iter()
                    .filter(|source| {
                        source.get("final_status").and_then(Value::as_str) == Some(status)
                    })
                    .count(),
            )
            .unwrap_or(i64::MAX)
        };
        let rejected_total = self
            .sources
            .iter()
            .map(|source| {
                source
                    .get("rejected_pressure_attempts")
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
            })
            .sum::<i64>();
        let manifest = json!({
            "sizing_policy_version": policy_version,
            "stock_rows": stock_rows,
            "stock_groups": stock_group_count,
            "selected_rows": self.rows.len(),
            "selected_groups": self.sources.len(),
            "selected_stock_groups": self.stock,
            "selected_resize_groups": self.resize,
            "selected_quality_repair_groups": self.quality_repair,
            "pending_resize_groups": self.pending.len(),
            "pending_run_ids": self.pending,
            "complete": self.pending.is_empty(),
            "final_status_counts": {
                "accepted": status_count("accepted"),
                "variable": status_count("variable"),
                "undersized": status_count("undersized"),
            },
            "selected_followup_rejected_pressure_attempts": rejected_total,
            "sources": self.sources,
        });
        (self.rows, manifest)
    }
}

fn write_selection(
    rows: &[Value],
    manifest: &Value,
    ndjson_out: &Path,
    manifest_out: &Path,
) -> Result<()> {
    if let Some(parent) = ndjson_out.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut ndjson = String::new();
    for row in rows {
        ndjson.push_str(&serde_json::to_string(row)?);
        ndjson.push('\n');
    }
    fs::write(ndjson_out, ndjson)
        .with_context(|| format!("failed to write {}", ndjson_out.display()))?;
    if let Some(parent) = manifest_out.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(manifest_out, serde_json::to_string_pretty(manifest)? + "\n")
        .with_context(|| format!("failed to write {}", manifest_out.display()))?;
    Ok(())
}

fn read_selection(source: SelectionSource<'_>) -> Result<(Vec<Value>, Value)> {
    let stock_rows = read_ndjson(source.stock_results)?;
    let audit = read_json(source.audit)?;
    let quality = source.quality_repair_plan.map(read_json).transpose()?;
    let (rows, mut manifest) = select_rows(
        &stock_rows,
        &audit,
        source.resize_root,
        source.resize_prefix,
        source.allow_missing_resize,
        quality.as_ref(),
    )?;
    manifest
        .as_object_mut()
        .context("selection manifest is not an object")?
        .insert(
            "stock_rejected_pressure_attempts".to_owned(),
            json!(rejected_pressure_attempts(
                source
                    .stock_results
                    .parent()
                    .context("stock results have no parent")?
            )?),
        );
    Ok((rows, manifest))
}

pub fn run_select(args: SelectArgs<'_>) -> Result<()> {
    let (rows, manifest) = read_selection(SelectionSource {
        stock_results: args.stock_results,
        audit: args.audit,
        resize_root: args.resize_root,
        resize_prefix: args.resize_prefix,
        quality_repair_plan: args.quality_repair_plan,
        allow_missing_resize: args.allow_missing_resize,
    })?;
    write_selection(&rows, &manifest, args.ndjson_out, args.manifest_out)?;
    println!(
        "{{\"complete\": {}, \"pending_resize_groups\": {}, \"selected_groups\": {}, \"selected_quality_repair_groups\": {}, \"selected_resize_groups\": {}, \"selected_rows\": {}, \"selected_stock_groups\": {}}}",
        manifest
            .get("complete")
            .context("manifest complete missing")?,
        manifest
            .get("pending_resize_groups")
            .context("manifest pending count missing")?,
        manifest
            .get("selected_groups")
            .context("manifest group count missing")?,
        manifest
            .get("selected_quality_repair_groups")
            .context("manifest quality count missing")?,
        manifest
            .get("selected_resize_groups")
            .context("manifest resize count missing")?,
        manifest
            .get("selected_rows")
            .context("manifest row count missing")?,
        manifest
            .get("selected_stock_groups")
            .context("manifest stock count missing")?,
    );
    Ok(())
}

struct PreparedFinalization<'a> {
    lane: FinalizeLane<'a>,
    rows: Vec<Value>,
    manifest: Value,
    report: summary::SummaryReport,
}

impl<'a> PreparedFinalization<'a> {
    fn prepare(repo: &Path, lane: FinalizeLane<'a>) -> Result<Self> {
        let (rows, manifest) = read_selection(SelectionSource {
            stock_results: lane.stock_results,
            audit: lane.audit,
            resize_root: &repo.join("results/runs"),
            resize_prefix: lane.resize_prefix,
            quality_repair_plan: lane.quality_repair_plan,
            allow_missing_resize: false,
        })?;
        let expected_groups = lane.kind.expected_groups();
        if manifest.get("complete").and_then(Value::as_bool) != Some(true) {
            bail!("selection remained incomplete: {}", lane.out_dir.display());
        }
        let selected_rows = usize::try_from(value_i64(manifest.get("selected_rows"))?)?;
        if usize::try_from(value_i64(manifest.get("selected_groups"))?)? != expected_groups {
            bail!("selection group-count mismatch: {}", lane.out_dir.display());
        }
        let report =
            summary::SummaryReport::from_rows(&lane.out_dir.join("results.ndjson"), &rows, None)?;
        report.validate_corpus(lane.kind.name(), selected_rows, expected_groups)?;
        Ok(Self {
            lane,
            rows,
            manifest,
            report,
        })
    }

    fn publish(&self) -> Result<String> {
        let path = self.lane.out_dir;
        fs::create_dir_all(path).with_context(|| format!("failed to create {}", path.display()))?;
        write_selection(
            &self.rows,
            &self.manifest,
            &path.join("results.ndjson"),
            &path.join("manifest.json"),
        )?;
        self.report.write(
            Some(&path.join("summary.json")),
            Some(&path.join("summary.md")),
        )?;
        Ok(format!(
            "{}={} groups",
            self.lane.kind.name(),
            self.lane.kind.expected_groups()
        ))
    }
}

/// Resolve existing symlinks and normalize the uncreated suffix without
/// touching the filesystem. Relative and absolute spellings of an output
/// directory must not allow two lanes to publish into the same corpus.
fn output_identity(path: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => resolved.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(name) => {
                resolved.push(name);
                match fs::symlink_metadata(&resolved) {
                    Ok(_) => {
                        resolved = resolved.canonicalize().with_context(|| {
                            format!("resolve output directory {}", resolved.display())
                        })?;
                        if !resolved.is_dir() {
                            bail!("output parent is not a directory: {}", resolved.display());
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("inspect output directory {}", resolved.display())
                        });
                    }
                }
            }
        }
    }
    Ok(resolved)
}

pub fn finalize_lanes(repo: &Path, lanes: Vec<FinalizeLane<'_>>) -> Result<()> {
    if lanes.is_empty() {
        bail!("no baseline lanes selected");
    }
    let mut destinations = HashSet::new();
    for lane in &lanes {
        if !destinations.insert(output_identity(lane.out_dir)?) {
            bail!("baseline lanes must have distinct output directories");
        }
    }
    // Complete all source/selection/summary validation before touching outputs.
    let prepared = lanes
        .into_iter()
        .map(|lane| PreparedFinalization::prepare(repo, lane))
        .collect::<Result<Vec<_>>>()?;
    let labels = prepared
        .iter()
        .map(PreparedFinalization::publish)
        .collect::<Result<Vec<_>>>()?;
    println!("final baseline corpora validated: {}", labels.join(" "));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        FinalizeInputs, FinalizeKind, FinalizeLane, final_quality, finalize_lanes, run_id,
        select_rows,
    };
    use anyhow::{Context, Result};
    use serde_json::{Value, json};
    use std::{fs, path::Path};

    fn row(engine: &str, workload: &str, trial: i64, ops: i64, elapsed: f64, rate: f64) -> Value {
        json!({
            "lane":"kv","scenario":"baseline-core","engine":engine,"engine_version":"1.0",
            "durability":"relaxed","workload":workload,"records":100,"ops_requested":ops,
            "clients":1,"value_bytes":256,"value_pattern":"pseudo-random","key_bytes":8,
            "key_shape":"sequential","access_pattern":"auto","miss_percent":0,"write_pattern":"append",
            "txn_size":100,"scan_len":100,"settle_ms":0,"configuration":"fixed","payload_pattern":"hex-random",
            "trial":trial,"elapsed_s":elapsed,"ops_per_s":rate
        })
    }

    fn audit_group(
        engine: &str,
        workload: &str,
        ops: i64,
        status: &str,
        suggested: Option<i64>,
        trials: Option<i64>,
        strategy: Option<&str>,
    ) -> Value {
        json!({
            "engine":engine,"engine_version":"1.0","durability":"relaxed","workload":workload,
            "records":100,"ops_requested":ops,"status":status,"resize_strategy":strategy,
            "suggested_effective_ops":suggested,"suggested_trials":trials
        })
    }

    fn audit(groups: &[Value]) -> Value {
        json!({
            "sizing_policy_version":2,"group_count":groups.len(),
            "thresholds":{"expect_trials":3,"max_cv":0.10,"max_relative_spread":0.25,
                "read_only_min_seconds":0.5,"read_only_workloads":["indexed-read","point-read","range-scan"],
                "stateful_min_total_seconds":1.0},"groups":groups
        })
    }

    fn temp_root(name: &str) -> Result<std::path::PathBuf> {
        let root = std::env::temp_dir().join(format!(
            "all-db-bench-selection-{name}-{}",
            std::process::id()
        ));
        if root.exists() {
            fs::remove_dir_all(&root)?;
        }
        fs::create_dir_all(&root)?;
        Ok(root)
    }

    fn write_finalization_fixture(
        root: &Path,
        kind: FinalizeKind,
        valid_summary: bool,
    ) -> Result<()> {
        let name = kind.name();
        let mut stock = String::new();
        let mut groups = Vec::new();
        for group in 0..kind.expected_groups() {
            let engine = format!("engine-{group}");
            groups.push(audit_group(
                &engine,
                "point-read",
                100,
                "accepted",
                None,
                None,
                None,
            ));
            for trial in 1..=3 {
                let mut row = row(&engine, "point-read", trial, 100, 1.0, 100.0);
                row["lane"] = json!(name);
                row["ops_completed"] = json!(100);
                row["durability_mapping"] = json!("test");
                if valid_summary {
                    row["db_bytes"] = json!(4096);
                }
                stock.push_str(&serde_json::to_string(&row)?);
                stock.push('\n');
            }
        }
        fs::write(root.join(format!("{name}-stock.ndjson")), stock)?;
        fs::write(
            root.join(format!("{name}-audit.json")),
            serde_json::to_string(&audit(&groups))?,
        )?;
        Ok(())
    }

    fn fixture_lane<'a>(
        kind: FinalizeKind,
        stock: &'a Path,
        audit: &'a Path,
        out: &'a Path,
    ) -> Result<FinalizeLane<'a>> {
        FinalizeLane::resolve(
            kind,
            FinalizeInputs {
                stock_results: Some(stock),
                audit: Some(audit),
                resize_prefix: "unused",
                out_dir: Some(out),
                quality_repair_plan: None,
                skip_quality_repairs: false,
            },
        )
    }

    #[test]
    fn finalization_validates_all_lanes_before_replacing_any_output() -> Result<()> {
        let root = temp_root("finalization-preflight")?;
        write_finalization_fixture(&root, FinalizeKind::Kv, true)?;
        write_finalization_fixture(&root, FinalizeKind::Record, false)?;
        let kv_out = root.join("kv");
        let record_out = root.join("record");
        fs::create_dir(&kv_out)?;
        fs::write(kv_out.join("results.ndjson"), "previous corpus")?;
        let kv_stock = root.join("kv-stock.ndjson");
        let kv_audit = root.join("kv-audit.json");
        let record_stock = root.join("record-stock.ndjson");
        let record_audit = root.join("record-audit.json");
        let kv = fixture_lane(FinalizeKind::Kv, &kv_stock, &kv_audit, &kv_out)?;
        let record = fixture_lane(
            FinalizeKind::Record,
            &record_stock,
            &record_audit,
            &record_out,
        )?;
        assert!(finalize_lanes(&root, vec![kv, record]).is_err());
        assert_eq!(
            fs::read_to_string(kv_out.join("results.ndjson"))?,
            "previous corpus"
        );
        assert!(!kv_out.join("manifest.json").exists());
        assert!(!record_out.exists());

        write_finalization_fixture(&root, FinalizeKind::Record, true)?;
        finalize_lanes(&root, vec![kv, record])?;
        for (path, groups) in [(&kv_out, 180), (&record_out, 40)] {
            let summary: Value =
                serde_json::from_str(&fs::read_to_string(path.join("summary.json"))?)?;
            assert_eq!(summary["group_count"], groups);
            assert_eq!(summary["row_count"], groups * 3);
            assert_eq!(summary["problems"], json!([]));
            assert!(path.join("manifest.json").is_file());
            assert!(path.join("summary.md").is_file());
        }
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn finalization_rejects_missing_inputs_and_duplicate_destinations() -> Result<()> {
        let root = temp_root("finalization-resolve")?;
        let stock = root.join("stock");
        let audit = root.join("audit");
        let out = root.join("out");
        for (stock_results, audit_input, missing) in [
            (None, Some(audit.as_path()), "stock-results"),
            (Some(stock.as_path()), None, "audit"),
        ] {
            let error = FinalizeLane::resolve(
                FinalizeKind::Record,
                FinalizeInputs {
                    stock_results,
                    audit: audit_input,
                    resize_prefix: "unused",
                    out_dir: Some(&out),
                    quality_repair_plan: None,
                    skip_quality_repairs: false,
                },
            )
            .err()
            .context("expected missing input rejection")?;
            assert_eq!(
                error.to_string(),
                format!("--record-{missing} is required for the selected lane")
            );
        }
        let kv = fixture_lane(FinalizeKind::Kv, &stock, &audit, &out)?;
        let alias = root.join("not-created/../out");
        let record = fixture_lane(FinalizeKind::Record, &stock, &audit, &alias)?;
        let error = finalize_lanes(&root, vec![kv, record])
            .err()
            .context("expected alias rejection")?;
        assert!(error.to_string().contains("distinct output directories"));
        assert!(!out.exists());
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn run_id_accepts_record_prefix_and_trial_count() -> Result<()> {
        let target = audit_group(
            "sqlite",
            "tiny-txn",
            5_000,
            "undersized",
            Some(5_000),
            Some(11),
            Some("more-trials"),
        );
        assert_eq!(
            run_id(&target, "20261006-record-resize-v4")?,
            "20261006-record-resize-v4-sqlite-relaxed-tiny-txn-e5000-t11"
        );
        Ok(())
    }

    #[test]
    fn final_quality_matches_sampling_policy() -> Result<()> {
        let thresholds = json!({"max_cv":0.10,"max_relative_spread":0.25,"read_only_min_seconds":0.5,
            "read_only_workloads":["indexed-read","point-read","range-scan"],"stateful_min_total_seconds":1.0});
        let rows = (1..=5)
            .zip([100.0, 100.0, 100.0, 100.0, 40.0])
            .map(|(trial, rate)| row("x", "tiny-txn", trial, 100, 0.2, rate))
            .collect::<Vec<_>>();
        assert_eq!(
            final_quality(&rows, &thresholds)?["final_status"],
            "variable"
        );
        let read = (1..=3)
            .map(|trial| row("x", "point-read", trial, 100, 0.2, 100.0))
            .collect::<Vec<_>>();
        assert_eq!(
            final_quality(&read, &thresholds)?["final_status"],
            "undersized"
        );
        Ok(())
    }

    #[test]
    fn replaces_stateful_group_with_more_trials_same_ops() -> Result<()> {
        let root = temp_root("resize")?;
        let stock = (1..=3)
            .map(|t| row("a", "point-read", t, 100, 1.0, 100.0))
            .chain((1..=3).map(|t| row("b", "tiny-txn", t, 100, 1.0, 100.0)))
            .collect::<Vec<_>>();
        let target = audit_group(
            "b",
            "tiny-txn",
            100,
            "undersized",
            Some(100),
            Some(5),
            Some("more-trials"),
        );
        let plan = audit(&[
            audit_group("a", "point-read", 100, "accepted", None, None, None),
            target.clone(),
        ]);
        let run = root.join(run_id(&target, "resize")?);
        fs::create_dir_all(&run)?;
        let resized = (1..=5)
            .map(|t| row("b", "tiny-txn", t, 100, 0.25, 100.0))
            .collect::<Vec<_>>();
        fs::write(
            run.join("results.ndjson"),
            resized
                .iter()
                .map(|v| serde_json::to_string(v).map(|s| s + "\n"))
                .collect::<Result<String, _>>()?,
        )?;
        fs::write(
            run.join("summary.json"),
            serde_json::to_string(&json!({"row_count":5,"group_count":1,"problems":[]}))?,
        )?;
        let (selected, manifest) = select_rows(&stock, &plan, &root, "resize", false, None)?;
        assert_eq!(selected.len(), 8);
        assert_eq!(manifest["selected_resize_groups"], 1);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn selection_rejects_duplicate_or_unmatched_plans_and_inconsistent_stock() -> Result<()> {
        let root = temp_root("ambiguous")?;
        let stock = (1..=3)
            .map(|t| row("a", "point-read", t, 100, 1.0, 100.0))
            .collect::<Vec<_>>();
        let group = audit_group("a", "point-read", 100, "accepted", None, None, None);
        let valid = audit(std::slice::from_ref(&group));
        assert!(select_rows(&stock, &valid, &root, "resize", false, None).is_ok());
        let mut duplicate = audit(&[group.clone(), group]);
        duplicate["group_count"] = json!(1);
        assert!(select_rows(&stock, &duplicate, &root, "resize", false, None).is_err());
        let orphan = audit_group("b", "point-read", 100, "accepted", None, None, None);
        let quality = json!({"quality_policy_version":1,"groups":[orphan]});
        assert!(select_rows(&stock, &valid, &root, "resize", false, Some(&quality)).is_err());
        for (key, value) in [
            ("ops_requested", json!(99)),
            ("ops_completed", json!(99)),
            ("elapsed_s", json!("NaN")),
            ("payload_bytes", json!(1024)),
            ("engine_compression", json!("changed")),
            ("trial", json!(2)),
        ] {
            let mut invalid = stock.clone();
            invalid[0][key] = value;
            assert!(
                select_rows(&invalid, &valid, &root, "resize", false, None).is_err(),
                "{key}"
            );
        }
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn final_confirmation_can_override_ops() -> Result<()> {
        let root = temp_root("confirmation")?;
        let stock = (1..=3)
            .map(|t| row("sqlite", "point-read", t, 100, 0.2, 500.0))
            .collect::<Vec<_>>();
        let target = audit_group(
            "sqlite",
            "point-read",
            100,
            "undersized",
            Some(300),
            Some(3),
            Some("more-ops"),
        );
        let plan = audit(std::slice::from_ref(&target));
        let mut confirmation = target;
        confirmation["suggested_effective_ops"] = json!(200);
        confirmation["suggested_trials"] = json!(5);
        confirmation["resize_strategy"] = json!("quality-repair");
        let quality = json!({"quality_policy_version":1,"strategy":"final-five-trial-common-work-v3","groups":[confirmation.clone()]});
        let run = root.join(run_id(&confirmation, "final")?);
        fs::create_dir_all(&run)?;
        let repaired = (1..=5)
            .map(|t| row("sqlite", "point-read", t, 200, 1.0, 200.0 + int_as_f64!(t)))
            .collect::<Vec<_>>();
        fs::write(
            run.join("results.ndjson"),
            repaired
                .iter()
                .map(|v| serde_json::to_string(v).map(|s| s + "\n"))
                .collect::<Result<String, _>>()?,
        )?;
        fs::write(
            run.join("summary.json"),
            serde_json::to_string(&json!({"row_count":5,"group_count":1,"problems":[]}))?,
        )?;
        let (selected, manifest) =
            select_rows(&stock, &plan, &root, "final", false, Some(&quality))?;
        assert_eq!(selected.len(), 5);
        assert_eq!(manifest["selected_quality_repair_groups"], 1);
        fs::remove_dir_all(root)?;
        Ok(())
    }
}
