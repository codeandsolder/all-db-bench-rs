use hdrhistogram::Histogram;
use serde::Serialize;
use std::{fs, path::Path, time::Duration};

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
