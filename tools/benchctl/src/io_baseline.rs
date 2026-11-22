use crate::io_evidence::{
    FioExpectation, composite_mixed_iops, fio_p99_us, load_storage_delta, ratio, validate_fio_job,
};
use crate::json_io::read_json;
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use std::{collections::BTreeSet, fs, path::Path};

#[derive(Clone, Copy)]
pub struct IoBaselineArgs<'a> {
    pub(crate) run_dir: &'a Path,
    pub(crate) json_out: Option<&'a Path>,
    pub(crate) markdown_out: Option<&'a Path>,
}

fn as_f64(value: Option<&Value>, default: f64) -> f64 {
    value.and_then(Value::as_f64).unwrap_or(default)
}

fn as_i64(value: Option<&Value>, default: i64) -> i64 {
    value.and_then(Value::as_i64).unwrap_or(default)
}

fn text(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str)
}

fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    Ok(())
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

struct DirectionMetrics {
    iops: f64,
    mbps: f64,
    p99_us: Option<f64>,
    submitted_bytes: f64,
    backing_ratio: Option<f64>,
    direct_ratio: Option<f64>,
}

impl DirectionMetrics {
    fn from_job(job: &Value, storage: &Value, direction: &str) -> Self {
        let data = &job[direction];
        let submitted_bytes = as_f64(data.get("io_bytes"), 0.0);
        Self {
            iops: as_f64(data.get("iops"), 0.0),
            mbps: as_f64(data.get("bw_bytes"), 0.0) / 1_000_000.0,
            p99_us: fio_p99_us(data),
            submitted_bytes,
            backing_ratio: ratio(
                storage
                    .get(format!("storage_{direction}_bytes"))
                    .and_then(Value::as_f64),
                Some(submitted_bytes),
            ),
            direct_ratio: ratio(
                storage
                    .get(format!("zfs_direct_{direction}_bytes"))
                    .and_then(Value::as_f64),
                Some(submitted_bytes),
            ),
        }
    }

    fn insert_fields(&self, row: &mut Map<String, Value>, direction: &str) {
        for (field, value) in [
            (format!("fio_{direction}_iops"), json!(self.iops)),
            (format!("fio_{direction}_mbps"), json!(self.mbps)),
            (format!("fio_{direction}_p99_us"), json!(self.p99_us)),
            (
                format!("fio_submitted_{direction}_bytes"),
                json!(self.submitted_bytes),
            ),
            (
                format!("backing_{direction}_bytes_per_submitted_{direction}_byte"),
                json!(self.backing_ratio),
            ),
            (
                format!("zfs_direct_{direction}_bytes_per_submitted_{direction}_byte"),
                json!(self.direct_ratio),
            ),
        ] {
            row.insert(field, value);
        }
    }
}

fn summarize_one(
    run: &Path,
    filename: &str,
    pressure: &Map<String, Value>,
    authoritative: &std::collections::BTreeMap<String, &'static str>,
    filesystem: Option<&str>,
    problems: &mut Vec<String>,
) -> Result<Value> {
    let fio_path = run.join(filename);
    let storage_path = run.join("storage").join(filename);
    let raw = read_json(&fio_path)?;
    let jobs = raw
        .get("jobs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if jobs.len() != 1 {
        bail!("expected one fio job, got {}", jobs.len());
    }
    let job = jobs.first().context("fio result unexpectedly had no job")?;
    let error = as_i64(job.get("error"), 0);
    if error != 0 {
        bail!("fio error={error}");
    }

    let calibration_direction = authoritative.get(filename).copied();
    if let Some(direction) = calibration_direction {
        let direction_object = pressure
            .get(direction)
            .and_then(Value::as_object)
            .with_context(|| format!("pressure_calibration missing {direction}"))?;
        validate_fio_job(
            job,
            FioExpectation {
                rw: text(direction_object.get("rw")),
                rwmixread: None,
                bs_bytes: pressure.get("bs_bytes").and_then(Value::as_i64),
                blockalign_bytes: pressure.get("blockalign_bytes").and_then(Value::as_i64),
                ioengine: text(pressure.get("ioengine")),
                direct: pressure.get("direct").and_then(Value::as_i64),
            },
        )?;
    }

    let storage = load_storage_delta(&storage_path)?;
    let mut row = Map::new();
    row.insert(
        "name".to_owned(),
        json!(
            fio_path
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or("")
        ),
    );
    row.insert(
        "authoritative_pressure_calibration".to_owned(),
        json!(calibration_direction),
    );
    for direction in ["read", "write"] {
        let metrics = DirectionMetrics::from_job(job, &storage, direction);
        if calibration_direction == Some(direction)
            && filesystem == Some("zfs")
            && metrics.direct_ratio.is_none_or(|value| value < 0.90)
        {
            problems.push(format!(
                "{}: ZFS direct-{direction} bytes cover only {:?} of fio submitted {direction}s",
                fio_path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or(filename),
                metrics.direct_ratio
            ));
        }
        metrics.insert_fields(&mut row, direction);
    }
    if let Some(storage_object) = storage.as_object() {
        for (key, value) in storage_object {
            row.insert(key.clone(), value.clone());
        }
    }
    row.insert(
        "job_options".to_owned(),
        job.get("job options").cloned().unwrap_or_else(|| json!({})),
    );
    Ok(Value::Object(row))
}

