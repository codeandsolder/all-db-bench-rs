use anyhow::{Context, Result, bail};
use serde_json::{Map, Number, Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub(crate) struct TsdbSummaryArgs<'a> {
    pub(crate) run_dir: &'a Path,
    pub(crate) expect_trials: Option<usize>,
    pub(crate) json_out: Option<&'a Path>,
    pub(crate) markdown_out: Option<&'a Path>,
}

fn read_cases(run_dir: &Path) -> Result<Vec<Value>> {
    let case_dir = run_dir.join("cases");
    let mut paths = Vec::<PathBuf>::new();
    if case_dir.is_dir() {
        for entry in fs::read_dir(&case_dir)
            .with_context(|| format!("failed to list {}", case_dir.display()))?
        {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) == Some("json") {
                paths.push(path);
            }
        }
    }
    paths.sort();
    let mut cases = Vec::with_capacity(paths.len());
    for path in paths {
        let text = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        cases.push(
            serde_json::from_str(&text)
                .with_context(|| format!("invalid JSON in {}", path.display()))?,
        );
    }
    if cases.is_empty() {
        bail!("no TSDB case results");
    }
    Ok(cases)
}

fn py_string(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        Value::Array(_) | Value::Object(_) => value.to_string(),
    }
}

fn field<'a>(value: &'a Value, path: &[&str]) -> Result<&'a Value> {
    let mut current = value;
    for key in path {
        current = current
            .get(*key)
            .with_context(|| format!("missing TSDB field {}", path.join(".")))?;
    }
    Ok(current)
}

fn float_field(value: &Value, path: &[&str]) -> Result<f64> {
    let raw = field(value, path)?;
    let parsed = match raw {
        Value::Bool(true) => 1.0,
        Value::Bool(false) => 0.0,
        Value::Number(number) => number
            .as_f64()
            .with_context(|| format!("invalid float field {}: {raw}", path.join(".")))?,
        Value::String(value) => value
            .trim()
            .parse::<f64>()
            .with_context(|| format!("invalid float field {}: {raw}", path.join(".")))?,
        Value::Null | Value::Array(_) | Value::Object(_) => {
            bail!("invalid float field {}: {raw}", path.join("."));
        }
    };
    Ok(parsed)
}

fn median(values: &mut [f64]) -> Result<f64> {
    if values.is_empty() {
        bail!("median of empty TSDB sample set");
    }
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        Ok((values[middle - 1] + values[middle]) / 2.0)
    } else {
        Ok(values[middle])
    }
}

fn number(value: f64) -> Result<Value> {
    Number::from_f64(value)
        .map(Value::Number)
        .with_context(|| format!("cannot encode non-finite TSDB value: {value}"))
}

fn insert_thousands(raw: &str) -> String {
    let (sign, digits) = raw
        .strip_prefix('-')
        .map_or(("", raw), |digits| ("-", digits));
    let mut grouped = String::with_capacity(raw.len() + raw.len() / 3);
    grouped.push_str(sign);
    let first = digits.len() % 3;
    if first != 0 {
        grouped.push_str(&digits[..first]);
        if digits.len() > first {
            grouped.push(',');
        }
    }
    for (index, chunk) in digits.as_bytes()[first..].chunks(3).enumerate() {
        if index > 0 {
            grouped.push(',');
        }
        grouped.push_str(std::str::from_utf8(chunk).unwrap_or_default());
    }
    grouped
}

fn fmt(value: f64, digits: usize) -> String {
    if value.is_nan() {
        return "—".to_owned();
    }
    if value.abs() >= 1000.0 {
        return insert_thousands(&format!("{value:.0}"));
    }
    format!("{value:.digits$}")
}

