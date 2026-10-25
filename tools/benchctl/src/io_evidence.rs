use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use std::{collections::BTreeMap, fs, path::Path};

pub(crate) fn ratio(num: Option<f64>, den: Option<f64>) -> Option<f64> {
    match (num, den) {
        (Some(num), Some(den)) if den > 0.0 => Some(num / den),
        _ => None,
    }
}

pub(crate) fn composite_mixed_iops(
    read_iops: f64,
    write_iops: f64,
    read_fraction: f64,
) -> Result<f64> {
    if read_iops <= 0.0 || write_iops <= 0.0 {
        bail!("directional IOPS must be positive");
    }
    if !(0.0 < read_fraction && read_fraction < 1.0) {
        bail!("read_fraction must be between 0 and 1");
    }
    let write_fraction = 1.0 - read_fraction;
    Ok((read_iops / read_fraction).min(write_iops / write_fraction))
}

pub(crate) fn fio_p99_us(direction: &Value) -> Option<f64> {
    direction
        .get("clat_ns")?
        .get("percentile")?
        .get("99.000000")?
        .as_f64()
        .map(|value| value / 1000.0)
}

fn value_as_i64(value: Option<&Value>, default: i64) -> Result<i64> {
    let Some(value) = value else {
        return Ok(default);
    };
    match value {
        Value::Number(number) => number
            .as_i64()
            .context("integer JSON value is outside i64 range"),
        Value::String(raw) => raw
            .parse::<i64>()
            .with_context(|| format!("invalid integer string {raw:?}")),
        _ => bail!("expected integer-compatible JSON value, got {value}"),
    }
}

fn value_as_f64(value: Option<&Value>, default: f64) -> Result<f64> {
    let Some(value) = value else {
        return Ok(default);
    };
    match value {
        Value::Number(number) => number
            .as_f64()
            .context("number cannot be represented as f64"),
        Value::String(raw) => raw
            .parse::<f64>()
            .with_context(|| format!("invalid numeric string {raw:?}")),
        _ => bail!("expected numeric-compatible JSON value, got {value}"),
    }
}

pub(crate) struct FioExpectation<'a> {
    pub(crate) rw: Option<&'a str>,
    pub(crate) rwmixread: Option<i64>,
    pub(crate) bs_bytes: Option<i64>,
    pub(crate) blockalign_bytes: Option<i64>,
    pub(crate) ioengine: Option<&'a str>,
    pub(crate) direct: Option<i64>,
}

pub(crate) fn validate_fio_job(job: &Value, expected: FioExpectation<'_>) -> Result<()> {
    let error = value_as_i64(job.get("error"), 0)?;
    if error != 0 {
        bail!("fio error={error}");
    }
    let options = job
        .get("job options")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let mut expectations = BTreeMap::<&str, String>::new();
    if let Some(value) = expected.rw {
        expectations.insert("rw", value.to_owned());
    }
    if let Some(value) = expected.rwmixread {
        expectations.insert("rwmixread", value.to_string());
    }
    if let Some(value) = expected.bs_bytes {
        expectations.insert("bs", value.to_string());
    }
    if let Some(value) = expected.blockalign_bytes {
        expectations.insert("ba", value.to_string());
    }
    if let Some(value) = expected.ioengine {
        expectations.insert("ioengine", value.to_owned());
    }
    if let Some(value) = expected.direct {
        expectations.insert("direct", value.to_string());
    }

    let mut mismatched = BTreeMap::<String, Value>::new();
    for (key, expected_value) in expectations {
        let actual = options.get(key).cloned().unwrap_or(Value::Null);
        if actual.as_str() != Some(expected_value.as_str()) {
            mismatched.insert(key.to_owned(), json!([actual, expected_value]));
        }
    }
    if !mismatched.is_empty() {
        bail!("fio job options mismatch: {mismatched:?}");
    }
    Ok(())
}

fn object<'a>(value: &'a Value, key: &str) -> Option<&'a Map<String, Value>> {
    value.get(key).and_then(Value::as_object)
}

