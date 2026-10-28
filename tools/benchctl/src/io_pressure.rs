use crate::io_evidence::{FioExpectation, fio_p99_us, load_storage_delta, ratio, validate_fio_job};
use crate::json_io::read_json;
use crate::json_value::py_string;
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use std::{collections::BTreeMap, fs, path::Path};

pub(crate) struct IoPressureArgs<'a> {
    pub(crate) run_dir: &'a Path,
    pub(crate) expect_trials: Option<usize>,
    pub(crate) json_out: Option<&'a Path>,
    pub(crate) markdown_out: Option<&'a Path>,
}

const AGGREGATE_FIELDS: [&str; 47] = [
    "pressure_target_iops",
    "pressure_target_read_iops",
    "pressure_target_write_iops",
    "pressure_delivered_iops",
    "pressure_delivered_read_iops",
    "pressure_delivered_write_iops",
    "pressure_delivered_bw_mbps",
    "pressure_delivered_vs_target",
    "pressure_delivered_read_vs_target",
    "pressure_delivered_write_vs_target",
    "pressure_delivered_vs_baseline",
    "pressure_read_p99_us",
    "pressure_write_p99_us",
    "db_ops_per_s",
    "db_throughput_ratio_vs_zero",
    "db_read_p99_us",
    "db_read_p99_ratio_vs_zero",
    "db_write_p99_us",
    "db_write_p99_ratio_vs_zero",
    "db_cpu_ns_per_op",
    "db_runqueue_wait_fraction",
    "db_io_psi_full_fraction",
    "storage_accounting_s",
    "storage_read_ios",
    "storage_write_ios",
    "storage_read_bytes",
    "storage_write_bytes",
    "storage_discard_bytes",
    "storage_flushes",
    "storage_io_ms",
    "storage_weighted_io_ms",
    "zfs_arc_hits",
    "zfs_arc_misses",
    "storage_read_mbps",
    "storage_write_mbps",
    "zfs_arc_hit_fraction",
    "zfs_direct_read_ios",
    "zfs_direct_read_bytes",
    "zfs_direct_write_ios",
    "zfs_direct_write_bytes",
    "pressure_storage_accounting_s",
    "pressure_zfs_direct_read_ios",
    "pressure_zfs_direct_read_bytes",
    "pressure_zfs_direct_write_ios",
    "pressure_zfs_direct_write_bytes",
    "zfs_direct_read_bytes_per_pressure_read_byte",
    "zfs_direct_write_bytes_per_pressure_write_byte",
];

fn as_f64(value: Option<&Value>, default: f64) -> f64 {
    value.and_then(Value::as_f64).unwrap_or(default)
}

fn as_i64(value: Option<&Value>, default: i64) -> i64 {
    value.and_then(Value::as_i64).unwrap_or(default)
}

fn is_close(actual: f64, expected: f64, rel_tol: f64, abs_tol: f64) -> bool {
    (actual - expected).abs() <= abs_tol.max(rel_tol * actual.abs().max(expected.abs()))
}

fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    Ok(())
}

fn parse_pressure_percent(scenario: &str) -> Result<i64> {
    let raw = scenario
        .strip_prefix("io-pressure-")
        .and_then(|value| value.strip_suffix("pct"))
        .with_context(|| format!("unexpected scenario {scenario:?}"))?;
    raw.parse::<i64>()
        .with_context(|| format!("unexpected scenario {scenario:?}"))
}

fn fio_job_rw(job: &Value) -> Option<&str> {
    job.get("job options")?.get("rw")?.as_str()
}

#[derive(Clone, Copy)]
pub(crate) struct PressureCalibration {
    pub(crate) baseline_iops: f64,
    pub(crate) expected_bs: i64,
    pub(crate) baseline_read_iops: f64,
    pub(crate) baseline_write_iops: f64,
    pub(crate) read_fraction: f64,
    pub(crate) write_fraction: f64,
}

