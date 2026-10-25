use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

const READ: &str = "full-record-v1";
const WRITE: &str = "no-return-v1";
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

pub(crate) struct RecordSmokeArgs<'a> {
    pub(crate) bench_bin: &'a Path,
    pub(crate) concurrency_bin: &'a Path,
    pub(crate) sustained_bin: &'a Path,
    pub(crate) rocks_bench_bin: &'a Path,
    pub(crate) rocks_concurrency_bin: &'a Path,
    pub(crate) rocks_sustained_bin: &'a Path,
}

struct TempRoot(PathBuf);

impl TempRoot {
    fn create() -> Result<Self> {
        for _ in 0..1000 {
            let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let path = PathBuf::from(format!(
                "/tmp/record-runtime-smoke-{}-{id}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to create {}", path.display()));
                }
            }
        }
        bail!("failed to allocate a unique record runtime smoke directory")
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn value_as_i64(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number.as_i64(),
        Value::String(raw) => raw.parse::<i64>().ok(),
        _ => None,
    }
}

fn read_result_rows(output: &Path) -> Result<Vec<Value>> {
    let text = fs::read_to_string(output)
        .with_context(|| format!("failed to read smoke output {}", output.display()))?;
    if let Ok(document) = serde_json::from_str::<Value>(&text) {
        return Ok(vec![document]);
    }
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .with_context(|| format!("invalid NDJSON in {}", output.display()))
        })
        .collect()
}

fn checked_result(
    mut command: Command,
    output: &Path,
    label: &str,
    expected_format: i64,
    expected_ops: i64,
) -> Result<()> {
    let result = command
        .output()
        .with_context(|| format!("failed to launch runtime smoke for {label}"))?;
    if !result.status.success() {
        let rc = result.status.code().unwrap_or(-1);
        let mut combined = String::from_utf8_lossy(&result.stdout).into_owned();
        combined.push_str(&String::from_utf8_lossy(&result.stderr));
        bail!("runtime smoke failed for {label} rc={rc}\n{combined}");
    }
    let rows = read_result_rows(output)?;
    if rows.len() != 1 || !rows[0].is_object() {
        bail!("expected one result object for {label}, got {}", rows.len());
    }
    let row = &rows[0];
    if row.get("read_materialization").and_then(Value::as_str) != Some(READ)
        || row.get("write_materialization").and_then(Value::as_str) != Some(WRITE)
    {
        bail!("semantic marker mismatch for {label}: {row}");
    }
    if value_as_i64(row.get("ops_completed")) != Some(expected_ops) {
        bail!(
            "operation count mismatch for {label}: {}",
            row.get("ops_completed").unwrap_or(&Value::Null)
        );
    }
    if value_as_i64(row.get("format_version")) != Some(expected_format) {
        bail!(
            "format version mismatch for {label}: {}",
            row.get("format_version").unwrap_or(&Value::Null)
        );
    }
    println!("record runtime smoke ok: {label}");
    Ok(())
}

fn baseline_command(
    binary: &Path,
    engine: &str,
    workload: &str,
    root: &Path,
) -> (Command, PathBuf) {
    let output = root.join(format!("baseline-{engine}-{workload}.json"));
    let mut command = Command::new(binary);
    command.args([
        "--engine",
        engine,
        "--durability",
        "sync",
        "--workload",
        workload,
        "--records",
        "32",
        "--ops",
        "64",
        "--payload-bytes",
        "64",
        "--txn-size",
        "8",
        "--warmup-reads",
        "8",
        "--root",
    ]);
    command.arg(root.join(format!("baseline-{engine}-{workload}")));
    command.arg("--output").arg(&output).arg("--keep-db");
    (command, output)
}

fn concurrency_command(binary: &Path, engine: &str, root: &Path) -> (Command, PathBuf) {
    let output = root.join(format!("concurrency-{engine}.json"));
    let mut command = Command::new(binary);
    command.args([
        "--engine",
        engine,
        "--durability",
        "sync",
        "--workload",
        "point-read",
        "--records",
        "32",
        "--ops",
        "64",
        "--clients",
        "2",
        "--payload-bytes",
        "64",
        "--txn-size",
        "8",
        "--warmup-reads",
        "8",
        "--root",
    ]);
    command.arg(root.join(format!("concurrency-{engine}")));
    command.arg("--output").arg(&output).arg("--keep-db");
    (command, output)
}

fn sustained_command(binary: &Path, engine: &str, root: &Path) -> (Command, PathBuf) {
    let output = root.join(format!("sustained-{engine}.json"));
    let mut command = Command::new(binary);
    command.args([
        "--engine",
        engine,
        "--durability",
        "sync",
        "--pattern",
        "churn",
        "--records",
        "32",
        "--ops",
        "40",
        "--window-ops",
        "20",
        "--payload-bytes",
        "64",
        "--txn-size",
        "10",
        "--warmup-reads",
        "8",
        "--settle-ms",
        "0",
        "--root",
    ]);
    command.arg(root.join(format!("sustained-{engine}")));
    command.arg("--output").arg(&output).arg("--keep-db");
    (command, output)
}

fn verify_files(args: &RecordSmokeArgs<'_>) -> Result<()> {
    for path in [
        args.bench_bin,
        args.concurrency_bin,
        args.sustained_bin,
        args.rocks_bench_bin,
        args.rocks_concurrency_bin,
        args.rocks_sustained_bin,
    ] {
        if !path.is_file() {
            bail!("binary does not exist: {}", path.display());
        }
    }
    Ok(())
}

pub(crate) fn run(args: RecordSmokeArgs<'_>) -> Result<()> {
    verify_files(&args)?;
    let root = TempRoot::create()?;

    for engine in ["surrealdb", "turso", "sqlite"] {
        for workload in ["point-read", "write-burst"] {
            let (command, output) = baseline_command(args.bench_bin, engine, workload, root.path());
            checked_result(
                command,
                &output,
                &format!("baseline/{engine}/{workload}"),
                7,
                64,
            )?;
        }
    }
    for workload in ["point-read", "write-burst"] {
        let (command, output) = baseline_command(
            args.rocks_bench_bin,
            "surrealdb-rocksdb",
            workload,
            root.path(),
        );
        checked_result(
            command,
            &output,
            &format!("baseline/surrealdb-rocksdb/{workload}"),
            7,
            64,
        )?;
    }

    for (binary, engine) in [
        (args.concurrency_bin, "surrealdb"),
        (args.rocks_concurrency_bin, "surrealdb-rocksdb"),
    ] {
        let (command, output) = concurrency_command(binary, engine, root.path());
        checked_result(
            command,
            &output,
            &format!("concurrency/{engine}/point-read-c2"),
            8,
            64,
        )?;
    }

    for engine in ["surrealdb", "turso", "sqlite"] {
        let (command, output) = sustained_command(args.sustained_bin, engine, root.path());
        checked_result(
            command,
            &output,
            &format!("sustained/{engine}/churn"),
            7,
            40,
        )?;
    }
    let (command, output) =
        sustained_command(args.rocks_sustained_bin, "surrealdb-rocksdb", root.path());
    checked_result(command, &output, "sustained/surrealdb-rocksdb/churn", 7, 40)?;

    println!("record runtime smoke complete: 14/14");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::value_as_i64;
    use serde_json::json;

    #[test]
    fn numeric_string_results_match_python_int_coercion() {
        assert_eq!(value_as_i64(Some(&json!("64"))), Some(64));
        assert_eq!(value_as_i64(Some(&json!(64))), Some(64));
        assert_eq!(value_as_i64(Some(&json!("nope"))), None);
    }
}