fn device_map(value: &Value) -> Result<BTreeMap<String, &Value>> {
    let mut devices = BTreeMap::new();
    let Some(rows) = value.get("devices").and_then(Value::as_array) else {
        return Ok(devices);
    };
    for row in rows {
        let name = row
            .get("name")
            .and_then(Value::as_str)
            .context("storage device is missing name")?;
        devices.insert(name.to_owned(), row);
    }
    Ok(devices)
}

fn stat_values(device: &Value) -> Result<Vec<i64>> {
    let values = device
        .get("stat")
        .and_then(Value::as_array)
        .context("storage device is missing stat array")?;
    values
        .iter()
        .map(|value| value_as_i64(Some(value), 0))
        .collect()
}

fn checked_delta(after: i64, before: i64, label: &str) -> Result<i64> {
    let delta = after - before;
    if delta < 0 {
        bail!("{label} moved backwards");
    }
    Ok(delta)
}

pub(crate) fn load_storage_delta(path: &Path) -> Result<Value> {
    let raw: Value = serde_json::from_str(
        &fs::read_to_string(path)
            .with_context(|| format!("failed to read storage sidecar {}", path.display()))?,
    )
    .with_context(|| format!("invalid JSON in storage sidecar {}", path.display()))?;
    let before = raw
        .get("before")
        .and_then(Value::as_object)
        .context("storage sidecar is missing before object")?;
    let after = raw
        .get("after")
        .and_then(Value::as_object)
        .context("storage sidecar is missing after object")?;

    if before.get("filesystem") != after.get("filesystem")
        || before.get("source") != after.get("source")
    {
        bail!("storage source changed during case");
    }

    let before_value = Value::Object(before.clone());
    let after_value = Value::Object(after.clone());
    let before_devices = device_map(&before_value)?;
    let after_devices = device_map(&after_value)?;
    let before_names = before_devices.keys().cloned().collect::<Vec<_>>();
    let after_names = after_devices.keys().cloned().collect::<Vec<_>>();
    if before_names != after_names {
        bail!("storage device set changed: {before_names:?} -> {after_names:?}");
    }

    let accounting_s = ((value_as_f64(after.get("captured_monotonic_ns"), 0.0)?
        - value_as_f64(before.get("captured_monotonic_ns"), 0.0)?)
        / 1e9)
        .max(0.0);

    let mut storage_read_ios = 0_i64;
    let mut storage_write_ios = 0_i64;
    let mut storage_read_bytes = 0_i64;
    let mut storage_write_bytes = 0_i64;
    let mut storage_discard_bytes = 0_i64;
    let mut storage_flushes = 0_i64;
    let mut storage_io_ms = 0_i64;
    let mut storage_weighted_io_ms = 0_i64;

    for name in &before_names {
        let before_stat = stat_values(
            before_devices
                .get(name)
                .copied()
                .with_context(|| format!("missing before device {name}"))?,
        )?;
        let after_stat = stat_values(
            after_devices
                .get(name)
                .copied()
                .with_context(|| format!("missing after device {name}"))?,
        )?;
        if before_stat.len() < 11 || after_stat.len() < 11 {
            bail!("short block stat for {name}");
        }
        let deltas = before_stat
            .iter()
            .zip(&after_stat)
            .map(|(before, after)| after - before)
            .collect::<Vec<_>>();
        let mut monotonic_indexes = vec![0_usize, 2, 4, 6, 9, 10];
        if deltas.len() >= 14 {
            monotonic_indexes.push(13);
        }
        if deltas.len() >= 16 {
            monotonic_indexes.push(15);
        }
        if monotonic_indexes
            .into_iter()
            .any(|index| deltas.get(index).is_some_and(|delta| *delta < 0))
        {
            bail!("block counters moved backwards for {name}");
        }

        storage_read_ios += deltas[0];
        storage_read_bytes += deltas[2] * 512;
        storage_write_ios += deltas[4];
        storage_write_bytes += deltas[6] * 512;
        storage_io_ms += deltas[9];
        storage_weighted_io_ms += deltas[10];
        if deltas.len() >= 14 {
            storage_discard_bytes += deltas[13] * 512;
        }
        if deltas.len() >= 16 {
            storage_flushes += deltas[15];
        }
    }

    let mut zfs_arc_hits = None;
    let mut zfs_arc_misses = None;
    let mut zfs_pool_arc_read_ios = None;
    let mut zfs_pool_arc_read_bytes = None;
    let mut zfs_pool_arc_write_ios = None;
    let mut zfs_pool_arc_write_bytes = None;
    let mut zfs_direct_read_ios = None;
    let mut zfs_direct_read_bytes = None;
    let mut zfs_direct_write_ios = None;
    let mut zfs_direct_write_bytes = None;

    if let (Some(before_pool), Some(after_pool)) = (
        object(&before_value, "zfs_pool_io"),
        object(&after_value, "zfs_pool_io"),
    ) {
        let fields = [
            ("arc_read_count", &mut zfs_pool_arc_read_ios),
            ("arc_read_bytes", &mut zfs_pool_arc_read_bytes),
            ("arc_write_count", &mut zfs_pool_arc_write_ios),
            ("arc_write_bytes", &mut zfs_pool_arc_write_bytes),
            ("direct_read_count", &mut zfs_direct_read_ios),
            ("direct_read_bytes", &mut zfs_direct_read_bytes),
            ("direct_write_count", &mut zfs_direct_write_ios),
            ("direct_write_bytes", &mut zfs_direct_write_bytes),
        ];
        for (source_name, slot) in fields {
            let before_counter = value_as_i64(before_pool.get(source_name), 0)?;
            let after_counter = value_as_i64(after_pool.get(source_name), 0)?;
            *slot = Some(checked_delta(
                after_counter,
                before_counter,
                &format!("ZFS pool counter {source_name}"),
            )?);
        }
    }

    if let (Some(before_arc), Some(after_arc)) = (
        object(&before_value, "zfs_arc"),
        object(&after_value, "zfs_arc"),
    ) {
        let hits =
            value_as_i64(after_arc.get("hits"), 0)? - value_as_i64(before_arc.get("hits"), 0)?;
        let misses =
            value_as_i64(after_arc.get("misses"), 0)? - value_as_i64(before_arc.get("misses"), 0)?;
        if hits >= 0 && misses >= 0 {
            zfs_arc_hits = Some(hits);
            zfs_arc_misses = Some(misses);
        }
    }

    let elapsed = accounting_s.max(1e-9);
    let arc_hit_fraction = ratio(
        zfs_arc_hits.map(|value| value as f64),
        match (zfs_arc_hits, zfs_arc_misses) {
            (Some(hits), Some(misses)) => Some((hits + misses) as f64),
            _ => None,
        },
    );

    Ok(json!({
        "storage_accounting_s": accounting_s,
        "storage_read_ios": storage_read_ios,
        "storage_write_ios": storage_write_ios,
        "storage_read_bytes": storage_read_bytes,
        "storage_write_bytes": storage_write_bytes,
        "storage_discard_bytes": storage_discard_bytes,
        "storage_flushes": storage_flushes,
        "storage_io_ms": storage_io_ms,
        "storage_weighted_io_ms": storage_weighted_io_ms,
        "storage_devices": before_names,
        "zfs_arc_hits": zfs_arc_hits,
        "zfs_arc_misses": zfs_arc_misses,
        "zfs_pool_arc_read_ios": zfs_pool_arc_read_ios,
        "zfs_pool_arc_read_bytes": zfs_pool_arc_read_bytes,
        "zfs_pool_arc_write_ios": zfs_pool_arc_write_ios,
        "zfs_pool_arc_write_bytes": zfs_pool_arc_write_bytes,
        "zfs_direct_read_ios": zfs_direct_read_ios,
        "zfs_direct_read_bytes": zfs_direct_read_bytes,
        "zfs_direct_write_ios": zfs_direct_write_ios,
        "zfs_direct_write_bytes": zfs_direct_write_bytes,
        "storage_read_mbps": storage_read_bytes as f64 / elapsed / 1_000_000.0,
        "storage_write_mbps": storage_write_bytes as f64 / elapsed / 1_000_000.0,
        "zfs_arc_hit_fraction": arc_hit_fraction,
    }))
}

