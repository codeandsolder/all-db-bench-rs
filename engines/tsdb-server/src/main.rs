use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use hdrhistogram::Histogram;
use prost::Message;
use reqwest::blocking::{Client, Response};
use serde::Serialize;
use serde_json::Value;
use snap::raw::Encoder as SnappyEncoder;
#[path = "../../../src/metrics.rs"]
mod metrics;
use metrics::{ProcDelta, ProcSnapshot, SystemDelta, SystemSnapshot};
use std::{
    fs,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const HIST_MAX_NS: u64 = 120_000_000_000;
const METRIC_NAME: &str = "bench_metric";
const INFLUX_DATABASE: &str = "bench";

#[derive(Clone, Copy, Debug, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
enum EngineKind {
    Greptimedb,
    Victoriametrics,
    Prometheus,
    Influxdb3,
}

impl EngineKind {
    fn transport(self) -> &'static str {
        match self {
            Self::Greptimedb | Self::Victoriametrics | Self::Prometheus => {
                "prometheus-remote-write-v1-snappy-protobuf"
            }
            Self::Influxdb3 => "influxdb-line-protocol-v3-http",
        }
    }

    fn query_language(self) -> &'static str {
        match self {
            Self::Greptimedb | Self::Victoriametrics | Self::Prometheus => "promql",
            Self::Influxdb3 => "sql",
        }
    }
}

#[derive(Debug, Parser)]
#[command(about = "Benchmark one already-running time-series database server")]
struct Args {
    #[arg(long, value_enum)]
    engine: EngineKind,
    #[arg(long)]
    engine_version: String,
    #[arg(long)]
    endpoint: String,
    #[arg(long, default_value_t = 32)]
    series: usize,
    #[arg(long, default_value_t = 20)]
    samples_per_series: usize,
    #[arg(long, default_value_t = 10_000)]
    step_ms: i64,
    #[arg(long, default_value_t = 16)]
    batch_series: usize,
    #[arg(long, default_value_t = 3)]
    query_iterations: usize,
    #[arg(long, default_value_t = 1)]
    trial: u32,
    #[arg(long, default_value = "baseline")]
    scenario: String,
    #[arg(long, default_value = "-")]
    output: String,
}

#[derive(Clone, PartialEq, Message)]
struct WriteRequest {
    #[prost(message, repeated, tag = "1")]
    timeseries: Vec<TimeSeries>,
}

#[derive(Clone, PartialEq, Message)]
struct TimeSeries {
    #[prost(message, repeated, tag = "1")]
    labels: Vec<Label>,
    #[prost(message, repeated, tag = "2")]
    samples: Vec<Sample>,
}

#[derive(Clone, PartialEq, Message)]
struct Label {
    #[prost(string, tag = "1")]
    name: String,
    #[prost(string, tag = "2")]
    value: String,
}

#[derive(Clone, PartialEq, Message)]
struct Sample {
    #[prost(double, tag = "1")]
    value: f64,
    #[prost(int64, tag = "2")]
    timestamp: i64,
}

#[derive(Debug, Serialize)]
struct Quantiles {
    count: u64,
    p50_us: f64,
    p95_us: f64,
    p99_us: f64,
    p999_us: f64,
    max_us: f64,
}

#[derive(Debug, Serialize)]
struct IngestMeasurement {
    samples: u64,
    requests: u64,
    payload_bytes: u64,
    elapsed_s: f64,
    request_elapsed_s: f64,
    encode_elapsed_s: f64,
    samples_per_s: f64,
    request_latency: Quantiles,
}

#[derive(Debug, Serialize)]
struct QueryMeasurement {
    iterations: usize,
    validated_rows: usize,
    latency: Quantiles,
}

#[derive(Debug, Serialize)]
struct QueryMeasurements {
    point: QueryMeasurement,
    range: QueryMeasurement,
    aggregate: QueryMeasurement,
}

#[derive(Debug, Serialize)]
struct Measurement {
    format_version: u32,
    lane: &'static str,
    engine: EngineKind,
    engine_version: String,
    transport: &'static str,
    query_language: &'static str,
    endpoint: String,
    series: usize,
    samples_per_series: usize,
    total_samples: u64,
    step_ms: i64,
    batch_series: usize,
    trial: u32,
    scenario: String,
    start_timestamp_ms: i64,
    end_timestamp_ms: i64,
    ingest: IngestMeasurement,
    queries: QueryMeasurements,
    client_process: ProcDelta,
    system: SystemDelta,
}

