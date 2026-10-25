use anyhow::{Context, Result, bail};
use serde_json::{Map, Number, Value, json};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub(crate) struct SustainedArgs<'a> {
    pub(crate) input: &'a Path,
    pub(crate) expect_trials: Option<usize>,
    pub(crate) json_out: Option<&'a Path>,
    pub(crate) markdown_out: Option<&'a Path>,
}

const NUMERIC_FIELDS: [&str; 31] = [
    "wall_ops_per_s",
    "active_ops_per_s",
    "instrumentation_fraction",
    "aggregate_cpu_ns_per_op",
    "aggregate_process_write_bytes_per_logical_mutated_byte",
    "baseline_ops_per_s",
    "window_ops_per_s_median",
    "window_ops_per_s_min",
    "tail_ops_per_s_median",
    "tail_throughput_ratio_vs_baseline",
    "throughput_cv",
    "normalized_throughput_slope_per_window",
    "min_throughput_ratio_vs_baseline",
    "windows_below_75pct_baseline",
    "windows_below_50pct_baseline",
    "windows_below_25pct_baseline",
    "longest_run_below_50pct_baseline",
    "baseline_p99_txn_us",
    "worst_p99_txn_us",
    "worst_p99_multiplier_vs_baseline",
    "cpu_ns_per_op_median_window",
    "cpu_ns_per_op_max_window",
    "process_write_bytes_per_logical_mutated_byte_median_window",
    "process_write_bytes_per_logical_mutated_byte_max_window",
    "runqueue_wait_fraction_median_window",
    "runqueue_wait_fraction_max_window",
    "cpu_psi_some_fraction_median_window",
    "cpu_psi_some_fraction_max_window",
    "io_psi_full_fraction_median_window",
    "io_psi_full_fraction_max_window",
    "foreground_db_growth_bytes",
];

const IDENTITY_FIELDS: [&str; 19] = [
    "format_version",
    "lane",
    "scenario",
    "engine",
    "engine_version",
    "durability",
    "durability_mapping",
    "pattern",
    "records",
    "ops_requested",
    "window_ops",
    "value_bytes",
    "payload_bytes",
    "value_pattern",
    "key_bytes",
    "key_shape",
    "txn_size",
    "read_materialization",
    "write_materialization",
];

fn f64_value(value: f64) -> Result<Value> {
    Number::from_f64(value)
        .map(Value::Number)
        .with_context(|| format!("cannot encode non-finite sustained value: {value}"))
}

fn as_f64(value: Option<&Value>, default: f64) -> Result<f64> {
    match value {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Number(number)) => number
            .as_f64()
            .context("numeric sustained field was not representable as f64"),
        Some(Value::String(raw)) => raw
            .parse::<f64>()
            .with_context(|| format!("invalid sustained float {raw:?}")),
        Some(Value::Bool(value)) => Ok(if *value { 1.0 } else { 0.0 }),
        Some(other) => bail!("invalid sustained numeric value: {other}"),
    }
}

fn as_i64(value: Option<&Value>, default: i64) -> Result<i64> {
    match value {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Number(number)) => number
            .as_i64()
            .or_else(|| number.as_u64().and_then(|value| i64::try_from(value).ok()))
            .context("numeric sustained field was not representable as i64"),
        Some(Value::String(raw)) => raw
            .parse::<i64>()
            .with_context(|| format!("invalid sustained integer {raw:?}")),
        Some(Value::Bool(value)) => Ok(i64::from(*value)),
        Some(other) => bail!("invalid sustained integer value: {other}"),
    }
}

fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    Some(if values.len().is_multiple_of(2) {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    })
}

fn quantile(values: &[f64], q: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    if sorted.len() == 1 {
        return sorted.first().copied();
    }
    let position = q * (sorted.len() - 1) as f64;
    let low = position.floor() as usize;
    let high = position.ceil() as usize;
    if low == high {
        Some(sorted[low])
    } else {
        let fraction = position - low as f64;
        Some(sorted[low] * (1.0 - fraction) + sorted[high] * fraction)
    }
}

fn ratio(numerator: f64, denominator: f64) -> Option<f64> {
    (denominator > 0.0).then_some(numerator / denominator)
}

fn longest_run(flags: &[bool]) -> usize {
    let mut best = 0;
    let mut current = 0;
    for flag in flags {
        if *flag {
            current += 1;
            best = best.max(current);
        } else {
            current = 0;
        }
    }
    best
}