fn markdown(rows: &[Value]) -> String {
    let mut lines = vec![
        "# Storage calibration summary".to_owned(),
        String::new(),
        "Backing-device counters are whole-device context and can include unrelated host I/O. Ratios are observational, not process attribution.".to_owned(),
        String::new(),
        "| test | pressure ref | fio read IOPS | fio write IOPS | fio read MB/s | fio write MB/s | read p99 us | write p99 us | backing read MB/s | backing write MB/s | backing/read submitted | backing/write submitted | ZFS direct/read submitted | ZFS direct/write submitted | ARC hit % |".to_owned(),
        "|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|".to_owned(),
    ];
    for row in rows {
        let arc_percent = row
            .get("zfs_arc_hit_fraction")
            .and_then(Value::as_f64)
            .map(|value| value * 100.0);
        lines.push(format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            row.get("name").and_then(Value::as_str).unwrap_or(""),
            row.get("authoritative_pressure_calibration")
                .and_then(Value::as_str)
                .unwrap_or(""),
            fmt(row.get("fio_read_iops"), 3),
            fmt(row.get("fio_write_iops"), 3),
            fmt(row.get("fio_read_mbps"), 3),
            fmt(row.get("fio_write_mbps"), 3),
            fmt(row.get("fio_read_p99_us"), 3),
            fmt(row.get("fio_write_p99_us"), 3),
            fmt(row.get("storage_read_mbps"), 3),
            fmt(row.get("storage_write_mbps"), 3),
            fmt(row.get("backing_read_bytes_per_submitted_read_byte"), 3),
            fmt(row.get("backing_write_bytes_per_submitted_write_byte"), 3),
            fmt(row.get("zfs_direct_read_bytes_per_submitted_read_byte"), 3),
            fmt(
                row.get("zfs_direct_write_bytes_per_submitted_write_byte"),
                3
            ),
            arc_percent.map_or_else(|| "—".to_owned(), |value| format!("{value:.3}")),
        ));
    }
    lines.join("\n") + "\n"
}

fn declared_tests(support: &Value) -> Result<Vec<String>> {
    let declared = support
        .get("tests")
        .and_then(Value::as_array)
        .context("storage baseline support.json has no declared tests")?;
    if declared.is_empty() {
        bail!("storage baseline support.json has no declared tests");
    }
    let declared_names = declared
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .context("storage baseline test filename is not a string")
        })
        .collect::<Result<Vec<_>>>()?;
    let unique = declared_names.iter().collect::<BTreeSet<_>>();
    if unique.len() != declared_names.len() {
        bail!("storage baseline support.json contains duplicate test filenames");
    }

    Ok(declared_names)
}

fn summarize_capacity(
    rows: &[Value],
    read_file: &str,
    write_file: &str,
    pressure: &Map<String, Value>,
    problems: &mut Vec<String>,
) -> Result<Value> {
    let read_name = Path::new(read_file)
        .file_stem()
        .and_then(|value| value.to_str())
        .context("pressure read filename has no UTF-8 stem")?;
    let write_name = Path::new(write_file)
        .file_stem()
        .and_then(|value| value.to_str())
        .context("pressure write filename has no UTF-8 stem")?;
    let read_row = rows
        .iter()
        .find(|row| row.get("name").and_then(Value::as_str) == Some(read_name));
    let write_row = rows
        .iter()
        .find(|row| row.get("name").and_then(Value::as_str) == Some(write_name));
    let mut composite = None;
    if read_row.is_none() || write_row.is_none() {
        problems.push("missing one or both protocol-v4 directional calibration rows".to_owned());
    } else if let (Some(read_row), Some(write_row)) = (read_row, write_row) {
        match composite_mixed_iops(
            as_f64(read_row.get("fio_read_iops"), 0.0),
            as_f64(write_row.get("fio_write_iops"), 0.0),
            as_f64(pressure.get("read_fraction"), 0.0),
        ) {
            Ok(value) => composite = Some(value),
            Err(error) => {
                problems.push(format!("invalid directional calibration capacity: {error}"));
            }
        }
    }

    Ok(json!({
        "read_iops": read_row.and_then(|row| row.get("fio_read_iops")).and_then(Value::as_f64),
        "write_iops": write_row.and_then(|row| row.get("fio_write_iops")).and_then(Value::as_f64),
        "composite_iops": composite,
        "read_fraction": as_f64(pressure.get("read_fraction"), 0.0),
        "write_fraction": as_f64(pressure.get("write_fraction"), 0.0),
    }))
}