fn quantiles(hist: &Histogram<u64>) -> Quantiles {
    Quantiles {
        count: hist.len(),
        p50_us: hist.value_at_quantile(0.50) as f64 / 1_000.0,
        p95_us: hist.value_at_quantile(0.95) as f64 / 1_000.0,
        p99_us: hist.value_at_quantile(0.99) as f64 / 1_000.0,
        p999_us: hist.value_at_quantile(0.999) as f64 / 1_000.0,
        max_us: hist.max() as f64 / 1_000.0,
    }
}

fn record_duration(hist: &mut Histogram<u64>, elapsed: Duration) -> Result<()> {
    let ns = u64::try_from(elapsed.as_nanos())
        .unwrap_or(u64::MAX)
        .clamp(1, HIST_MAX_NS);
    hist.record(ns).context("record latency")
}

fn response_json(response: Response, context: &str) -> Result<Value> {
    let status = response.status();
    let body = response.text().context("read HTTP response body")?;
    if !status.is_success() {
        bail!("{context} returned HTTP {status}: {body}");
    }
    serde_json::from_str(&body).with_context(|| format!("parse {context} JSON: {body}"))
}

fn prometheus_seconds(millis: i64) -> String {
    format!(
        "{}.{:03}",
        millis.div_euclid(1_000),
        millis.rem_euclid(1_000)
    )
}

fn align_timestamp_to_step(timestamp_ms: i64, step_ms: i64) -> i64 {
    timestamp_ms.div_euclid(step_ms) * step_ms
}

fn deterministic_value(series: usize, sample: usize) -> f64 {
    sample as f64 + series as f64 / 1_000.0
}

fn host_label(series: usize) -> String {
    format!("h{series:06}")
}

fn region_label(series: usize) -> String {
    format!("r{:02}", series % 8)
}

fn remote_write_body(
    start_series: usize,
    end_series: usize,
    samples_per_series: usize,
    start_ms: i64,
    step_ms: i64,
) -> Result<Vec<u8>> {
    let mut timeseries = Vec::with_capacity(end_series - start_series);
    for series in start_series..end_series {
        let mut samples = Vec::with_capacity(samples_per_series);
        for sample in 0..samples_per_series {
            let sample_i64 = i64::try_from(sample).context("sample index overflow")?;
            samples.push(Sample {
                value: deterministic_value(series, sample),
                timestamp: start_ms + sample_i64 * step_ms,
            });
        }
        timeseries.push(TimeSeries {
            labels: vec![
                Label {
                    name: "__name__".to_string(),
                    value: METRIC_NAME.to_string(),
                },
                Label {
                    name: "host".to_string(),
                    value: host_label(series),
                },
                Label {
                    name: "region".to_string(),
                    value: region_label(series),
                },
            ],
            samples,
        });
    }
    let protobuf = WriteRequest { timeseries }.encode_to_vec();
    SnappyEncoder::new()
        .compress_vec(&protobuf)
        .context("snappy-compress remote-write request")
}

fn influx_body(
    start_series: usize,
    end_series: usize,
    samples_per_series: usize,
    start_ms: i64,
    step_ms: i64,
) -> Result<Vec<u8>> {
    let estimated_lines = (end_series - start_series).saturating_mul(samples_per_series);
    let mut body = String::with_capacity(estimated_lines.saturating_mul(72));
    for series in start_series..end_series {
        for sample in 0..samples_per_series {
            let sample_i64 = i64::try_from(sample).context("sample index overflow")?;
            let timestamp_ms = start_ms + sample_i64 * step_ms;
            let timestamp_ns = timestamp_ms
                .checked_mul(1_000_000)
                .context("timestamp nanosecond overflow")?;
            body.push_str(METRIC_NAME);
            body.push_str(",host=");
            body.push_str(&host_label(series));
            body.push_str(",region=");
            body.push_str(&region_label(series));
            body.push_str(" value=");
            body.push_str(&format!("{:.6}", deterministic_value(series, sample)));
            body.push(' ');
            body.push_str(&timestamp_ns.to_string());
            body.push('\n');
        }
    }
    Ok(body.into_bytes())
}

fn setup_engine(client: &Client, args: &Args) -> Result<()> {
    if !matches!(args.engine, EngineKind::Influxdb3) {
        return Ok(());
    }
    let url = format!(
        "{}/api/v3/configure/database",
        args.endpoint.trim_end_matches('/')
    );
    let response = client
        .post(url)
        .json(&serde_json::json!({"db": INFLUX_DATABASE}))
        .send()
        .context("create InfluxDB benchmark database")?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        bail!("create InfluxDB benchmark database returned HTTP {status}: {body}");
    }
    Ok(())
}