fn build_summary(cases: &[Value], expect_trials: Option<usize>) -> Result<Value> {
    let mut grouped = BTreeMap::<String, Vec<&Value>>::new();
    for case in cases {
        let lane = case
            .get("lane")
            .map_or_else(|| "None".to_owned(), py_string);
        if lane != "tsdb-server" {
            bail!("unexpected lane in result: {lane}");
        }
        let engine = py_string(field(case, &["engine"])?);
        grouped.entry(engine).or_default().push(case);
    }

    let mut problems = Vec::<Value>::new();
    let mut rows = Vec::<Value>::new();
    for (engine, items) in grouped {
        if let Some(expected) = expect_trials
            && items.len() != expected
        {
            problems.push(Value::String(format!(
                "{engine}: expected {expected} trials, got {}",
                items.len()
            )));
        }

        let mut ingest_rate = Vec::with_capacity(items.len());
        let mut startup = Vec::with_capacity(items.len());
        let mut cpu_per_sample = Vec::with_capacity(items.len());
        let mut write_per_sample = Vec::with_capacity(items.len());
        let mut disk_per_sample = Vec::with_capacity(items.len());
        let mut point_p99 = Vec::with_capacity(items.len());
        let mut range_p99 = Vec::with_capacity(items.len());
        let mut aggregate_p99 = Vec::with_capacity(items.len());

        for item in &items {
            let samples = float_field(item, &["total_samples"])?;
            if samples == 0.0 {
                bail!("TSDB total_samples must be non-zero");
            }
            ingest_rate.push(float_field(item, &["ingest", "samples_per_s"])?);
            startup.push(float_field(item, &["server_startup_s"])?);
            cpu_per_sample
                .push(float_field(item, &["server_process", "cpu_runtime_ns"])? / samples);
            write_per_sample.push(float_field(item, &["server_process", "write_bytes"])? / samples);
            disk_per_sample.push(float_field(item, &["server_data_bytes"])? / samples);
            point_p99.push(float_field(
                item,
                &["queries", "point", "latency", "p99_us"],
            )?);
            range_p99.push(float_field(
                item,
                &["queries", "range", "latency", "p99_us"],
            )?);
            aggregate_p99.push(float_field(
                item,
                &["queries", "aggregate", "latency", "p99_us"],
            )?);
        }

        let first = items
            .first()
            .context("internal TSDB group unexpectedly empty")?;
        let mut row = Map::new();
        row.insert("engine".to_owned(), Value::String(engine));
        row.insert(
            "version".to_owned(),
            field(first, &["engine_version"])?.clone(),
        );
        row.insert(
            "transport".to_owned(),
            field(first, &["transport"])?.clone(),
        );
        row.insert("trials".to_owned(), json!(items.len()));
        row.insert(
            "median_ingest_samples_per_s".to_owned(),
            number(median(&mut ingest_rate)?)?,
        );
        row.insert(
            "median_server_cpu_ns_per_sample".to_owned(),
            number(median(&mut cpu_per_sample)?)?,
        );
        row.insert(
            "median_server_write_bytes_per_sample".to_owned(),
            number(median(&mut write_per_sample)?)?,
        );
        row.insert(
            "median_data_bytes_per_sample".to_owned(),
            number(median(&mut disk_per_sample)?)?,
        );
        row.insert(
            "median_startup_s".to_owned(),
            number(median(&mut startup)?)?,
        );
        row.insert(
            "median_point_p99_us".to_owned(),
            number(median(&mut point_p99)?)?,
        );
        row.insert(
            "median_range_p99_us".to_owned(),
            number(median(&mut range_p99)?)?,
        );
        row.insert(
            "median_aggregate_p99_us".to_owned(),
            number(median(&mut aggregate_p99)?)?,
        );
        rows.push(Value::Object(row));
    }

    Ok(json!({
        "lane": "tsdb-server",
        "cases": cases.len(),
        "engines": rows.len(),
        "problems": problems,
        "rows": rows,
    }))
}

fn value_f64(row: &Value, key: &str) -> Result<f64> {
    row.get(key)
        .and_then(Value::as_f64)
        .with_context(|| format!("missing numeric TSDB summary field {key}"))
}