pub(crate) fn load_case(
    case_path: &Path,
    pressure_path: &Path,
    calibration: PressureCalibration,
) -> Result<Value> {
    let PressureCalibration {
        baseline_iops,
        expected_bs,
        baseline_read_iops,
        baseline_write_iops,
        read_fraction,
        write_fraction,
    } = calibration;
    let db = read_json(case_path)?;
    let pressure = read_json(pressure_path)?;
    let scenario = db.get("scenario").and_then(Value::as_str).unwrap_or("");
    let pct = parse_pressure_percent(scenario)?;
    let sidecar_pct = as_i64(pressure.get("pressure_percent"), -1);
    let sidecar_bs = as_i64(pressure.get("bs_bytes"), 0);
    if sidecar_pct != pct {
        bail!("pressure sidecar percent {sidecar_pct} != scenario {pct}");
    }
    if sidecar_bs != expected_bs {
        bail!("pressure sidecar block size {sidecar_bs} != run support {expected_bs}");
    }

    for (field, expected) in [
        ("baseline_iops", baseline_iops),
        ("baseline_read_iops", baseline_read_iops),
        ("baseline_write_iops", baseline_write_iops),
        ("read_fraction", read_fraction),
        ("write_fraction", write_fraction),
    ] {
        let actual = as_f64(pressure.get(field), 0.0);
        if !is_close(actual, expected, 1e-9, 1e-6) {
            bail!("pressure sidecar {field} {actual} != run support {expected}");
        }
    }

    let target_iops = as_f64(pressure.get("target_iops"), 0.0);
    let target_read_iops = as_f64(pressure.get("target_read_iops"), 0.0);
    let target_write_iops = as_f64(pressure.get("target_write_iops"), 0.0);
    let mut pressure_read_iops = 0.0;
    let mut pressure_write_iops = 0.0;
    let mut pressure_bw_bytes = 0.0;
    let mut pressure_read_bytes = 0.0;
    let mut pressure_write_bytes = 0.0;
    let mut pressure_read_p99_us = None;
    let mut pressure_write_p99_us = None;
    let mut fio_error = 0_i64;
    let jobs = pressure
        .get("jobs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    if pct > 0 {
        if jobs.len() != 2 {
            bail!(
                "nonzero pressure case has {} fio jobs, expected 2",
                jobs.len()
            );
        }
        let read_jobs = jobs
            .iter()
            .filter(|job| fio_job_rw(job) == Some("randread"))
            .collect::<Vec<_>>();
        let write_jobs = jobs
            .iter()
            .filter(|job| fio_job_rw(job) == Some("randwrite"))
            .collect::<Vec<_>>();
        if read_jobs.len() != 1 || write_jobs.len() != 1 {
            bail!("pressure sidecar must contain exactly one randread and one randwrite job");
        }
        let read_job = read_jobs[0];
        let write_job = write_jobs[0];
        validate_fio_job(
            read_job,
            FioExpectation {
                rw: Some("randread"),
                rwmixread: None,
                bs_bytes: Some(expected_bs),
                blockalign_bytes: Some(expected_bs),
                ioengine: Some("psync"),
                direct: Some(1),
            },
        )?;
        validate_fio_job(
            write_job,
            FioExpectation {
                rw: Some("randwrite"),
                rwmixread: None,
                bs_bytes: Some(expected_bs),
                blockalign_bytes: Some(expected_bs),
                ioengine: Some("psync"),
                direct: Some(1),
            },
        )?;
        fio_error = as_i64(read_job.get("error"), 0).max(as_i64(write_job.get("error"), 0));
        let read = read_job.get("read").cloned().unwrap_or_else(|| json!({}));
        let write = write_job.get("write").cloned().unwrap_or_else(|| json!({}));
        pressure_read_iops = as_f64(read.get("iops"), 0.0);
        pressure_write_iops = as_f64(write.get("iops"), 0.0);
        pressure_bw_bytes = as_f64(read.get("bw_bytes"), 0.0) + as_f64(write.get("bw_bytes"), 0.0);
        pressure_read_bytes = as_f64(read.get("io_bytes"), 0.0);
        pressure_write_bytes = as_f64(write.get("io_bytes"), 0.0);
        pressure_read_p99_us = fio_p99_us(&read);
        pressure_write_p99_us = fio_p99_us(&write);
    }

    if pct == 0 && (target_iops != 0.0 || target_read_iops != 0.0 || target_write_iops != 0.0) {
        bail!("zero-pressure case has nonzero target IOPS");
    }
    if pct > 0 && (target_iops <= 0.0 || target_read_iops <= 0.0 || target_write_iops <= 0.0) {
        bail!("nonzero pressure case has nonpositive target IOPS");
    }
    if pct > 0 && !is_close(target_iops, target_read_iops + target_write_iops, 0.0, 1e-6) {
        bail!("pressure target_iops does not equal directional target sum");
    }

    let delivered_iops = pressure_read_iops + pressure_write_iops;
    let ops_completed = as_i64(db.get("ops_completed"), 0).max(1);
    let proc = db
        .get("measured_process")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let system = db
        .get("measured_system_delta")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let system_wall_ns = as_f64(system.get("accounting_wall_ns"), 0.0).max(1.0);

    Ok(json!({
        "case_id": case_path.file_stem().and_then(|value| value.to_str()).unwrap_or(""),
        "format_version": db.get("format_version").cloned().unwrap_or(Value::Null),
        "engine": db.get("engine").cloned().unwrap_or(Value::Null),
        "engine_version": db.get("engine_version").cloned().unwrap_or(Value::Null),
        "durability": db.get("durability").cloned().unwrap_or(Value::Null),
        "workload": db.get("workload").cloned().unwrap_or(Value::Null),
        "records": db.get("records").cloned().unwrap_or(Value::Null),
        "ops_requested": db.get("ops_requested").cloned().unwrap_or(Value::Null),
        "value_bytes": db.get("value_bytes").cloned().unwrap_or(Value::Null),
        "txn_size": db.get("txn_size").cloned().unwrap_or(Value::Null),
        "trial": db.get("trial").cloned().unwrap_or(Value::Null),
        "pressure_percent": pct,
        "pressure_bs_bytes": sidecar_bs,
        "pressure_target_iops": target_iops,
        "pressure_target_read_iops": target_read_iops,
        "pressure_target_write_iops": target_write_iops,
        "pressure_delivered_iops": delivered_iops,
        "pressure_delivered_read_iops": pressure_read_iops,
        "pressure_delivered_write_iops": pressure_write_iops,
        "pressure_delivered_vs_target": if pct > 0 { ratio(Some(delivered_iops), Some(target_iops)) } else { None },
        "pressure_delivered_read_vs_target": if pct > 0 { ratio(Some(pressure_read_iops), Some(target_read_iops)) } else { None },
        "pressure_delivered_write_vs_target": if pct > 0 { ratio(Some(pressure_write_iops), Some(target_write_iops)) } else { None },
        "pressure_delivered_vs_baseline": if pct > 0 { ratio(Some(delivered_iops), Some(baseline_iops)) } else { Some(0.0) },
        "pressure_delivered_bw_mbps": pressure_bw_bytes / 1_000_000.0,
        "pressure_read_bytes": pressure_read_bytes,
        "pressure_write_bytes": pressure_write_bytes,
        "pressure_read_p99_us": pressure_read_p99_us,
        "pressure_write_p99_us": pressure_write_p99_us,
        "pressure_fio_error": fio_error,
        "db_ops_per_s": as_f64(db.get("ops_per_s"), 0.0),
        "db_read_p99_us": as_f64(db.get("read_latency").and_then(|value| value.get("p99_us")), 0.0),
        "db_write_p99_us": as_f64(db.get("write_txn_latency").and_then(|value| value.get("p99_us")), 0.0),
        "db_cpu_ns_per_op": as_f64(proc.get("cpu_runtime_ns"), 0.0) / ops_completed as f64,
        "db_runqueue_wait_fraction": as_f64(proc.get("runqueue_wait_fraction_of_wall"), 0.0),
        "db_io_psi_full_fraction": as_f64(system.get("psi_io_full_us"), 0.0) * 1000.0 / system_wall_ns,
    }))
}