#[cfg(test)]
mod tests {
    use super::{composite_mixed_iops, load_storage_delta};
    use anyhow::Result;
    use serde_json::json;
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);

    fn sidecar(before_stat: Vec<i64>, after_stat: Vec<i64>) -> Result<PathBuf> {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "all-db-bench-io-evidence-{}-{id}.json",
            std::process::id()
        ));
        let payload = json!({
            "before": {
                "filesystem": "zfs",
                "source": "pool/data",
                "captured_monotonic_ns": 1_000_000_000_i64,
                "devices": [{"name":"disk0","stat":before_stat}],
                "zfs_arc": {"hits":10,"misses":5},
                "zfs_pool_io": {
                    "direct_read_count":10,"direct_read_bytes":1000,
                    "direct_write_count":20,"direct_write_bytes":2000,
                    "arc_read_count":30,"arc_read_bytes":3000,
                    "arc_write_count":40,"arc_write_bytes":4000
                }
            },
            "after": {
                "filesystem": "zfs",
                "source": "pool/data",
                "captured_monotonic_ns": 2_000_000_000_i64,
                "devices": [{"name":"disk0","stat":after_stat}],
                "zfs_arc": {"hits":14,"misses":6},
                "zfs_pool_io": {
                    "direct_read_count":12,"direct_read_bytes":1512,
                    "direct_write_count":23,"direct_write_bytes":3024,
                    "arc_read_count":31,"arc_read_bytes":3512,
                    "arc_write_count":42,"arc_write_bytes":5024
                }
            }
        });
        fs::write(&path, serde_json::to_vec(&payload)?)?;
        Ok(path)
    }

    #[test]
    fn in_flight_gauge_may_decrease() -> Result<()> {
        let path = sidecar(
            vec![
                100, 0, 200, 0, 300, 0, 400, 0, 7, 500, 600, 0, 0, 700, 0, 10, 0,
            ],
            vec![
                101, 0, 210, 0, 302, 0, 420, 0, 2, 503, 605, 0, 0, 700, 0, 11, 0,
            ],
        )?;
        let delta = load_storage_delta(&path)?;
        fs::remove_file(&path)?;
        assert_eq!(delta["storage_read_ios"], 1);
        assert_eq!(delta["storage_read_bytes"], 10 * 512);
        assert_eq!(delta["storage_write_ios"], 2);
        assert_eq!(delta["storage_write_bytes"], 20 * 512);
        assert_eq!(delta["storage_flushes"], 1);
        assert_eq!(delta["zfs_arc_hits"], 4);
        assert_eq!(delta["zfs_arc_misses"], 1);
        assert_eq!(delta["zfs_direct_read_ios"], 2);
        assert_eq!(delta["zfs_direct_read_bytes"], 512);
        assert_eq!(delta["zfs_direct_write_ios"], 3);
        assert_eq!(delta["zfs_direct_write_bytes"], 1024);
        Ok(())
    }

    #[test]
    fn consumed_monotonic_counter_may_not_decrease() -> Result<()> {
        let path = sidecar(
            vec![100, 0, 200, 0, 300, 0, 400, 0, 1, 500, 600],
            vec![99, 0, 210, 0, 302, 0, 420, 0, 0, 503, 605],
        )?;
        let result = load_storage_delta(&path);
        fs::remove_file(&path)?;
        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn composite_capacity_matches_directional_limits() -> Result<()> {
        assert!((composite_mixed_iops(700.0, 600.0, 0.7)? - 1000.0).abs() < 1e-9);
        assert!((composite_mixed_iops(1400.0, 150.0, 0.7)? - 500.0).abs() < 1e-9);
        assert!(composite_mixed_iops(0.0, 1.0, 0.7).is_err());
        assert!(composite_mixed_iops(1.0, 1.0, 1.0).is_err());
        Ok(())
    }
}
