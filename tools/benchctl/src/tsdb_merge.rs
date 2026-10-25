use anyhow::{Context, Result};
use serde_json::{Map, Number, Value};
use std::{fs, path::Path};

const COUNTERS: [&str; 10] = [
    "cpu_runtime_ns",
    "runqueue_wait_ns",
    "timeslices",
    "rchar",
    "wchar",
    "syscr",
    "syscw",
    "read_bytes",
    "write_bytes",
    "cancelled_write_bytes",
];

#[derive(Debug)]
pub(crate) struct TsdbMergeArgs<'a> {
    pub(crate) client: &'a Path,
    pub(crate) server_before: &'a Path,
    pub(crate) server_after: &'a Path,
    pub(crate) data_dir: &'a Path,
    pub(crate) startup_s: f64,
    pub(crate) server_log: Option<&'a Path>,
    pub(crate) output: &'a Path,
}

fn load(path: &Path) -> Result<Value> {
    serde_json::from_str(
        &fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?,
    )
    .with_context(|| format!("invalid JSON in {}", path.display()))
}

fn py_int(value: &Value) -> Option<i64> {
    match value {
        Value::Bool(value) => Some(i64::from(*value)),
        Value::Number(number) => {
            if let Some(value) = number.as_i64() {
                Some(value)
            } else if let Some(value) = number.as_u64() {
                i64::try_from(value).ok()
            } else {
                let value = number.as_f64()?;
                if value.is_finite() && value >= i64::MIN as f64 && value <= i64::MAX as f64 {
                    Some(value.trunc() as i64)
                } else {
                    None
                }
            }
        }
        Value::String(value) => value.trim().parse::<i64>().ok(),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

fn int_or_zero(value: Option<&Value>) -> i64 {
    value.and_then(py_int).unwrap_or(0)
}

fn delta(after: i64, before: i64) -> i64 {
    (after - before).max(0)
}

fn directory_bytes(path: &Path) -> u64 {
    fn walk(path: &Path, total: &mut u64) {
        let Ok(metadata) = fs::symlink_metadata(path) else {
            return;
        };
        if metadata.file_type().is_symlink() {
            return;
        }
        if metadata.is_file() {
            *total = total.saturating_add(metadata.len());
            return;
        }
        if !metadata.is_dir() {
            return;
        }
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            walk(&entry.path(), total);
        }
    }

    if !path.exists() {
        return 0;
    }
    let mut total = 0;
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        walk(&entry.path(), &mut total);
    }
    total
}

fn number(value: f64) -> Result<Value> {
    Number::from_f64(value)
        .map(Value::Number)
        .with_context(|| format!("cannot encode non-finite TSDB merge value: {value}"))
}

fn merge(args: TsdbMergeArgs<'_>) -> Result<Value> {
    let mut result = load(args.client)?;
    let result_object = result
        .as_object_mut()
        .context("client result must be a JSON object")?;
    let before = load(args.server_before)?;
    let after = load(args.server_after)?;
    let before_wall = before
        .get("captured_monotonic_ns")
        .and_then(py_int)
        .context("invalid before captured_monotonic_ns")?;
    let after_wall = after
        .get("captured_monotonic_ns")
        .and_then(py_int)
        .context("invalid after captured_monotonic_ns")?;
    let wall_ns = delta(after_wall, before_wall);

    let mut server = Map::new();
    for key in COUNTERS {
        server.insert(
            key.to_owned(),
            Value::from(delta(
                int_or_zero(after.get(key)),
                int_or_zero(before.get(key)),
            )),
        );
    }
    let cpu_runtime = server
        .get("cpu_runtime_ns")
        .and_then(Value::as_i64)
        .context("missing cpu_runtime_ns")?;
    let runqueue = server
        .get("runqueue_wait_ns")
        .and_then(Value::as_i64)
        .context("missing runqueue_wait_ns")?;
    server.insert("accounting_wall_ns".to_owned(), Value::from(wall_ns));
    server.insert(
        "cpu_runtime_fraction_of_wall".to_owned(),
        number(if wall_ns == 0 {
            0.0
        } else {
            cpu_runtime as f64 / wall_ns as f64
        })?,
    );
    server.insert(
        "runqueue_wait_fraction_of_wall".to_owned(),
        number(if wall_ns == 0 {
            0.0
        } else {
            runqueue as f64 / wall_ns as f64
        })?,
    );
    server.insert(
        "rss_before_kib".to_owned(),
        Value::from(int_or_zero(before.get("rss_kib"))),
    );
    server.insert(
        "rss_after_kib".to_owned(),
        Value::from(int_or_zero(after.get("rss_kib"))),
    );
    server.insert(
        "peak_rss_after_kib".to_owned(),
        Value::from(int_or_zero(after.get("peak_rss_kib"))),
    );
    server.insert(
        "threads_before".to_owned(),
        Value::from(int_or_zero(before.get("threads"))),
    );
    server.insert(
        "threads_after".to_owned(),
        Value::from(int_or_zero(after.get("threads"))),
    );
    server.insert(
        "pids_before".to_owned(),
        before
            .get("pids")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new())),
    );
    server.insert(
        "pids_after".to_owned(),
        after
            .get("pids")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new())),
    );

    result_object.insert("server_startup_s".to_owned(), number(args.startup_s)?);
    result_object.insert("server_process".to_owned(), Value::Object(server));
    result_object.insert(
        "server_data_bytes".to_owned(),
        Value::from(directory_bytes(args.data_dir)),
    );
    if let Some(log) = args.server_log {
        let bytes = fs::metadata(log).map_or(0, |metadata| metadata.len());
        result_object.insert("server_log_bytes".to_owned(), Value::from(bytes));
    }
    Ok(result)
}