fn identity(case: &Value) -> Vec<Value> {
    [
        "format_version",
        "engine",
        "engine_version",
        "durability",
        "workload",
        "records",
        "ops_requested",
        "value_bytes",
        "txn_size",
    ]
    .into_iter()
    .map(|key| case.get(key).cloned().unwrap_or(Value::Null))
    .collect()
}

fn key_string(values: &[Value]) -> Result<String> {
    serde_json::to_string(values).context("failed to serialize grouping key")
}

fn sort_key(values: &[Value]) -> Vec<String> {
    values.iter().map(py_string).collect()
}

fn enrich_ratios(cases: &mut [Value], problems: &mut Vec<String>) -> Result<()> {
    let mut baselines = BTreeMap::<String, usize>::new();
    for (index, case) in cases.iter().enumerate() {
        if as_i64(case.get("pressure_percent"), -1) == 0 {
            let mut key = identity(case);
            key.push(case.get("trial").cloned().unwrap_or(Value::Null));
            let encoded = key_string(&key)?;
            if baselines.insert(encoded.clone(), index).is_some() {
                problems.push(format!("duplicate zero-pressure baseline for {encoded}"));
            }
        }
    }

    let snapshot = cases.to_vec();
    for case in cases {
        let mut key = identity(case);
        key.push(case.get("trial").cloned().unwrap_or(Value::Null));
        let encoded = key_string(&key)?;
        let Some(base_index) = baselines.get(&encoded).copied() else {
            problems.push(format!(
                "missing zero-pressure baseline for {} {} {} trial={}",
                case.get("engine")
                    .map(py_string)
                    .unwrap_or_else(|| "None".to_owned()),
                case.get("durability")
                    .map(py_string)
                    .unwrap_or_else(|| "None".to_owned()),
                case.get("workload")
                    .map(py_string)
                    .unwrap_or_else(|| "None".to_owned()),
                case.get("trial")
                    .map(py_string)
                    .unwrap_or_else(|| "None".to_owned()),
            ));
            if let Some(object) = case.as_object_mut() {
                object.insert("db_throughput_ratio_vs_zero".to_owned(), Value::Null);
                object.insert("db_read_p99_ratio_vs_zero".to_owned(), Value::Null);
                object.insert("db_write_p99_ratio_vs_zero".to_owned(), Value::Null);
            }
            continue;
        };
        let base = &snapshot[base_index];
        let ratios = [
            (
                "db_throughput_ratio_vs_zero",
                ratio(
                    case.get("db_ops_per_s").and_then(Value::as_f64),
                    base.get("db_ops_per_s").and_then(Value::as_f64),
                ),
            ),
            (
                "db_read_p99_ratio_vs_zero",
                ratio(
                    case.get("db_read_p99_us").and_then(Value::as_f64),
                    base.get("db_read_p99_us").and_then(Value::as_f64),
                ),
            ),
            (
                "db_write_p99_ratio_vs_zero",
                ratio(
                    case.get("db_write_p99_us").and_then(Value::as_f64),
                    base.get("db_write_p99_us").and_then(Value::as_f64),
                ),
            ),
        ];
        if let Some(object) = case.as_object_mut() {
            for (output, value) in ratios {
                object.insert(
                    output.to_owned(),
                    value.map_or(Value::Null, |value| json!(value)),
                );
            }
        }
    }
    Ok(())
}