fn render_markdown(summary: &Value) -> Result<String> {
    let rows = summary
        .get("rows")
        .and_then(Value::as_array)
        .context("missing TSDB summary rows")?;
    let mut lines = vec![
        "# TSDB server benchmark summary".to_owned(),
        String::new(),
        "Do not merge this lane with embedded KV or record-product leaderboards. Ingestion transport is explicit because InfluxDB 3 uses native line protocol while the other three use Prometheus Remote Write v1.".to_owned(),
        String::new(),
        "| Engine | Trials | Ingest samples/s | Server CPU ns/sample | Server write B/sample | Data B/sample | Startup s | Point p99 µs | Range p99 µs | Aggregate p99 µs |".to_owned(),
        "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|".to_owned(),
    ];
    for row in rows {
        let engine = py_string(field(row, &["engine"])?);
        let version = py_string(field(row, &["version"])?);
        let trials = field(row, &["trials"])?;
        lines.push(format!(
            "| {engine} {version} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            py_string(trials),
            fmt(value_f64(row, "median_ingest_samples_per_s")?, 2),
            fmt(value_f64(row, "median_server_cpu_ns_per_sample")?, 2),
            fmt(value_f64(row, "median_server_write_bytes_per_sample")?, 2),
            fmt(value_f64(row, "median_data_bytes_per_sample")?, 2),
            fmt(value_f64(row, "median_startup_s")?, 3),
            fmt(value_f64(row, "median_point_p99_us")?, 2),
            fmt(value_f64(row, "median_range_p99_us")?, 2),
            fmt(value_f64(row, "median_aggregate_p99_us")?, 2),
        ));
    }
    let problems = summary
        .get("problems")
        .and_then(Value::as_array)
        .context("missing TSDB summary problems")?;
    if !problems.is_empty() {
        lines.push(String::new());
        lines.push("## Validation problems".to_owned());
        lines.push(String::new());
        for problem in problems {
            lines.push(format!("- {}", py_string(problem)));
        }
    }
    Ok(lines.join("\n") + "\n")
}

pub(crate) fn run(args: TsdbSummaryArgs<'_>) -> Result<i32> {
    let cases = read_cases(args.run_dir)?;
    let summary = build_summary(&cases, args.expect_trials)?;
    let json_text = serde_json::to_string_pretty(&summary)? + "\n";
    if let Some(path) = args.json_out {
        fs::write(path, &json_text)
            .with_context(|| format!("failed to write {}", path.display()))?;
    } else {
        print!("{json_text}");
    }

    let markdown = render_markdown(&summary)?;
    if let Some(path) = args.markdown_out {
        fs::write(path, &markdown)
            .with_context(|| format!("failed to write {}", path.display()))?;
    } else {
        println!("{markdown}");
    }

    let has_problems = summary
        .get("problems")
        .and_then(Value::as_array)
        .is_some_and(|problems| !problems.is_empty());
    Ok(i32::from(has_problems))
}

#[cfg(test)]
mod tests {
    use super::{build_summary, render_markdown};
    use anyhow::Result;
    use serde_json::{Value, json};

    fn case(engine: &str, trial: i64, scale: f64) -> Value {
        json!({
            "lane": "tsdb-server",
            "engine": engine,
            "engine_version": "1.0.0",
            "transport": "test-transport",
            "trial": trial,
            "total_samples": 100,
            "ingest": {"samples_per_s": 1000.0 * scale},
            "server_process": {
                "cpu_runtime_ns": 10_000.0 * scale,
                "write_bytes": 2_000.0 * scale,
            },
            "server_data_bytes": 5_000.0 * scale,
            "server_startup_s": 0.5 * scale,
            "queries": {
                "point": {"latency": {"p99_us": 10.0 * scale}},
                "range": {"latency": {"p99_us": 20.0 * scale}},
                "aggregate": {"latency": {"p99_us": 30.0 * scale}},
            },
        })
    }

    #[test]
    fn medians_and_expected_trials() -> Result<()> {
        let cases = vec![
            case("test", 1, 1.0),
            case("test", 2, 2.0),
            case("test", 3, 3.0),
        ];
        let summary = build_summary(&cases, Some(3))?;
        assert_eq!(summary["problems"], json!([]));
        let row = &summary["rows"][0];
        assert_eq!(row["median_ingest_samples_per_s"], json!(2000.0));
        assert_eq!(row["median_server_cpu_ns_per_sample"], json!(200.0));
        assert_eq!(row["median_data_bytes_per_sample"], json!(100.0));
        assert!(render_markdown(&summary)?.contains("TSDB server benchmark summary"));
        Ok(())
    }

    #[test]
    fn missing_trial_is_validation_problem() -> Result<()> {
        let summary = build_summary(&[case("test", 1, 1.0)], Some(2))?;
        assert_eq!(
            summary["problems"],
            json!(["test: expected 2 trials, got 1"])
        );
        Ok(())
    }

    #[test]
    fn rejects_wrong_lane() {
        let mut item = case("test", 1, 1.0);
        item["lane"] = json!("kv");
        let result = build_summary(&[item], None);
        assert!(result.is_err_and(|error| error.to_string().contains("unexpected lane")));
    }
}