pub fn run(args: IoBaselineArgs<'_>) -> Result<i32> {
    let run = args.run_dir;
    let support = read_json(&run.join("support.json"))?;
    if as_i64(support.get("calibration_protocol_version"), 0) != 4 {
        bail!("storage baseline must use calibration protocol 4");
    }
    let pressure = support
        .get("pressure_calibration")
        .and_then(Value::as_object)
        .context("support.json missing pressure_calibration")?;
    let read_file = pressure
        .get("read")
        .and_then(Value::as_object)
        .and_then(|value| value.get("file"))
        .and_then(Value::as_str)
        .context("pressure calibration read file missing")?;
    let write_file = pressure
        .get("write")
        .and_then(Value::as_object)
        .and_then(|value| value.get("file"))
        .and_then(Value::as_str)
        .context("pressure calibration write file missing")?;
    let authoritative = std::collections::BTreeMap::from([
        (read_file.to_owned(), "read"),
        (write_file.to_owned(), "write"),
    ]);

    let declared_names = declared_tests(&support)?;
    let filesystem = support.get("filesystem").and_then(Value::as_str);
    let mut rows = Vec::new();
    let mut problems = Vec::new();
    let warnings: Vec<String> = Vec::new();
    for filename in &declared_names {
        let fio_path = run.join(filename);
        if !fio_path.is_file() {
            problems.push(format!("missing declared fio result {filename}"));
            continue;
        }
        let storage_path = run.join("storage").join(filename);
        if !storage_path.is_file() {
            problems.push(format!("missing storage sidecar for {filename}"));
            continue;
        }
        match summarize_one(
            run,
            filename,
            pressure,
            &authoritative,
            filesystem,
            &mut problems,
        ) {
            Ok(row) => rows.push(row),
            Err(error) => problems.push(format!("{filename}: {error}")),
        }
    }

    let capacity = summarize_capacity(&rows, read_file, write_file, pressure, &mut problems)?;

    let result = json!({
        "support": support,
        "test_count": rows.len(),
        "problems": problems,
        "warnings": warnings,
        "pressure_calibration": capacity,
        "tests": rows,
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
        let report_rows = result
            .get("tests")
            .and_then(Value::as_array)
            .context("internal baseline tests array missing")?;
        fs::write(path, markdown(report_rows))
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(i32::from(
        !result
            .get("problems")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty),
    ))
}

#[cfg(test)]
mod tests {
    use super::{DirectionMetrics, fmt, summarize_capacity};
    use anyhow::Result;
    use serde_json::Map;
    use serde_json::json;

    #[test]
    fn formatting_matches_python_summary_contract() {
        assert_eq!(fmt(Some(&json!(12.34567)), 3), "12.346");
        assert_eq!(fmt(Some(&Value::Null), 3), "—");
    }

    #[test]
    fn directional_metrics_preserve_report_keys_and_units() -> Result<()> {
        let job = json!({"read": {"iops": 100.0, "bw_bytes": 4_000_000, "io_bytes": 1000,
                                    "clat_ns": {"percentile": {"99.000000": 5000}}}});
        let storage = json!({"storage_read_bytes": 2000, "zfs_direct_read_bytes": 900});
        let mut row = Map::new();
        DirectionMetrics::from_job(&job, &storage, "read").insert_fields(&mut row, "read");
        DirectionMetrics::from_job(&job, &storage, "write").insert_fields(&mut row, "write");
        assert_eq!(row["fio_read_iops"], 100.0);
        assert_eq!(row["fio_read_mbps"], 4.0);
        assert_eq!(row["fio_read_p99_us"], 5.0);
        assert_eq!(row["backing_read_bytes_per_submitted_read_byte"], 2.0);
        assert_eq!(row["zfs_direct_read_bytes_per_submitted_read_byte"], 0.9);
        assert_eq!(row["fio_write_iops"], 0.0);
        assert!(row["fio_write_p99_us"].is_null());
        assert!(row["zfs_direct_write_bytes_per_submitted_write_byte"].is_null());
        let capacity = json!({"read_fraction": 0.5, "write_fraction": 0.5});
        let mut problems = Vec::new();
        let report = summarize_capacity(
            &[],
            "r.json",
            "w.json",
            capacity
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("capacity object"))?,
            &mut problems,
        )?;
        assert!(report["composite_iops"].is_null());
        assert_eq!(problems.len(), 1);
        Ok(())
    }

    use serde_json::Value;
}