fn median(mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        Some((values[mid - 1] + values[mid]) / 2.0)
    } else {
        Some(values[mid])
    }
}

fn aggregate(cases: &[Value]) -> Result<Vec<Value>> {
    let mut groups = BTreeMap::<String, (Vec<Value>, Vec<&Value>)>::new();
    for case in cases {
        let mut key = identity(case);
        key.push(case.get("pressure_percent").cloned().unwrap_or(Value::Null));
        key.push(
            case.get("pressure_bs_bytes")
                .cloned()
                .unwrap_or(Value::Null),
        );
        let encoded = key_string(&key)?;
        groups
            .entry(encoded)
            .and_modify(|(_, rows)| rows.push(case))
            .or_insert_with(|| (key, vec![case]));
    }
    let mut grouped = groups.into_values().collect::<Vec<_>>();
    grouped.sort_by_key(|(key, _)| sort_key(key));

    let identity_fields = [
        "format_version",
        "engine",
        "engine_version",
        "durability",
        "workload",
        "records",
        "ops_requested",
        "value_bytes",
        "txn_size",
        "pressure_percent",
        "pressure_bs_bytes",
    ];
    let mut output = Vec::new();
    for (_, rows) in grouped {
        let first = rows
            .first()
            .copied()
            .context("internal empty I/O pressure group")?;
        let mut group = Map::new();
        for field in identity_fields {
            group.insert(
                field.to_owned(),
                first.get(field).cloned().unwrap_or(Value::Null),
            );
        }
        group.insert("trials".to_owned(), json!(rows.len()));
        for field in AGGREGATE_FIELDS {
            let values = rows
                .iter()
                .filter_map(|row| row.get(field).and_then(Value::as_f64))
                .filter(|value| value.is_finite())
                .collect::<Vec<_>>();
            let med = median(values.clone());
            let min = values.iter().copied().min_by(f64::total_cmp);
            let max = values.iter().copied().max_by(f64::total_cmp);
            group.insert(
                format!("{field}_median"),
                med.map_or(Value::Null, |value| json!(value)),
            );
            group.insert(
                format!("{field}_min"),
                min.map_or(Value::Null, |value| json!(value)),
            );
            group.insert(
                format!("{field}_max"),
                max.map_or(Value::Null, |value| json!(value)),
            );
        }
        output.push(Value::Object(group));
    }
    Ok(output)
}