fn ingest(client: &Client, args: &Args, start_ms: i64) -> Result<IngestMeasurement> {
    let mut hist = Histogram::<u64>::new_with_max(HIST_MAX_NS, 3)?;
    let overall_started = Instant::now();
    let mut request_elapsed = Duration::ZERO;
    let mut encode_elapsed = Duration::ZERO;
    let mut payload_bytes = 0u64;
    let mut requests = 0u64;

    for start_series in (0..args.series).step_by(args.batch_series) {
        let end_series = (start_series + args.batch_series).min(args.series);
        let encode_started = Instant::now();
        let body = match args.engine {
            EngineKind::Influxdb3 => influx_body(
                start_series,
                end_series,
                args.samples_per_series,
                start_ms,
                args.step_ms,
            )?,
            EngineKind::Greptimedb | EngineKind::Victoriametrics | EngineKind::Prometheus => {
                remote_write_body(
                    start_series,
                    end_series,
                    args.samples_per_series,
                    start_ms,
                    args.step_ms,
                )?
            }
        };
        encode_elapsed += encode_started.elapsed();
        payload_bytes = payload_bytes.saturating_add(u64::try_from(body.len()).unwrap_or(u64::MAX));

        let request_started = Instant::now();
        let response = match args.engine {
            EngineKind::Greptimedb => client
                .post(format!(
                    "{}/v1/prometheus/write?db=public",
                    args.endpoint.trim_end_matches('/')
                ))
                .header("Content-Encoding", "snappy")
                .header("Content-Type", "application/x-protobuf")
                .header("X-Prometheus-Remote-Write-Version", "0.1.0")
                .body(body)
                .send(),
            EngineKind::Victoriametrics | EngineKind::Prometheus => client
                .post(format!(
                    "{}/api/v1/write",
                    args.endpoint.trim_end_matches('/')
                ))
                .header("Content-Encoding", "snappy")
                .header("Content-Type", "application/x-protobuf")
                .header("X-Prometheus-Remote-Write-Version", "0.1.0")
                .body(body)
                .send(),
            EngineKind::Influxdb3 => client
                .post(format!(
                    "{}/api/v3/write_lp?db={INFLUX_DATABASE}&precision=nanosecond&accept_partial=false",
                    args.endpoint.trim_end_matches('/')
                ))
                .header("Content-Type", "text/plain; charset=utf-8")
                .body(body)
                .send(),
        }
        .with_context(|| format!("send ingestion request for series {start_series}..{end_series}"))?;
        let elapsed = request_started.elapsed();
        request_elapsed += elapsed;
        record_duration(&mut hist, elapsed)?;
        requests += 1;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().unwrap_or_default();
            bail!("ingestion returned HTTP {status}: {body}");
        }
    }

    let elapsed = overall_started.elapsed();
    let total_samples = u64::try_from(args.series)?
        .checked_mul(u64::try_from(args.samples_per_series)?)
        .context("sample count overflow")?;
    Ok(IngestMeasurement {
        samples: total_samples,
        requests,
        payload_bytes,
        elapsed_s: elapsed.as_secs_f64(),
        request_elapsed_s: request_elapsed.as_secs_f64(),
        encode_elapsed_s: encode_elapsed.as_secs_f64(),
        samples_per_s: total_samples as f64 / elapsed.as_secs_f64(),
        request_latency: quantiles(&hist),
    })
}

fn prom_query(
    client: &Client,
    args: &Args,
    range: bool,
    query: &str,
    start_ms: i64,
    end_ms: i64,
) -> Result<Value> {
    let base = args.endpoint.trim_end_matches('/');
    let path = match (args.engine, range) {
        (EngineKind::Greptimedb, false) => "/v1/prometheus/api/v1/query",
        (EngineKind::Greptimedb, true) => "/v1/prometheus/api/v1/query_range",
        (_, false) => "/api/v1/query",
        (_, true) => "/api/v1/query_range",
    };
    let mut request = client.post(format!("{base}{path}"));
    if matches!(args.engine, EngineKind::Greptimedb) {
        request = request.query(&[("db", "public")]);
    }
    let end_s = prometheus_seconds(end_ms);
    let start_s = prometheus_seconds(start_ms);
    let step_s = prometheus_seconds(args.step_ms);
    let response = if range {
        request
            .form(&[
                ("query", query.to_string()),
                ("start", start_s),
                ("end", end_s),
                ("step", step_s),
            ])
            .send()
    } else {
        request
            .form(&[("query", query.to_string()), ("time", end_s)])
            .send()
    }
    .context("send PromQL query")?;
    response_json(response, "PromQL query")
}