fn fraction(delta: &Value, numerator: &str) -> Result<f64> {
    let wall_ns = as_f64(delta.get("accounting_wall_ns"), 0.0)?.max(1.0);
    let raw = as_f64(delta.get(numerator), 0.0)?;
    Ok(if numerator.ends_with("_us") {
        raw * 1000.0 / wall_ns
    } else {
        raw / wall_ns
    })
}

fn object_or_empty(value: Option<&Value>) -> Value {
    value
        .filter(|value| value.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}))
}

fn summarize_case(raw: &Value) -> Result<Value> {
    let windows = raw
        .get("windows")
        .and_then(Value::as_array)
        .context("sustained result has no windows")?;
    if windows.is_empty() {
        bail!("sustained result has no windows");
    }

    let rates = windows
        .iter()
        .map(|window| as_f64(window.get("ops_per_s"), 0.0))
        .collect::<Result<Vec<_>>>()?;
    let p99 = windows
        .iter()
        .map(|window| {
            let latency = window
                .get("transaction_latency")
                .context("sustained window missing transaction_latency")?;
            as_f64(latency.get("p99_us"), 0.0)
        })
        .collect::<Result<Vec<_>>>()?;
    let baseline_count = windows.len().min(3);
    let mut baseline_rates = rates[..baseline_count].to_vec();
    let mut baseline_p99s = p99[..baseline_count].to_vec();
    let baseline_rate = median(&mut baseline_rates).context("empty sustained baseline")?;
    let baseline_p99 = median(&mut baseline_p99s).context("empty sustained p99 baseline")?;
    if baseline_rate <= 0.0 {
        bail!("sustained baseline throughput must be positive");
    }
    let rate_ratio = rates
        .iter()
        .map(|rate| *rate / baseline_rate)
        .collect::<Vec<_>>();
    let tail_count = rates.len().min(3);
    let mut tail_rates = rates[rates.len() - tail_count..].to_vec();
    let tail_rate = median(&mut tail_rates).context("empty sustained tail")?;
    let mean_rate = rates.iter().sum::<f64>() / rates.len() as f64;
    let throughput_cv = if rates.len() > 1 && mean_rate > 0.0 {
        let variance = rates
            .iter()
            .map(|rate| (*rate - mean_rate).powi(2))
            .sum::<f64>()
            / rates.len() as f64;
        variance.sqrt() / mean_rate
    } else {
        0.0
    };
    let normalized_slope = if rates.len() > 1 {
        let x_mean = (rates.len() - 1) as f64 / 2.0;
        let denominator = (0..rates.len())
            .map(|index| (index as f64 - x_mean).powi(2))
            .sum::<f64>();
        let slope = rates
            .iter()
            .enumerate()
            .map(|(index, rate)| (index as f64 - x_mean) * (*rate - mean_rate))
            .sum::<f64>()
            / denominator;
        slope / baseline_rate
    } else {
        0.0
    };
    let below75 = rate_ratio
        .iter()
        .map(|value| *value < 0.75)
        .collect::<Vec<_>>();
    let below50 = rate_ratio
        .iter()
        .map(|value| *value < 0.50)
        .collect::<Vec<_>>();
    let below25 = rate_ratio
        .iter()
        .map(|value| *value < 0.25)
        .collect::<Vec<_>>();
    let first50 = below50.iter().position(|flag| *flag);
    let (recovered90, max_after_first50) = if let Some(first) = first50 {
        let later = &rate_ratio[first + 1..];
        let recovered = later
            .iter()
            .position(|value| *value >= 0.90)
            .map(|index| first + 1 + index);
        let max_after = later.iter().copied().reduce(f64::max);
        (recovered, max_after)
    } else {
        (None, None)
    };

    let mut cpu_ns_per_op = Vec::new();
    let mut write_amp = Vec::new();
    let mut runqueue = Vec::new();
    let mut cpu_psi = Vec::new();
    let mut io_psi_full = Vec::new();
    for window in windows {
        let ops = as_i64(window.get("ops_completed"), 0)?.max(1) as f64;
        let logical = as_i64(window.get("logical_mutated_bytes"), 0)?.max(1) as f64;
        let process = object_or_empty(window.get("process"));
        let system = object_or_empty(window.get("system"));
        cpu_ns_per_op.push(as_f64(process.get("cpu_runtime_ns"), 0.0)? / ops);
        write_amp.push(as_f64(process.get("write_bytes"), 0.0)? / logical);
        runqueue.push(as_f64(process.get("runqueue_wait_fraction_of_wall"), 0.0)?);
        cpu_psi.push(fraction(&system, "psi_cpu_some_us")?);
        io_psi_full.push(fraction(&system, "psi_io_full_us")?);
    }

    let settle = raw.get("post_workload_settle");
    let settle_summary = if settle
        .is_some_and(|value| value.as_object().is_some_and(|map| !map.is_empty()))
    {
        let settle = settle.context("missing settle")?;
        let process = object_or_empty(settle.get("process"));
        Some(json!({
            "elapsed_s": as_f64(settle.get("elapsed_s"), 0.0)?,
            "cpu_runtime_ns": as_i64(process.get("cpu_runtime_ns"), 0)?,
            "write_bytes": as_i64(process.get("write_bytes"), 0)?,
            "db_bytes_delta": as_i64(settle.get("db_bytes_after"), 0)? - as_i64(settle.get("db_bytes_before"), 0)?,
            "sample_count": settle.get("samples").and_then(Value::as_array).map_or(0, Vec::len),
        }))
    } else {
        None
    };

    let measured_process = object_or_empty(raw.get("measured_process"));
    let completed = as_i64(raw.get("ops_completed"), 0)?.max(1) as f64;
    let logical = as_i64(raw.get("logical_mutated_bytes"), 0)?.max(1) as f64;
    let mut case = Map::new();
    for field in [
        "format_version",
        "lane",
        "scenario",
        "engine",
        "engine_version",
        "durability",
        "durability_mapping",
        "pattern",
        "records",
        "ops_requested",
        "ops_completed",
        "window_ops",
        "value_bytes",
        "payload_bytes",
        "value_pattern",
        "key_bytes",
        "key_shape",
        "txn_size",
        "trial",
    ] {
        case.insert(
            field.to_owned(),
            raw.get(field).cloned().unwrap_or(Value::Null),
        );
    }
    case.insert(
        "read_materialization".to_owned(),
        raw.get("read_materialization")
            .cloned()
            .unwrap_or_else(|| Value::String("legacy-read-v0".to_owned())),
    );
    case.insert(
        "write_materialization".to_owned(),
        raw.get("write_materialization")
            .cloned()
            .unwrap_or_else(|| Value::String("legacy-return-v0".to_owned())),
    );
    case.insert("window_count".to_owned(), json!(windows.len()));
    case.insert(
        "wall_ops_per_s".to_owned(),
        f64_value(as_f64(raw.get("ops_per_s"), 0.0)?)?,
    );
    case.insert(
        "active_ops_per_s".to_owned(),
        f64_value(as_f64(raw.get("active_ops_per_s"), 0.0)?)?,
    );
    case.insert(
        "instrumentation_fraction".to_owned(),
        f64_value(as_f64(raw.get("instrumentation_fraction_of_wall"), 0.0)?)?,
    );
    case.insert(
        "aggregate_cpu_ns_per_op".to_owned(),
        f64_value(as_f64(measured_process.get("cpu_runtime_ns"), 0.0)? / completed)?,
    );
    case.insert(
        "aggregate_process_write_bytes_per_logical_mutated_byte".to_owned(),
        f64_value(as_f64(measured_process.get("write_bytes"), 0.0)? / logical)?,
    );
    case.insert("baseline_windows".to_owned(), json!(baseline_count));
    case.insert("baseline_ops_per_s".to_owned(), f64_value(baseline_rate)?);
    let mut median_rates = rates.clone();
    case.insert(
        "window_ops_per_s_median".to_owned(),
        f64_value(median(&mut median_rates).context("empty sustained rates")?)?,
    );
    case.insert(
        "window_ops_per_s_q1".to_owned(),
        f64_value(quantile(&rates, 0.25).context("empty sustained q1")?)?,
    );
    case.insert(
        "window_ops_per_s_q3".to_owned(),
        f64_value(quantile(&rates, 0.75).context("empty sustained q3")?)?,
    );
    case.insert(
        "window_ops_per_s_min".to_owned(),
        f64_value(
            rates
                .iter()
                .copied()
                .reduce(f64::min)
                .context("empty rates")?,
        )?,
    );
    case.insert(
        "window_ops_per_s_max".to_owned(),
        f64_value(
            rates
                .iter()
                .copied()
                .reduce(f64::max)
                .context("empty rates")?,
        )?,
    );
    case.insert("tail_windows".to_owned(), json!(tail_count));
    case.insert("tail_ops_per_s_median".to_owned(), f64_value(tail_rate)?);
    case.insert(
        "tail_throughput_ratio_vs_baseline".to_owned(),
        f64_value(ratio(tail_rate, baseline_rate).context("zero baseline")?)?,
    );
    case.insert("throughput_cv".to_owned(), f64_value(throughput_cv)?);
    case.insert(
        "normalized_throughput_slope_per_window".to_owned(),
        f64_value(normalized_slope)?,
    );
    case.insert(
        "min_throughput_ratio_vs_baseline".to_owned(),
        f64_value(
            rate_ratio
                .iter()
                .copied()
                .reduce(f64::min)
                .context("empty ratios")?,
        )?,
    );
    case.insert(
        "windows_below_75pct_baseline".to_owned(),
        json!(below75.iter().filter(|flag| **flag).count()),
    );
    case.insert(
        "windows_below_50pct_baseline".to_owned(),
        json!(below50.iter().filter(|flag| **flag).count()),
    );
    case.insert(
        "windows_below_25pct_baseline".to_owned(),
        json!(below25.iter().filter(|flag| **flag).count()),
    );
    case.insert(
        "longest_run_below_50pct_baseline".to_owned(),
        json!(longest_run(&below50)),
    );
    case.insert(
        "first_below_50pct_window".to_owned(),
        first50.map_or(Value::Null, |value| json!(value)),
    );
    case.insert(
        "first_below_50pct_ops_start".to_owned(),
        first50
            .and_then(|index| windows.get(index))
            .and_then(|window| window.get("ops_start"))
            .cloned()
            .unwrap_or(Value::Null),
    );
    case.insert(
        "first_recovered_90pct_window".to_owned(),
        recovered90.map_or(Value::Null, |value| json!(value)),
    );
    case.insert(
        "max_throughput_ratio_after_first_50pct_cliff".to_owned(),
        match max_after_first50 {
            Some(value) => f64_value(value)?,
            None => Value::Null,
        },
    );
    case.insert("baseline_p99_txn_us".to_owned(), f64_value(baseline_p99)?);
    case.insert(
        "worst_p99_txn_us".to_owned(),
        f64_value(p99.iter().copied().reduce(f64::max).context("empty p99")?)?,
    );
    case.insert(
        "worst_p99_multiplier_vs_baseline".to_owned(),
        f64_value(
            ratio(
                p99.iter().copied().reduce(f64::max).context("empty p99")?,
                baseline_p99,
            )
            .context("zero p99 baseline")?,
        )?,
    );
    for (name, mut values, max_name) in [
        (
            "cpu_ns_per_op_median_window",
            cpu_ns_per_op,
            "cpu_ns_per_op_max_window",
        ),
        (
            "process_write_bytes_per_logical_mutated_byte_median_window",
            write_amp,
            "process_write_bytes_per_logical_mutated_byte_max_window",
        ),
        (
            "runqueue_wait_fraction_median_window",
            runqueue,
            "runqueue_wait_fraction_max_window",
        ),
        (
            "cpu_psi_some_fraction_median_window",
            cpu_psi,
            "cpu_psi_some_fraction_max_window",
        ),
        (
            "io_psi_full_fraction_median_window",
            io_psi_full,
            "io_psi_full_fraction_max_window",
        ),
    ] {
        let maximum = values
            .iter()
            .copied()
            .reduce(f64::max)
            .context("empty sustained metric")?;
        let med = median(&mut values).context("empty sustained metric")?;
        case.insert(name.to_owned(), f64_value(med)?);
        case.insert(max_name.to_owned(), f64_value(maximum)?);
    }
    let db_before = as_i64(raw.get("db_bytes_before"), 0)?;
    let db_foreground = as_i64(raw.get("db_bytes_after_foreground"), 0)?;
    case.insert("db_bytes_before".to_owned(), json!(db_before));
    case.insert("db_bytes_after_foreground".to_owned(), json!(db_foreground));
    case.insert(
        "db_bytes_final".to_owned(),
        json!(as_i64(raw.get("db_bytes_final"), 0)?),
    );
    case.insert(
        "foreground_db_growth_bytes".to_owned(),
        json!(db_foreground - db_before),
    );
    case.insert("settle".to_owned(), settle_summary.unwrap_or(Value::Null));
    Ok(Value::Object(case))
}