pub(crate) fn run(args: TsdbMergeArgs<'_>) -> Result<()> {
    let output = args.output;
    let result = merge(args)?;
    fs::write(output, serde_json::to_string_pretty(&result)? + "\n")
        .with_context(|| format!("failed to write {}", output.display()))
}

#[cfg(test)]
mod tests {
    use super::{TsdbMergeArgs, merge};
    use anyhow::Result;
    use serde_json::json;
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

    fn root() -> Result<PathBuf> {
        let suffix = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "all-db-bench-tsdb-merge-{}-{suffix}",
            std::process::id()
        ));
        if root.exists() {
            fs::remove_dir_all(&root)?;
        }
        fs::create_dir_all(&root)?;
        Ok(root)
    }

    #[test]
    fn server_deltas_and_data_size() -> Result<()> {
        let root = root()?;
        let client = root.join("client.json");
        let before = root.join("before.json");
        let after = root.join("after.json");
        let data = root.join("data");
        fs::create_dir(&data)?;
        fs::write(data.join("a"), vec![b'x'; 123])?;
        fs::write(
            &client,
            serde_json::to_string(&json!({"lane":"tsdb-server","engine":"test"}))?,
        )?;
        fs::write(
            &before,
            serde_json::to_string(&json!({
                "captured_monotonic_ns": 1000,
                "cpu_runtime_ns": 100,
                "runqueue_wait_ns": 20,
                "write_bytes": 1000,
                "rss_kib": 10,
                "peak_rss_kib": 12,
                "threads": 2,
                "pids": [1],
            }))?,
        )?;
        fs::write(
            &after,
            serde_json::to_string(&json!({
                "captured_monotonic_ns": 2000,
                "cpu_runtime_ns": 500,
                "runqueue_wait_ns": 120,
                "write_bytes": 1500,
                "rss_kib": 14,
                "peak_rss_kib": 16,
                "threads": 3,
                "pids": [1,2],
            }))?,
        )?;
        let output = root.join("out.json");
        let result = merge(TsdbMergeArgs {
            client: &client,
            server_before: &before,
            server_after: &after,
            data_dir: &data,
            startup_s: 0.25,
            server_log: None,
            output: &output,
        })?;
        assert_eq!(result["server_process"]["cpu_runtime_ns"], 400);
        assert_eq!(result["server_process"]["runqueue_wait_ns"], 100);
        assert_eq!(result["server_process"]["write_bytes"], 500);
        assert_eq!(result["server_process"]["accounting_wall_ns"], 1000);
        assert_eq!(result["server_process"]["rss_after_kib"], 14);
        assert_eq!(result["server_data_bytes"], 123);
        assert_eq!(result["server_startup_s"], json!(0.25));
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn deltas_saturate_and_symlinks_are_not_counted() -> Result<()> {
        let root = root()?;
        let client = root.join("client.json");
        let before = root.join("before.json");
        let after = root.join("after.json");
        let data = root.join("data");
        fs::create_dir(&data)?;
        fs::write(data.join("real"), vec![0_u8; 7])?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(data.join("real"), data.join("link"))?;
        fs::write(&client, "{}")?;
        fs::write(
            &before,
            r#"{"captured_monotonic_ns":200,"cpu_runtime_ns":20}"#,
        )?;
        fs::write(
            &after,
            r#"{"captured_monotonic_ns":100,"cpu_runtime_ns":10}"#,
        )?;
        let output = root.join("out.json");
        let result = merge(TsdbMergeArgs {
            client: &client,
            server_before: &before,
            server_after: &after,
            data_dir: &data,
            startup_s: 0.0,
            server_log: None,
            output: &output,
        })?;
        assert_eq!(result["server_process"]["accounting_wall_ns"], 0);
        assert_eq!(result["server_process"]["cpu_runtime_ns"], 0);
        assert_eq!(result["server_data_bytes"], 7);
        fs::remove_dir_all(root)?;
        Ok(())
    }
}