fn influx_query(client: &Client, args: &Args, query: &str) -> Result<Value> {
    let response = client
        .post(format!(
            "{}/api/v3/query_sql",
            args.endpoint.trim_end_matches('/')
        ))
        .json(&serde_json::json!({"db": INFLUX_DATABASE, "q": query, "format": "json"}))
        .send()
        .context("send InfluxDB SQL query")?;
    response_json(response, "InfluxDB SQL query")
}

fn prom_result_len(value: &Value, range: bool) -> Result<(usize, usize)> {
    if value.get("status").and_then(Value::as_str) != Some("success") {
        bail!("PromQL response status is not success: {value}");
    }
    let result = value
        .pointer("/data/result")
        .and_then(Value::as_array)
        .context("PromQL response has no data.result array")?;
    let rows = if range {
        result
            .first()
            .and_then(|series| series.get("values"))
            .and_then(Value::as_array)
            .map_or(0, Vec::len)
    } else {
        result.len()
    };
    Ok((result.len(), rows))
}

fn influx_rows(value: &Value) -> Result<usize> {
    value
        .as_array()
        .map(Vec::len)
        .context("InfluxDB query response is not a JSON row array")
}

fn point_query(client: &Client, args: &Args, start_ms: i64, end_ms: i64) -> Result<usize> {
    match args.engine {
        EngineKind::Influxdb3 => influx_rows(&influx_query(
            client,
            args,
            "SELECT value FROM bench_metric WHERE host = 'h000000' ORDER BY time DESC LIMIT 1",
        )?),
        EngineKind::Greptimedb | EngineKind::Victoriametrics | EngineKind::Prometheus => {
            let (series, rows) = prom_result_len(
                &prom_query(
                    client,
                    args,
                    false,
                    "bench_metric{host=\"h000000\"}",
                    start_ms,
                    end_ms,
                )?,
                false,
            )?;
            if series != 1 || rows != 1 {
                bail!("point query expected exactly one series/row, got {series}/{rows}");
            }
            Ok(rows)
        }
    }
}

fn range_query(client: &Client, args: &Args, start_ms: i64, end_ms: i64) -> Result<usize> {
    match args.engine {
        EngineKind::Influxdb3 => influx_rows(&influx_query(
            client,
            args,
            "SELECT time, value FROM bench_metric WHERE host = 'h000000' ORDER BY time ASC",
        )?),
        EngineKind::Greptimedb | EngineKind::Victoriametrics | EngineKind::Prometheus => {
            let (series, rows) = prom_result_len(
                &prom_query(
                    client,
                    args,
                    true,
                    "bench_metric{host=\"h000000\"}",
                    start_ms,
                    end_ms,
                )?,
                true,
            )?;
            if series != 1 {
                bail!("range query expected one series, got {series}");
            }
            Ok(rows)
        }
    }
}

fn aggregate_query(client: &Client, args: &Args, start_ms: i64, end_ms: i64) -> Result<usize> {
    match args.engine {
        EngineKind::Influxdb3 => influx_rows(&influx_query(
            client,
            args,
            "SELECT SUM(value) AS total FROM bench_metric GROUP BY time ORDER BY time DESC LIMIT 1",
        )?),
        EngineKind::Greptimedb | EngineKind::Victoriametrics | EngineKind::Prometheus => {
            let (series, rows) = prom_result_len(
                &prom_query(client, args, false, "sum(bench_metric)", start_ms, end_ms)?,
                false,
            )?;
            if series != 1 || rows != 1 {
                bail!("aggregate query expected exactly one series/row, got {series}/{rows}");
            }
            Ok(rows)
        }
    }
}

fn wait_until_visible(client: &Client, args: &Args, start_ms: i64, end_ms: i64) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match point_query(client, args, start_ms, end_ms) {
            Ok(1) => return Ok(()),
            Ok(_) | Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100))
            }
            Ok(rows) => bail!("point query remained invalid after ingest: {rows} rows"),
            Err(error) => {
                return Err(error).context("data did not become query-visible within 15 s");
            }
        }
    }
}

