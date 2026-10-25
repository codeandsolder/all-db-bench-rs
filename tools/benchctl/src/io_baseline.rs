use crate::io_evidence::{
    FioExpectation, composite_mixed_iops, fio_p99_us, load_storage_delta, ratio, validate_fio_job,
};
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use std::{collections::BTreeSet, fs, path::Path};

pub(crate) struct IoBaselineArgs<'a> {
    pub(crate) run_dir: &'a Path,
    pub(crate) json_out: Option<&'a Path>,
    pub(crate) markdown_out: Option<&'a Path>,
}

fn read_json(path: &Path) -> Result<Value> {
    serde_json::from_str(
        &fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?,
    )
    .with_context(|| format!("invalid JSON in {}", path.display()))
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

    let read = job.get("read").cloned().unwrap_or_else(|| json!({}));
    let write = job.get("write").cloned().unwrap_or_else(|| json!({}));
    let submitted_read = as_f64(read.get("io_bytes"), 0.0);
    let submitted_write = as_f64(write.get("io_bytes"), 0.0);
    let storage = load_storage_delta(&storage_path)?;
    let direct_read_ratio = ratio(
        storage.get("zfs_direct_read_bytes").and_then(Value::as_f64),
        Some(submitted_read),
    );
    let direct_write_ratio = ratio(
        storage
            .get("zfs_direct_write_bytes")
            .and_then(Value::as_f64),
        Some(submitted_write),
    );

    if calibration_direction.is_some() && filesystem == Some("zfs") {
        if calibration_direction == Some("read")
            && direct_read_ratio.is_none_or(|value| value < 0.90)
        {
            problems.push(format!(
                "{}: ZFS direct-read bytes cover only {direct_read_ratio:?} of fio submitted reads",
                fio_path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or(filename)
            ));
        }
        if calibration_direction == Some("write")
            && direct_write_ratio.is_none_or(|value| value < 0.90)
        {
            problems.push(format!(
                "{}: ZFS direct-write bytes cover only {direct_write_ratio:?} of fio submitted writes",
                fio_path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or(filename)
            ));
        }
    }

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
        calibration_direction.map_or(Value::Null, |value| json!(value)),
    );
    row.insert(
        "fio_read_iops".to_owned(),
        json!(as_f64(read.get("iops"), 0.0)),
    );
    row.insert(
        "fio_write_iops".to_owned(),
        json!(as_f64(write.get("iops"), 0.0)),
    );
    row.insert(
        "fio_read_mbps".to_owned(),
        json!(as_f64(read.get("bw_bytes"), 0.0) / 1_000_000.0),
    );
    row.insert(
        "fio_write_mbps".to_owned(),
        json!(as_f64(write.get("bw_bytes"), 0.0) / 1_000_000.0),
    );
    row.insert(
        "fio_read_p99_us".to_owned(),
        fio_p99_us(&read).map_or(Value::Null, |value| json!(value)),
    );
    row.insert(
        "fio_write_p99_us".to_owned(),
        fio_p99_us(&write).map_or(Value::Null, |value| json!(value)),
    );
    row.insert("fio_submitted_read_bytes".to_owned(), json!(submitted_read));
    row.insert(
        "fio_submitted_write_bytes".to_owned(),
        json!(submitted_write),
    );
    row.insert(
        "backing_read_bytes_per_submitted_read_byte".to_owned(),
        ratio(
            storage.get("storage_read_bytes").and_then(Value::as_f64),
            Some(submitted_read),
        )
        .map_or(Value::Null, |value| json!(value)),
    );
    row.insert(
        "backing_write_bytes_per_submitted_write_byte".to_owned(),
        ratio(
            storage.get("storage_write_bytes").and_then(Value::as_f64),
            Some(submitted_write),
        )
        .map_or(Value::Null, |value| json!(value)),
    );
    row.insert(
        "zfs_direct_read_bytes_per_submitted_read_byte".to_owned(),
        direct_read_ratio.map_or(Value::Null, |value| json!(value)),
    );
    row.insert(
        "zfs_direct_write_bytes_per_submitted_write_byte".to_owned(),
        direct_write_ratio.map_or(Value::Null, |value| json!(value)),
    );
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

pub(crate) fn run(args: IoBaselineArgs<'_>) -> Result<i32> {
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
                problems.push(format!("invalid directional calibration capacity: {error}"))
            }
        }
    }

    let result = json!({
        "support": support,
        "test_count": rows.len(),
        "problems": problems,
        "warnings": warnings,
        "pressure_calibration": {
            "read_iops": read_row.and_then(|row| row.get("fio_read_iops")).and_then(Value::as_f64),
            "write_iops": write_row.and_then(|row| row.get("fio_write_iops")).and_then(Value::as_f64),
            "composite_iops": composite,
            "read_fraction": as_f64(pressure.get("read_fraction"), 0.0),
            "write_fraction": as_f64(pressure.get("write_fraction"), 0.0),
        },
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
    use super::fmt;
    use serde_json::json;

    #[test]
    fn formatting_matches_python_summary_contract() {
        assert_eq!(fmt(Some(&json!(12.34567)), 3), "12.346");
        assert_eq!(fmt(Some(&Value::Null), 3), "—");
    }

    use serde_json::Value;
}