fn group_key(case: &Value) -> Vec<Value> {
    IDENTITY_FIELDS
        .iter()
        .map(|field| {
            case.get(*field).cloned().unwrap_or_else(|| match *field {
                "read_materialization" => Value::String("legacy-read-v0".to_owned()),
                "write_materialization" => Value::String("legacy-return-v0".to_owned()),
                _ => Value::Null,
            })
        })
        .collect()
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

fn aggregate(cases: &[Value]) -> Result<Vec<Value>> {
    let mut groups = HashMap::<String, (Vec<String>, Vec<&Value>)>::new();
    for case in cases {
        let key = group_key(case);
        let serialized = serde_json::to_string(&key)?;
        let sort_key = key.iter().map(py_string).collect::<Vec<_>>();
        groups
            .entry(serialized)
            .or_insert_with(|| (sort_key, Vec::new()))
            .1
            .push(case);
    }
    let mut groups = groups.into_values().collect::<Vec<_>>();
    groups.sort_by(|left, right| left.0.cmp(&right.0));
    let mut output = Vec::new();
    for (_, rows) in groups {
        let first = rows.first().context("empty sustained aggregate group")?;
        let mut group = Map::new();
        for field in IDENTITY_FIELDS {
            group.insert(
                field.to_owned(),
                first.get(field).cloned().unwrap_or(Value::Null),
            );
        }
        group.insert("trials".to_owned(), json!(rows.len()));
        for field in NUMERIC_FIELDS {
            if group.contains_key(&format!("{field}_median")) {
                continue;
            }
            let mut values = rows
                .iter()
                .filter_map(|row| row.get(field))
                .filter(|value| !value.is_null())
                .map(|value| as_f64(Some(value), 0.0))
                .collect::<Result<Vec<_>>>()?;
            let med = median(&mut values);
            group.insert(
                format!("{field}_median"),
                match med {
                    Some(value) => f64_value(value)?,
                    None => Value::Null,
                },
            );
            if !values.is_empty() {
                group.insert(
                    format!("{field}_min"),
                    f64_value(
                        values
                            .iter()
                            .copied()
                            .reduce(f64::min)
                            .context("empty numeric")?,
                    )?,
                );
                group.insert(
                    format!("{field}_max"),
                    f64_value(
                        values
                            .iter()
                            .copied()
                            .reduce(f64::max)
                            .context("empty numeric")?,
                    )?,
                );
            }
        }
        let cliffs = rows
            .iter()
            .filter_map(|row| row.get("first_below_50pct_window"))
            .filter_map(Value::as_f64)
            .collect::<Vec<_>>();
        group.insert("trials_with_50pct_cliff".to_owned(), json!(cliffs.len()));
        let mut cliff_copy = cliffs.clone();
        group.insert(
            "first_below_50pct_window_median_when_present".to_owned(),
            match median(&mut cliff_copy) {
                Some(value) => f64_value(value)?,
                None => Value::Null,
            },
        );
        group.insert(
            "trials_recovered_to_90pct_after_cliff".to_owned(),
            json!(
                rows.iter()
                    .filter(|row| {
                        row.get("first_below_50pct_window")
                            .is_some_and(|value| !value.is_null())
                            && row
                                .get("first_recovered_90pct_window")
                                .is_some_and(|value| !value.is_null())
                    })
                    .count()
            ),
        );
        for (field, out) in [
            ("cpu_runtime_ns", "settle_cpu_runtime_ns_median"),
            ("write_bytes", "settle_write_bytes_median"),
            ("db_bytes_delta", "settle_db_bytes_delta_median"),
        ] {
            let mut values = rows
                .iter()
                .filter_map(|row| row.get("settle"))
                .filter_map(Value::as_object)
                .filter_map(|settle| settle.get(field))
                .map(|value| as_f64(Some(value), 0.0))
                .collect::<Result<Vec<_>>>()?;
            group.insert(
                out.to_owned(),
                match median(&mut values) {
                    Some(value) => f64_value(value)?,
                    None => Value::Null,
                },
            );
        }
        output.push(Value::Object(group));
    }
    Ok(output)
}

fn read_input(path: &Path) -> Result<Vec<Value>> {
    if path.is_dir() {
        let case_dir = path.join("cases");
        if !case_dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut paths = fs::read_dir(&case_dir)
            .with_context(|| format!("failed to list {}", case_dir.display()))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
            .collect::<Vec<PathBuf>>();
        paths.sort();
        paths
            .into_iter()
            .map(|path| {
                serde_json::from_str(&fs::read_to_string(&path)?)
                    .with_context(|| format!("invalid JSON in {}", path.display()))
            })
            .collect()
    } else if path
        .file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|name| name.ends_with(".ndjson"))
    {
        let text = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).context("invalid sustained NDJSON row"))
            .collect()
    } else {
        Ok(vec![
            serde_json::from_str(
                &fs::read_to_string(path)
                    .with_context(|| format!("failed to read {}", path.display()))?,
            )
            .with_context(|| format!("invalid JSON in {}", path.display()))?,
        ])
    }
}

