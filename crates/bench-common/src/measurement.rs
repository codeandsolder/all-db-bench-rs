use crate::metrics::{ProcDelta, ProcSnapshot, SystemDelta, SystemSnapshot};
use hdrhistogram::Histogram;
use serde::Serialize;
use std::{
    fs,
    future::Future,
    path::Path,
    time::{Duration, Instant},
};

const HIST_MAX_NS: u64 = 60_000_000_000;

#[derive(Debug, Serialize)]
pub struct Quantiles {
    count: u64,
    p50_us: f64,
    p95_us: f64,
    p99_us: f64,
    p999_us: f64,
    max_us: f64,
}

#[must_use]
pub fn histogram() -> Histogram<u64> {
    Histogram::new_with_bounds(1, HIST_MAX_NS, 3)
        .expect("fixed benchmark histogram bounds are valid")
}

pub fn record_duration(histogram: &mut Histogram<u64>, duration: Duration) {
    let ns = duration.as_nanos().min(HIST_MAX_NS as u128) as u64;
    let _ = histogram.record(ns.max(1));
}

#[must_use]
pub fn quantiles(histogram: &Histogram<u64>) -> Quantiles {
    let us = |value: u64| value as f64 / 1000.0;
    if histogram.is_empty() {
        return Quantiles {
            count: 0,
            p50_us: 0.0,
            p95_us: 0.0,
            p99_us: 0.0,
            p999_us: 0.0,
            max_us: 0.0,
        };
    }
    Quantiles {
        count: histogram.len(),
        p50_us: us(histogram.value_at_quantile(0.50)),
        p95_us: us(histogram.value_at_quantile(0.95)),
        p99_us: us(histogram.value_at_quantile(0.99)),
        p999_us: us(histogram.value_at_quantile(0.999)),
        max_us: us(histogram.max()),
    }
}

#[must_use]
pub fn peak_rss_kib() -> u64 {
    let Ok(status) = fs::read_to_string("/proc/self/status") else {
        return 0;
    };
    status
        .lines()
        .find_map(|line| {
            let rest = line.strip_prefix("VmHWM:")?;
            rest.split_whitespace().next()?.parse().ok()
        })
        .unwrap_or(0)
}

#[must_use]
pub fn dir_size(path: &Path) -> u64 {
    fn walk(path: &Path, sum: &mut u64) {
        let Ok(metadata) = fs::symlink_metadata(path) else {
            return;
        };
        if metadata.is_file() {
            *sum = sum.saturating_add(metadata.len());
            return;
        }
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            walk(&entry.path(), sum);
        }
    }

    let mut bytes = 0;
    walk(path, &mut bytes);
    bytes
}

#[derive(Debug, Serialize)]
pub struct SettleSample {
    elapsed_ms: u64,
    interval_ms: u64,
    db_bytes: u64,
    process: ProcDelta,
    system: SystemDelta,
}

#[derive(Debug, Serialize)]
pub struct SettleMeasurement {
    requested_ms: u64,
    sample_ms: u64,
    elapsed_s: f64,
    db_bytes_before: u64,
    db_bytes_after: u64,
    process: ProcDelta,
    system: SystemDelta,
    samples: Vec<SettleSample>,
}

pub async fn measure_settle<S, F>(
    path: &Path,
    requested_ms: u64,
    sample_ms: u64,
    mut sleep: S,
) -> Option<SettleMeasurement>
where
    S: FnMut(Duration) -> F,
    F: Future<Output = ()>,
{
    if requested_ms == 0 {
        return None;
    }

    let sample_ms = sample_ms.max(1);
    let started = Instant::now();
    let total_proc_before = ProcSnapshot::capture();
    let total_sys_before = SystemSnapshot::capture();
    let db_bytes_before = dir_size(path);
    let mut prev_proc = total_proc_before.clone();
    let mut prev_sys = total_sys_before.clone();
    let mut prev_elapsed = Duration::ZERO;
    let mut samples = Vec::new();

    while (started.elapsed().as_millis() as u64) < requested_ms {
        let elapsed = started.elapsed();
        let remaining_ms = requested_ms
            .saturating_sub(elapsed.as_millis() as u64)
            .max(1);
        sleep(Duration::from_millis(remaining_ms.min(sample_ms))).await;

        let now_elapsed = started.elapsed();
        let now_proc = ProcSnapshot::capture();
        let now_sys = SystemSnapshot::capture();
        let interval = now_elapsed.saturating_sub(prev_elapsed);
        samples.push(SettleSample {
            elapsed_ms: now_elapsed.as_millis() as u64,
            interval_ms: interval.as_millis() as u64,
            db_bytes: dir_size(path),
            process: prev_proc.delta(&now_proc, interval),
            system: prev_sys.delta(&now_sys),
        });
        prev_proc = now_proc;
        prev_sys = now_sys;
        prev_elapsed = now_elapsed;
    }

    let total_elapsed = started.elapsed();
    let total_proc_after = ProcSnapshot::capture();
    let total_sys_after = SystemSnapshot::capture();
    Some(SettleMeasurement {
        requested_ms,
        sample_ms,
        elapsed_s: total_elapsed.as_secs_f64(),
        db_bytes_before,
        db_bytes_after: dir_size(path),
        process: total_proc_before.delta(&total_proc_after, total_elapsed),
        system: total_sys_before.delta(&total_sys_after),
        samples,
    })
}