fn fmt(value: Option<&Value>, digits: usize) -> String {
    let Some(value) = value else {
        return "—".to_owned();
    };
    if value.is_null() {
        return "—".to_owned();
    }
    if let Some(number) = value.as_f64() {
        if !number.is_finite() {
            return number.to_string();
        }
        return format!("{number:.digits$}");
    }
    if let Some(raw) = value.as_str() {
        return raw.to_owned();
    }
    value.to_string()
}

fn markdown(groups: &[Value]) -> String {
    let mut lines = vec![
        "# I/O-pressure summary".to_owned(),
        String::new(),
        "Delivered pressure is measured by fio. Falling below the requested cap under database contention is a measured outcome, not automatically a harness failure.".to_owned(),
        String::new(),
        "| engine | dur | workload | pressure | trials | delivered IOPS | delivered/target | read/target | write/target | delivered/baseline | fio MB/s | backing read MB/s | backing write MB/s | ARC hit % | DB ops/s | DB throughput/zero | DB read p99/zero | DB write p99/zero | DB IO PSI full % |".to_owned(),
        "|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|".to_owned(),
    ];
    for group in groups {
        let arc_percent = group
            .get("zfs_arc_hit_fraction_median")
            .and_then(Value::as_f64)
            .map(|value| value * 100.0);
        let psi_percent = group
            .get("db_io_psi_full_fraction_median")
            .and_then(Value::as_f64)
            .map(|value| value * 100.0);
        lines.push(format!(
            "| {} {} | {} | {} | {}% | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            group.get("engine").and_then(Value::as_str).unwrap_or(""),
            group.get("engine_version").map(py_string).unwrap_or_default(),
            group.get("durability").and_then(Value::as_str).unwrap_or(""),
            group.get("workload").and_then(Value::as_str).unwrap_or(""),
            group.get("pressure_percent").map(py_string).unwrap_or_default(),
            group.get("trials").map(py_string).unwrap_or_default(),
            fmt(group.get("pressure_delivered_iops_median"), 3),
            fmt(group.get("pressure_delivered_vs_target_median"), 3),
            fmt(group.get("pressure_delivered_read_vs_target_median"), 3),
            fmt(group.get("pressure_delivered_write_vs_target_median"), 3),
            fmt(group.get("pressure_delivered_vs_baseline_median"), 3),
            fmt(group.get("pressure_delivered_bw_mbps_median"), 3),
            fmt(group.get("storage_read_mbps_median"), 3),
            fmt(group.get("storage_write_mbps_median"), 3),
            arc_percent.map_or_else(|| "—".to_owned(), |value| format!("{value:.3}")),
            fmt(group.get("db_ops_per_s_median"), 3),
            fmt(group.get("db_throughput_ratio_vs_zero_median"), 3),
            fmt(group.get("db_read_p99_ratio_vs_zero_median"), 3),
            fmt(group.get("db_write_p99_ratio_vs_zero_median"), 3),
            psi_percent.map_or_else(|| "—".to_owned(), |value| format!("{value:.3}")),
        ));
    }
    lines.join("\n") + "\n"
}

fn stem_map(dir: &Path) -> Result<BTreeMap<String, std::path::PathBuf>> {
    let mut output = BTreeMap::new();
    if !dir.is_dir() {
        return Ok(output);
    }
    for entry in fs::read_dir(dir).with_context(|| format!("failed to list {}", dir.display()))? {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|value| value.to_str())
            .context("JSON sidecar has non-UTF8 filename")?
            .to_owned();
        output.insert(stem, path);
    }
    Ok(output)
}