fn measure_query<F>(iterations: usize, mut query: F) -> Result<QueryMeasurement>
where
    F: FnMut() -> Result<usize>,
{
    let mut hist = Histogram::<u64>::new_with_max(HIST_MAX_NS, 3)?;
    let mut validated_rows = 0usize;
    for _ in 0..iterations {
        let started = Instant::now();
        let rows = query()?;
        let elapsed = started.elapsed();
        if rows == 0 {
            bail!("query returned zero rows");
        }
        validated_rows = rows;
        record_duration(&mut hist, elapsed)?;
    }
    Ok(QueryMeasurement {
        iterations,
        validated_rows,
        latency: quantiles(&hist),
    })
}

fn output_measurement(output: &str, measurement: &Measurement) -> Result<()> {
    let json = serde_json::to_string_pretty(measurement)?;
    if output == "-" {
        println!("{json}");
    } else {
        fs::write(output, format!("{json}\n")).with_context(|| format!("write {output}"))?;
    }
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.series == 0 || args.samples_per_series == 0 || args.batch_series == 0 {
        bail!("series, samples-per-series and batch-series must all be nonzero");
    }
    if args.query_iterations == 0 || args.step_ms <= 0 {
        bail!("query-iterations must be nonzero and step-ms must be positive");
    }

    let client = Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(120))
        .pool_idle_timeout(Duration::from_secs(30))
        .build()
        .context("build HTTP client")?;
    setup_engine(&client, &args)?;

    let now_ms = i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())
        .context("current timestamp exceeds i64")?;
    let span_ms = i64::try_from(args.samples_per_series.saturating_sub(1))?
        .checked_mul(args.step_ms)
        .context("sample time span overflow")?;
    let aligned_now_ms = align_timestamp_to_step(now_ms, args.step_ms);
    let safety_lag_ms = args
        .step_ms
        .checked_mul(6)
        .context("timestamp safety-lag overflow")?;
    let end_ms = aligned_now_ms
        .checked_sub(safety_lag_ms)
        .context("timestamp underflow")?;
    let start_ms = end_ms.checked_sub(span_ms).context("timestamp underflow")?;

    let client_started = Instant::now();
    let proc_before = ProcSnapshot::capture();
    let system_before = SystemSnapshot::capture();
    let ingest = ingest(&client, &args, start_ms)?;
    wait_until_visible(&client, &args, start_ms, end_ms)?;

    let point = measure_query(args.query_iterations, || {
        point_query(&client, &args, start_ms, end_ms)
    })?;
    let range = measure_query(args.query_iterations, || {
        let rows = range_query(&client, &args, start_ms, end_ms)?;
        if rows != args.samples_per_series {
            bail!(
                "range query expected {} samples, got {rows}",
                args.samples_per_series
            );
        }
        Ok(rows)
    })?;
    let aggregate = measure_query(args.query_iterations, || {
        aggregate_query(&client, &args, start_ms, end_ms)
    })?;
    let proc_after = ProcSnapshot::capture();
    let system_after = SystemSnapshot::capture();
    let client_elapsed = client_started.elapsed();

    let total_samples = u64::try_from(args.series)?
        .checked_mul(u64::try_from(args.samples_per_series)?)
        .context("sample count overflow")?;
    let measurement = Measurement {
        format_version: 1,
        lane: "tsdb-server",
        engine: args.engine,
        engine_version: args.engine_version.clone(),
        transport: args.engine.transport(),
        query_language: args.engine.query_language(),
        endpoint: args.endpoint.clone(),
        series: args.series,
        samples_per_series: args.samples_per_series,
        total_samples,
        step_ms: args.step_ms,
        batch_series: args.batch_series,
        trial: args.trial,
        scenario: args.scenario.clone(),
        start_timestamp_ms: start_ms,
        end_timestamp_ms: end_ms,
        ingest,
        queries: QueryMeasurements {
            point,
            range,
            aggregate,
        },
        client_process: proc_before.delta(&proc_after, client_elapsed),
        system: system_before.delta(&system_after),
    };
    output_measurement(&args.output, &measurement)
}

#[cfg(test)]
mod tests {
    use super::{align_timestamp_to_step, prometheus_seconds};

    #[test]
    fn prometheus_timestamp_format_preserves_millisecond_phase() {
        assert_eq!(prometheus_seconds(1_791_237_928_632), "1791237928.632");
        assert_eq!(prometheus_seconds(10_000), "10.000");
    }

    #[test]
    fn generated_metric_grid_is_step_aligned() {
        assert_eq!(
            align_timestamp_to_step(1_791_237_928_632, 10_000),
            1_791_237_920_000
        );
        assert_eq!(align_timestamp_to_step(10_001, 10_000), 10_000);
    }
}