fn fmt(value: Option<&Value>, digits: usize) -> String {
    let Some(value) = value else {
        return "—".to_owned();
    };
    if value.is_null() {
        return "—".to_owned();
    }
    if let Some(number) = value.as_f64() {
        return format!("{number:.digits$}");
    }
    py_string(value)
}

fn render_markdown(groups: &[Value]) -> Result<String> {
    let mut lines = vec![
        "# Sustained-write / compaction summary".to_owned(),
        String::new(),
        "Threshold columns are diagnostics relative to each trial's first three windows; they are not pass/fail criteria.".to_owned(),
        "Cases with >10% between-window instrumentation overhead are warned in summary.json and need larger windows before performance/cliff magnitude is interpreted.".to_owned(),
        String::new(),
        "| lane | scenario | engine | dur | pattern | trials | wall ops/s | tail/base | slope/window | CV | min/base | 50% cliff trials | longest <50% | worst p99/base | write B/logical B agg | write B/logical B max-win | rq max % | IO PSI full max % | instr % | settle write MiB |".to_owned(),
        "|---|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|".to_owned(),
    ];
    for group in groups {
        let settle = group
            .get("settle_write_bytes_median")
            .and_then(Value::as_f64)
            .map(|value| value / 1024.0 / 1024.0);
        let percent = |key: &str| {
            group
                .get(key)
                .and_then(Value::as_f64)
                .map(|value| value * 100.0)
        };
        lines.push(format!(
            "| {} | {} | {} {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            py_string(group.get("lane").unwrap_or(&Value::Null)),
            py_string(group.get("scenario").unwrap_or(&Value::Null)),
            py_string(group.get("engine").unwrap_or(&Value::Null)),
            py_string(group.get("engine_version").unwrap_or(&Value::Null)),
            py_string(group.get("durability").unwrap_or(&Value::Null)),
            py_string(group.get("pattern").unwrap_or(&Value::Null)),
            py_string(group.get("trials").unwrap_or(&Value::Null)),
            fmt(group.get("wall_ops_per_s_median"), 3),
            fmt(group.get("tail_throughput_ratio_vs_baseline_median"), 3),
            fmt(group.get("normalized_throughput_slope_per_window_median"), 3),
            fmt(group.get("throughput_cv_median"), 3),
            fmt(group.get("min_throughput_ratio_vs_baseline_median"), 3),
            py_string(group.get("trials_with_50pct_cliff").unwrap_or(&Value::Null)),
            fmt(group.get("longest_run_below_50pct_baseline_median"), 3),
            fmt(group.get("worst_p99_multiplier_vs_baseline_median"), 3),
            fmt(group.get("aggregate_process_write_bytes_per_logical_mutated_byte_median"), 3),
            fmt(group.get("process_write_bytes_per_logical_mutated_byte_max_window_median"), 3),
            percent("runqueue_wait_fraction_max_window_median").map_or_else(|| "—".to_owned(), |value| format!("{value:.3}")),
            percent("io_psi_full_fraction_max_window_median").map_or_else(|| "—".to_owned(), |value| format!("{value:.3}")),
            percent("instrumentation_fraction_median").map_or_else(|| "—".to_owned(), |value| format!("{value:.3}")),
            settle.map_or_else(|| "—".to_owned(), |value| format!("{value:.3}")),
        ));
    }
    Ok(lines.join("\n") + "\n")
}