fn case_paths(dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    let mut paths = stem_map(dir)?.into_values().collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

fn py_repr_option(value: Option<f64>) -> String {
    value.map_or_else(|| "None".to_owned(), |value| value.to_string())
}

fn merge_object(target: &mut Value, source: &Value) {
    if let (Some(target), Some(source)) = (target.as_object_mut(), source.as_object()) {
        for (key, value) in source {
            target.insert(key.clone(), value.clone());
        }
    }
}

pub(crate) fn run(args: IoPressureArgs<'_>) -> Result<i32> {
    let run_dir = args.run_dir;
    let support = read_json(&run_dir.join("support.json"))?;
    let protocol = as_i64(support.get("calibration_protocol_version"), 0);
    if protocol != 4 {
        bail!("unsupported I/O calibration protocol {protocol}; expected 4");
    }
    let baseline_iops = as_f64(support.get("baseline_iops"), 0.0);
    let baseline_read_iops = as_f64(support.get("baseline_read_iops"), 0.0);
    let baseline_write_iops = as_f64(support.get("baseline_write_iops"), 0.0);
    let read_fraction = as_f64(support.get("read_fraction"), 0.0);
    let write_fraction = as_f64(support.get("write_fraction"), 0.0);
    let expected_bs = as_i64(support.get("pressure_bs_bytes"), 0);
    let filesystem = support.get("filesystem").and_then(Value::as_str);

    let mut problems = Vec::<String>::new();
    let mut warnings = Vec::<String>::new();
    let mut cases = Vec::<Value>::new();
    let paths = case_paths(&run_dir.join("cases"))?;
    let pressure_paths = stem_map(&run_dir.join("pressure"))?;
    let storage_paths = stem_map(&run_dir.join("storage"))?;
    let pressure_storage_paths = stem_map(&run_dir.join("pressure-storage"))?;

    for case_path in &paths {
        let stem = case_path
            .file_stem()
            .and_then(|value| value.to_str())
            .context("case path has non-UTF8 stem")?;
        let Some(pressure_path) = pressure_paths.get(stem) else {
            problems.push(format!("missing pressure sidecar for {stem}"));
            continue;
        };
        let Some(storage_path) = storage_paths.get(stem) else {
            problems.push(format!("missing storage sidecar for {stem}"));
            continue;
        };
        let mut case = match load_case(
            case_path,
            pressure_path,
            PressureCalibration {
                baseline_iops,
                expected_bs,
                baseline_read_iops,
                baseline_write_iops,
                read_fraction,
                write_fraction,
            },
        ) {
            Ok(case) => case,
            Err(error) => {
                problems.push(format!("{stem}: {error}"));
                continue;
            }
        };
        let storage = match load_storage_delta(storage_path) {
            Ok(storage) => storage,
            Err(error) => {
                problems.push(format!("{stem}: {error}"));
                continue;
            }
        };
        merge_object(&mut case, &storage);

        let pressure_percent = as_i64(case.get("pressure_percent"), 0);
        if pressure_percent > 0 {
            let Some(pressure_storage_path) = pressure_storage_paths.get(stem) else {
                problems.push(format!("{stem}: missing pressure-lifetime storage sidecar"));
                continue;
            };
            let pressure_storage = match load_storage_delta(pressure_storage_path) {
                Ok(storage) => storage,
                Err(error) => {
                    problems.push(format!("{stem}: {error}"));
                    continue;
                }
            };
            let direct_read = pressure_storage
                .get("zfs_direct_read_bytes")
                .and_then(Value::as_f64);
            let direct_write = pressure_storage
                .get("zfs_direct_write_bytes")
                .and_then(Value::as_f64);
            let read_ratio = ratio(
                direct_read,
                case.get("pressure_read_bytes").and_then(Value::as_f64),
            );
            let write_ratio = ratio(
                direct_write,
                case.get("pressure_write_bytes").and_then(Value::as_f64),
            );
            if let Some(object) = case.as_object_mut() {
                object.insert(
                    "pressure_storage_accounting_s".to_owned(),
                    pressure_storage
                        .get("storage_accounting_s")
                        .cloned()
                        .unwrap_or(Value::Null),
                );
                for field in [
                    "zfs_direct_read_ios",
                    "zfs_direct_read_bytes",
                    "zfs_direct_write_ios",
                    "zfs_direct_write_bytes",
                ] {
                    object.insert(
                        format!("pressure_{field}"),
                        pressure_storage.get(field).cloned().unwrap_or(Value::Null),
                    );
                }
                object.insert(
                    "zfs_direct_read_bytes_per_pressure_read_byte".to_owned(),
                    read_ratio.map_or(Value::Null, |value| json!(value)),
                );
                object.insert(
                    "zfs_direct_write_bytes_per_pressure_write_byte".to_owned(),
                    write_ratio.map_or(Value::Null, |value| json!(value)),
                );
            }
        } else if let Some(object) = case.as_object_mut() {
            for field in [
                "pressure_storage_accounting_s",
                "pressure_zfs_direct_read_ios",
                "pressure_zfs_direct_read_bytes",
                "pressure_zfs_direct_write_ios",
                "pressure_zfs_direct_write_bytes",
                "zfs_direct_read_bytes_per_pressure_read_byte",
                "zfs_direct_write_bytes_per_pressure_write_byte",
            ] {
                object.insert(field.to_owned(), Value::Null);
            }
        }

        if as_i64(case.get("pressure_bs_bytes"), 0) != expected_bs {
            problems.push(format!(
                "{stem}: pressure block size {} != run support {expected_bs}",
                as_i64(case.get("pressure_bs_bytes"), 0)
            ));
        }
        if as_i64(case.get("pressure_fio_error"), 0) != 0 {
            problems.push(format!(
                "{stem}: fio error={}",
                as_i64(case.get("pressure_fio_error"), 0)
            ));
        }
        if filesystem == Some("zfs") && pressure_percent > 0 {
            let direct_read_ratio = case
                .get("zfs_direct_read_bytes_per_pressure_read_byte")
                .and_then(Value::as_f64);
            let direct_write_ratio = case
                .get("zfs_direct_write_bytes_per_pressure_write_byte")
                .and_then(Value::as_f64);
            if direct_read_ratio.is_none_or(|value| value < 0.80) {
                problems.push(format!(
                    "{stem}: ZFS direct-read evidence covers only {} of fio pressure reads",
                    py_repr_option(direct_read_ratio)
                ));
            }
            if direct_write_ratio.is_none_or(|value| value < 0.80) {
                problems.push(format!(
                    "{stem}: ZFS direct-write evidence covers only {} of fio pressure writes",
                    py_repr_option(direct_write_ratio)
                ));
            }
        }
        if pressure_percent > 0
            && let Some(delivery) = case
                .get("pressure_delivered_vs_target")
                .and_then(Value::as_f64)
            && delivery < 0.80
        {
            warnings.push(format!(
                "{stem}: delivered pressure was {:.1}% of target; this may be real device contention, but compare requested and delivered pressure when interpreting the DB result",
                100.0 * delivery
            ));
        }
        cases.push(case);
    }

    let completed_stems = paths
        .iter()
        .filter_map(|path| {
            path.file_stem()
                .and_then(|value| value.to_str())
                .map(str::to_owned)
        })
        .collect::<std::collections::BTreeSet<_>>();
    for (mapping, label) in [
        (&pressure_paths, "pressure sidecar"),
        (&storage_paths, "storage sidecar"),
        (&pressure_storage_paths, "pressure-lifetime storage sidecar"),
    ] {
        for stem in mapping.keys() {
            if !completed_stems.contains(stem) {
                warnings.push(format!("{label} has no completed database result: {stem}"));
            }
        }
    }

    enrich_ratios(&mut cases, &mut problems)?;
    let groups = aggregate(&cases)?;
    if let Some(expected) = args.expect_trials {
        for group in &groups {
            let trials = group.get("trials").and_then(Value::as_u64).unwrap_or(0) as usize;
            if trials != expected {
                problems.push(format!(
                    "{} {} {} pressure={}%: {trials} trials, expected {expected}",
                    group.get("engine").map(py_string).unwrap_or_default(),
                    group.get("durability").map(py_string).unwrap_or_default(),
                    group.get("workload").map(py_string).unwrap_or_default(),
                    group
                        .get("pressure_percent")
                        .map(py_string)
                        .unwrap_or_default(),
                ));
            }
        }
    }

    let result = json!({
        "baseline_iops": baseline_iops,
        "baseline_read_iops": baseline_read_iops,
        "baseline_write_iops": baseline_write_iops,
        "read_fraction": read_fraction,
        "write_fraction": write_fraction,
        "pressure_bs_bytes": expected_bs,
        "case_count": cases.len(),
        "group_count": groups.len(),
        "problems": problems,
        "warnings": warnings,
        "cases": cases,
        "groups": groups,
    });
    let encoded = serde_json::to_string_pretty(&result)? + "\n";
    if let Some(path) = args.json_out {
        ensure_parent(path)?;
        fs::write(path, encoded).with_context(|| format!("failed to write {}", path.display()))?;
    } else {
        print!("{encoded}");
    }
    if let Some(path) = args.markdown_out {
        ensure_parent(path)?;
        let report_groups = result
            .get("groups")
            .and_then(Value::as_array)
            .context("internal I/O pressure groups array missing")?;
        fs::write(path, markdown(report_groups))
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(
        if result
            .get("problems")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
        {
            0
        } else {
            1
        },
    )
}

#[cfg(test)]
mod tests {
    use super::{PressureCalibration, load_case};
    use anyhow::Result;
    use serde_json::json;
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);

    fn paths() -> (PathBuf, PathBuf) {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "all-db-bench-io-pressure-{}-{id}",
            std::process::id()
        ));
        (
            root.with_extension("case.json"),
            root.with_extension("pressure.json"),
        )
    }

    fn db() -> serde_json::Value {
        json!({
            "format_version":5,
            "scenario":"io-pressure-30pct",
            "engine":"redb",
            "engine_version":"test",
            "durability":"sync",
            "workload":"point-read",
            "records":100,
            "ops_requested":100,
            "ops_completed":100,
            "value_bytes":256,
            "txn_size":100,
            "trial":1,
            "ops_per_s":1000.0,
            "read_latency":{"p99_us":10.0},
            "write_txn_latency":{"p99_us":0.0},
            "measured_process":{"cpu_runtime_ns":1000,"runqueue_wait_fraction_of_wall":0.0},
            "measured_system_delta":{"accounting_wall_ns":1_000_000_000,"psi_io_full_us":0}
        })
    }

    #[test]
    fn two_directional_jobs_are_joined() -> Result<()> {
        let (case, pressure) = paths();
        fs::write(&case, serde_json::to_vec(&db())?)?;
        fs::write(
            &pressure,
            serde_json::to_vec(&json!({
                "pressure_percent":30,
                "target_iops":300,
                "target_read_iops":210,
                "target_write_iops":90,
                "baseline_iops":1000,
                "baseline_read_iops":700,
                "baseline_write_iops":300,
                "read_fraction":0.7,
                "write_fraction":0.3,
                "bs_bytes":131072,
                "jobs":[
                    {
                        "error":0,
                        "job options":{"rw":"randread","bs":"131072","ba":"131072","ioengine":"psync","direct":"1"},
                        "read":{"iops":200.0,"bw_bytes":20_000_000,"io_bytes":2_000_000},
                        "write":{"iops":0.0,"bw_bytes":0,"io_bytes":0}
                    },
                    {
                        "error":0,
                        "job options":{"rw":"randwrite","bs":"131072","ba":"131072","ioengine":"psync","direct":"1"},
                        "read":{"iops":0.0,"bw_bytes":0,"io_bytes":0},
                        "write":{"iops":80.0,"bw_bytes":8_000_000,"io_bytes":800_000}
                    }
                ]
            }))?,
        )?;
        let row = load_case(
            &case,
            &pressure,
            PressureCalibration {
                baseline_iops: 1000.0,
                expected_bs: 131072,
                baseline_read_iops: 700.0,
                baseline_write_iops: 300.0,
                read_fraction: 0.7,
                write_fraction: 0.3,
            },
        )?;
        fs::remove_file(&case)?;
        fs::remove_file(&pressure)?;
        assert_eq!(row["pressure_delivered_iops"], 280.0);
        assert_eq!(row["pressure_delivered_vs_target"], 280.0 / 300.0);
        assert_eq!(row["pressure_delivered_read_vs_target"], 200.0 / 210.0);
        assert_eq!(row["pressure_delivered_write_vs_target"], 80.0 / 90.0);
        Ok(())
    }

    #[test]
    fn rejects_single_mixed_job() -> Result<()> {
        let (case, pressure) = paths();
        fs::write(
            &case,
            serde_json::to_vec(&json!({"scenario":"io-pressure-30pct"}))?,
        )?;
        fs::write(
            &pressure,
            serde_json::to_vec(&json!({
                "pressure_percent":30,
                "target_iops":300,
                "target_read_iops":210,
                "target_write_iops":90,
                "baseline_iops":1000,
                "baseline_read_iops":700,
                "baseline_write_iops":300,
                "read_fraction":0.7,
                "write_fraction":0.3,
                "bs_bytes":131072,
                "jobs":[{"error":0,"job options":{"rw":"randrw"}}]
            }))?,
        )?;
        let result = load_case(
            &case,
            &pressure,
            PressureCalibration {
                baseline_iops: 1000.0,
                expected_bs: 131072,
                baseline_read_iops: 700.0,
                baseline_write_iops: 300.0,
                read_fraction: 0.7,
                write_fraction: 0.3,
            },
        );
        fs::remove_file(&case)?;
        fs::remove_file(&pressure)?;
        assert!(result.is_err());
        Ok(())
    }
}