pub(crate) fn run(args: SustainedArgs<'_>) -> Result<i32> {
    let raw = read_input(args.input)?;
    let raw = raw
        .into_iter()
        .filter(|row| {
            matches!(
                row.get("lane").and_then(Value::as_str),
                Some("kv-sustained" | "record-sustained")
            )
        })
        .collect::<Vec<_>>();
    let cases = raw.iter().map(summarize_case).collect::<Result<Vec<_>>>()?;
    let groups = aggregate(&cases)?;
    let mut problems = Vec::<Value>::new();
    let mut warnings = Vec::<Value>::new();
    for case in &cases {
        let instrumentation = case
            .get("instrumentation_fraction")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        if instrumentation > 0.10 {
            warnings.push(Value::String(format!(
                "{} {} {} {} trial={}: instrumentation is {:.1}% of sustained foreground wall time; use larger windows before interpreting cliff/performance magnitude",
                py_string(case.get("scenario").unwrap_or(&Value::Null)),
                py_string(case.get("engine").unwrap_or(&Value::Null)),
                py_string(case.get("durability").unwrap_or(&Value::Null)),
                py_string(case.get("pattern").unwrap_or(&Value::Null)),
                py_string(case.get("trial").unwrap_or(&Value::Null)),
                100.0 * instrumentation,
            )));
        }
    }
    if let Some(expected) = args.expect_trials {
        for group in &groups {
            if group.get("trials").and_then(Value::as_u64) != Some(expected as u64) {
                problems.push(Value::String(format!(
                    "{} {} {} {}: {} trials, expected {expected}",
                    py_string(group.get("scenario").unwrap_or(&Value::Null)),
                    py_string(group.get("engine").unwrap_or(&Value::Null)),
                    py_string(group.get("durability").unwrap_or(&Value::Null)),
                    py_string(group.get("pattern").unwrap_or(&Value::Null)),
                    py_string(group.get("trials").unwrap_or(&Value::Null)),
                )));
            }
        }
    }
    let result = json!({
        "case_count": cases.len(),
        "group_count": groups.len(),
        "problems": problems,
        "warnings": warnings,
        "cases": cases,
        "groups": groups,
    });
    let json_text = serde_json::to_string_pretty(&result)? + "\n";
    if let Some(path) = args.json_out {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, json_text)?;
    } else {
        print!("{json_text}");
    }
    if let Some(path) = args.markdown_out {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(
            path,
            render_markdown(result["groups"].as_array().context("groups missing")?)?,
        )?;
    }
    Ok(i32::from(
        result["problems"]
            .as_array()
            .is_some_and(|problems| !problems.is_empty()),
    ))
}

#[cfg(test)]
mod tests {
    use super::{aggregate, group_key, summarize_case};
    use anyhow::Result;
    use serde_json::{Value, json};

    fn identity_case() -> Value {
        json!({
            "format_version": 7,
            "lane": "record-sustained",
            "scenario": "record-sustained",
            "engine": "sqlite",
            "engine_version": "1",
            "durability": "sync",
            "durability_mapping": "full",
            "pattern": "append",
            "records": 100,
            "ops_requested": 1000,
            "window_ops": 100,
            "value_bytes": null,
            "payload_bytes": 512,
            "value_pattern": null,
            "key_bytes": null,
            "key_shape": null,
            "txn_size": 100,
            "read_materialization": "full-record-v1",
            "write_materialization": "no-return-v1",
        })
    }

    fn raw_case(trial: i64, instrumentation: f64) -> Value {
        json!({
            "format_version": 7,
            "lane": "record-sustained",
            "scenario": "record-sustained",
            "engine": "sqlite",
            "engine_version": "1",
            "durability": "sync",
            "durability_mapping": "full",
            "pattern": "append",
            "records": 100,
            "ops_requested": 1000,
            "ops_completed": 1000,
            "window_ops": 100,
            "value_bytes": null,
            "payload_bytes": 512,
            "value_pattern": null,
            "key_bytes": null,
            "key_shape": null,
            "txn_size": 100,
            "read_materialization": "full-record-v1",
            "write_materialization": "no-return-v1",
            "trial": trial,
            "ops_per_s": 80.0,
            "active_ops_per_s": 90.0,
            "instrumentation_fraction_of_wall": instrumentation,
            "logical_mutated_bytes": 10000,
            "measured_process": {"cpu_runtime_ns": 100000, "write_bytes": 20000},
            "db_bytes_before": 1000,
            "db_bytes_after_foreground": 2000,
            "db_bytes_final": 2200,
            "windows": [
                {"ops_start":0,"ops_completed":100,"ops_per_s":100.0,"logical_mutated_bytes":1000,"transaction_latency":{"p99_us":10.0},"process":{"cpu_runtime_ns":1000,"write_bytes":2000,"runqueue_wait_fraction_of_wall":0.1},"system":{"accounting_wall_ns":1000000,"psi_cpu_some_us":10,"psi_io_full_us":5}},
                {"ops_start":100,"ops_completed":100,"ops_per_s":100.0,"logical_mutated_bytes":1000,"transaction_latency":{"p99_us":12.0},"process":{"cpu_runtime_ns":1100,"write_bytes":2100,"runqueue_wait_fraction_of_wall":0.2},"system":{"accounting_wall_ns":1000000,"psi_cpu_some_us":20,"psi_io_full_us":6}},
                {"ops_start":200,"ops_completed":100,"ops_per_s":100.0,"logical_mutated_bytes":1000,"transaction_latency":{"p99_us":11.0},"process":{"cpu_runtime_ns":1200,"write_bytes":2200,"runqueue_wait_fraction_of_wall":0.3},"system":{"accounting_wall_ns":1000000,"psi_cpu_some_us":30,"psi_io_full_us":7}},
                {"ops_start":300,"ops_completed":100,"ops_per_s":40.0,"logical_mutated_bytes":1000,"transaction_latency":{"p99_us":30.0},"process":{"cpu_runtime_ns":1300,"write_bytes":2300,"runqueue_wait_fraction_of_wall":0.4},"system":{"accounting_wall_ns":1000000,"psi_cpu_some_us":40,"psi_io_full_us":8}},
                {"ops_start":400,"ops_completed":100,"ops_per_s":95.0,"logical_mutated_bytes":1000,"transaction_latency":{"p99_us":15.0},"process":{"cpu_runtime_ns":1400,"write_bytes":2400,"runqueue_wait_fraction_of_wall":0.5},"system":{"accounting_wall_ns":1000000,"psi_cpu_some_us":50,"psi_io_full_us":9}}
            ],
            "post_workload_settle": {"elapsed_s":1.0,"process":{"cpu_runtime_ns":50,"write_bytes":500},"db_bytes_before":2000,"db_bytes_after":2100,"samples":[1,2]}
        })
    }

    #[test]
    fn materialization_markers_are_group_identity() {
        let corrected = identity_case();
        let mut old_read = corrected.clone();
        old_read["read_materialization"] = json!("legacy-read-v0");
        let mut old_write = corrected.clone();
        old_write["write_materialization"] = json!("legacy-return-v0");
        assert_ne!(group_key(&corrected), group_key(&old_read));
        assert_ne!(group_key(&corrected), group_key(&old_write));
    }

    #[test]
    fn missing_markers_default_to_legacy() {
        let mut missing = identity_case();
        missing
            .as_object_mut()
            .and_then(|map| map.remove("read_materialization"));
        missing
            .as_object_mut()
            .and_then(|map| map.remove("write_materialization"));
        let mut legacy = identity_case();
        legacy["read_materialization"] = json!("legacy-read-v0");
        legacy["write_materialization"] = json!("legacy-return-v0");
        assert_eq!(group_key(&missing), group_key(&legacy));
    }

    #[test]
    fn cliff_recovery_and_aggregation_are_preserved() -> Result<()> {
        let first = summarize_case(&raw_case(1, 0.12))?;
        let second = summarize_case(&raw_case(2, 0.05))?;
        assert_eq!(first["first_below_50pct_window"], 3);
        assert_eq!(first["first_recovered_90pct_window"], 4);
        assert_eq!(first["longest_run_below_50pct_baseline"], 1);
        let groups = aggregate(&[first, second])?;
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["trials"], 2);
        assert_eq!(groups[0]["trials_with_50pct_cliff"], 2);
        Ok(())
    }
}
