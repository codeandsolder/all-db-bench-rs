#![allow(clippy::too_many_lines)]

#[allow(dead_code)]
#[path = "kvbench.rs"]
mod base;

use anyhow::{Context, Result, bail};
use base::metrics::{ProcDelta, ProcSnapshot, SystemDelta, SystemSnapshot};
use base::{
    AccessPattern, Durability, Engine, EngineKind, KeyShape, Quantiles, ValuePattern, Workload,
    dir_size, hist, key, peak_rss_kib, quantiles, record, sample_existing_id, value,
};
use clap::{Parser, ValueEnum};
use rand::{RngExt, SeedableRng, rngs::SmallRng, seq::SliceRandom};
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
enum SustainedPattern {
    Append,
    UpdateUniform,
    UpdateHot95,
    Churn,
}

#[derive(Debug, Parser)]
#[command(about = "Fixed-work windowed sustained-write/compaction benchmark")]
struct Args {
    #[arg(long, value_enum)]
    engine: EngineKind,
    #[arg(long, value_enum)]
    durability: Durability,
    #[arg(long, value_enum)]
    pattern: SustainedPattern,
    #[arg(long, default_value_t = 100_000)]
    records: u64,
    #[arg(long, default_value_t = 125_000)]
    ops: u64,
    #[arg(long, default_value_t = 5_000)]
    window_ops: u64,
    #[arg(long, default_value_t = 4096)]
    value_bytes: usize,
    #[arg(long, value_enum, default_value_t = ValuePattern::PseudoRandom)]
    value_pattern: ValuePattern,
    #[arg(long, default_value_t = 8)]
    key_bytes: usize,
    #[arg(long, value_enum, default_value_t = KeyShape::Sequential)]
    key_shape: KeyShape,
    #[arg(long, default_value_t = 100)]
    txn_size: usize,
    #[arg(long, default_value_t = 1)]
    trial: u32,
    #[arg(long, default_value_t = 0x5eed_2026)]
    seed: u64,
    #[arg(long, default_value_t = 5_000)]
    warmup_reads: u64,
    #[arg(long, default_value_t = 5_000)]
    settle_ms: u64,
    #[arg(long, default_value_t = 250)]
    settle_sample_ms: u64,
    #[arg(long, default_value = "sustained")]
    scenario: String,
    #[arg(long, default_value = "data")]
    root: PathBuf,
    #[arg(long, default_value = "-")]
    output: String,
    #[arg(long, default_value_t = false)]
    keep_db: bool,
}

#[derive(Debug, Serialize)]
struct WindowMeasurement {
    index: u64,
    ops_start: u64,
    ops_end: u64,
    ops_completed: u64,
    elapsed_s: f64,
    ops_per_s: f64,
    puts: u64,
    deletes: u64,
    transactions: u64,
    logical_mutated_bytes: u64,
    transaction_latency: Quantiles,
    process: ProcDelta,
    system: SystemDelta,
}

#[derive(Debug, Serialize)]
struct SettleSample {
    elapsed_ms: u64,
    interval_ms: u64,
    db_bytes: u64,
    process: ProcDelta,
    system: SystemDelta,
}

#[derive(Debug, Serialize)]
struct SettleMeasurement {
    requested_ms: u64,
    sample_ms: u64,
    elapsed_s: f64,
    db_bytes_before: u64,
    db_bytes_after: u64,
    process: ProcDelta,
    system: SystemDelta,
    samples: Vec<SettleSample>,
}

#[derive(Debug, Serialize)]
struct Measurement {
    format_version: u32,
    lane: String,
    engine: EngineKind,
    engine_version: String,
    durability: Durability,
    durability_mapping: String,
    pattern: SustainedPattern,
    records: u64,
    ops_requested: u64,
    ops_completed: u64,
    window_ops: u64,
    value_bytes: usize,
    value_pattern: ValuePattern,
    key_bytes: usize,
    key_shape: KeyShape,
    txn_size: usize,
    trial: u32,
    seed: u64,
    scenario: String,
    open_s: f64,
    prefill_s: f64,
    warmup_s: f64,
    elapsed_s: f64,
    active_elapsed_s: f64,
    instrumentation_s: f64,
    instrumentation_fraction_of_wall: f64,
    ops_per_s: f64,
    active_ops_per_s: f64,
    puts: u64,
    deletes: u64,
    transactions: u64,
    logical_mutated_bytes: u64,
    transaction_latency: Quantiles,
    db_bytes_before: u64,
    db_bytes_after_foreground: u64,
    db_bytes_final: u64,
    windows: Vec<WindowMeasurement>,
    post_workload_settle: Option<SettleMeasurement>,
    measured_process: ProcDelta,
    system_before: SystemSnapshot,
    system_after: SystemSnapshot,
    measured_system_delta: SystemDelta,
    peak_rss_kib: u64,
    path: String,
    db_kept: bool,
}

async fn prefill(engine: &mut Engine, args: &Args) -> Result<()> {
    const BATCH: usize = 10_000;
    let mut puts = Vec::with_capacity(BATCH);
    for id in 0..args.records {
        puts.push((
            key(id, args.key_bytes, args.key_shape, args.seed),
            value(id, args.value_bytes, args.seed, args.value_pattern),
        ));
        if puts.len() == BATCH {
            engine.write_batch(args.durability, &puts, &[]).await?;
            puts.clear();
        }
    }
    if !puts.is_empty() {
        engine.write_batch(args.durability, &puts, &[]).await?;
    }
    Ok(())
}

async fn warm_reads(engine: &mut Engine, args: &Args) -> Result<()> {
    let mut rng = SmallRng::seed_from_u64(args.seed ^ args.trial as u64 ^ 0x7777);
    for _ in 0..args.records.min(args.warmup_reads) {
        let id = rng.random_range(0..args.records);
        let _ = engine
            .get(&key(id, args.key_bytes, args.key_shape, args.seed))
            .await?;
    }
    Ok(())
}

async fn measure_settle(path: &Path, args: &Args) -> Option<SettleMeasurement> {
    if args.settle_ms == 0 {
        return None;
    }
    let sample_ms = args.settle_sample_ms.max(1);
    let started = Instant::now();
    let total_proc_before = ProcSnapshot::capture();
    let total_sys_before = SystemSnapshot::capture();
    let db_bytes_before = dir_size(path);
    let mut prev_proc = total_proc_before.clone();
    let mut prev_sys = total_sys_before.clone();
    let mut prev_elapsed = Duration::ZERO;
    let mut samples = Vec::new();

    while (started.elapsed().as_millis() as u64) < args.settle_ms {
        let elapsed = started.elapsed();
        let remaining = args
            .settle_ms
            .saturating_sub(elapsed.as_millis() as u64)
            .max(1);
        tokio::time::sleep(Duration::from_millis(remaining.min(sample_ms))).await;
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
        requested_ms: args.settle_ms,
        sample_ms,
        elapsed_s: total_elapsed.as_secs_f64(),
        db_bytes_before,
        db_bytes_after: dir_size(path),
        process: total_proc_before.delta(&total_proc_after, total_elapsed),
        system: total_sys_before.delta(&total_sys_after),
        samples,
    })
}

#[tokio::main(flavor = "multi_thread", worker_threads = 1)]
async fn main() -> Result<()> {
    let args = Args::parse();
    if args.records == 0 || args.ops == 0 || args.window_ops == 0 || args.txn_size == 0 {
        bail!("records, ops, window-ops and txn-size must all be greater than zero");
    }
    if args.key_bytes < 8 {
        bail!("--key-bytes must be at least 8");
    }
    if args.window_ops < args.txn_size as u64 || args.window_ops % args.txn_size as u64 != 0 {
        bail!(
            "--window-ops must be a positive multiple of --txn-size so window boundaries do not manufacture partial transactions"
        );
    }
    if args.ops % args.txn_size as u64 != 0 {
        bail!(
            "--ops must be a multiple of --txn-size so the final transaction is not silently shortened"
        );
    }
    if matches!(args.engine, EngineKind::ManifoldWal) {
        bail!(
            "manifold-wal is a known-broken recovery diagnostic and is excluded from sustained rankings"
        );
    }

    let mapping = args.engine.durability_mapping(args.durability)?;
    let path = args.root.join(
        format!(
            "sustained-{:?}-{:?}-{:?}-n{}-o{}-w{}-v{}-tx{}-t{}",
            args.engine,
            args.durability,
            args.pattern,
            args.records,
            args.ops,
            args.window_ops,
            args.value_bytes,
            args.txn_size,
            args.trial
        )
        .to_lowercase()
        .replace('_', "-"),
    );
    if path.exists() {
        fs::remove_dir_all(&path).context("remove stale sustained-run directory")?;
    }
    fs::create_dir_all(&path)?;

    let open_started = Instant::now();
    let mut engine = Engine::open(args.engine, args.durability, &path).await?;
    let open_s = open_started.elapsed().as_secs_f64();
    let prefill_started = Instant::now();
    prefill(&mut engine, &args).await?;
    let prefill_s = prefill_started.elapsed().as_secs_f64();
    let warmup_started = Instant::now();
    warm_reads(&mut engine, &args).await?;
    let warmup_s = warmup_started.elapsed().as_secs_f64();

    let db_bytes_before = dir_size(&path);
    let process_before = ProcSnapshot::capture();
    let system_before = SystemSnapshot::capture();
    let foreground_started = Instant::now();
    let mut rng = SmallRng::seed_from_u64(args.seed ^ args.trial as u64 ^ 0x5155_5354);
    let mut next_id = args.records;
    let mut done = 0u64;
    let mut puts_total = 0u64;
    let mut deletes_total = 0u64;
    let mut txn_total = 0u64;
    let mut logical_bytes_total = 0u64;
    let mut total_hist = hist();
    let mut windows = Vec::new();
    let mut active_elapsed = Duration::ZERO;

    while done < args.ops {
        let window_start_ops = done;
        let window_target = args.ops.min(done.saturating_add(args.window_ops));
        let window_proc_before = ProcSnapshot::capture();
        let window_sys_before = SystemSnapshot::capture();
        let window_started = Instant::now();
        let mut window_hist = hist();
        let mut window_puts = 0u64;
        let mut window_deletes = 0u64;
        let mut window_txns = 0u64;
        let mut window_logical_bytes = 0u64;

        if matches!(args.pattern, SustainedPattern::Churn) {
            #[derive(Clone, Copy)]
            enum ChurnOp {
                Update,
                Insert,
                Delete,
            }

            while done < window_target {
                let epoch = (window_target - done).min(1_000);
                let update_count = epoch * 40 / 100;
                let insert_count = epoch * 30 / 100;
                let delete_count = epoch - update_count - insert_count;
                let mut specs = Vec::with_capacity(epoch as usize);
                specs.extend(std::iter::repeat_n(ChurnOp::Update, update_count as usize));
                specs.extend(std::iter::repeat_n(ChurnOp::Insert, insert_count as usize));
                specs.extend(std::iter::repeat_n(ChurnOp::Delete, delete_count as usize));
                specs.shuffle(&mut rng);

                for chunk in specs.chunks(args.txn_size) {
                    let mut puts = Vec::with_capacity(chunk.len());
                    let mut deletes = Vec::with_capacity(chunk.len());
                    for (op_offset, spec) in chunk.iter().enumerate() {
                        let salt = args.seed ^ done ^ op_offset as u64;
                        match spec {
                            ChurnOp::Update => {
                                let id = rng.random_range(0..args.records);
                                puts.push((
                                    key(id, args.key_bytes, args.key_shape, args.seed),
                                    value(id, args.value_bytes, salt, args.value_pattern),
                                ));
                            }
                            ChurnOp::Insert => {
                                let id = next_id;
                                next_id += 1;
                                puts.push((
                                    key(id, args.key_bytes, args.key_shape, args.seed),
                                    value(id, args.value_bytes, salt, args.value_pattern),
                                ));
                            }
                            ChurnOp::Delete => {
                                let id = rng.random_range(0..args.records);
                                deletes.push(key(id, args.key_bytes, args.key_shape, args.seed));
                            }
                        }
                    }
                    let put_count = puts.len() as u64;
                    let delete_count = deletes.len() as u64;
                    let logical_bytes = put_count
                        .saturating_mul((args.key_bytes + args.value_bytes) as u64)
                        + delete_count.saturating_mul(args.key_bytes as u64);
                    let t = Instant::now();
                    engine.write_batch(args.durability, &puts, &deletes).await?;
                    let latency = t.elapsed();
                    record(&mut window_hist, latency);
                    record(&mut total_hist, latency);
                    done += chunk.len() as u64;
                    window_puts += put_count;
                    window_deletes += delete_count;
                    window_txns += 1;
                    window_logical_bytes = window_logical_bytes.saturating_add(logical_bytes);
                }
            }
        } else {
            while done < window_target {
                let n = (window_target - done).min(args.txn_size as u64) as usize;
                let mut puts = Vec::with_capacity(n);
                for op_offset in 0..n {
                    let salt = args.seed ^ done ^ op_offset as u64;
                    let id = match args.pattern {
                        SustainedPattern::Append => {
                            let id = next_id;
                            next_id += 1;
                            id
                        }
                        SustainedPattern::UpdateUniform => rng.random_range(0..args.records),
                        SustainedPattern::UpdateHot95 => sample_existing_id(
                            &mut rng,
                            args.records,
                            AccessPattern::Hot95,
                            Workload::WriteBurst,
                        ),
                        SustainedPattern::Churn => unreachable!(),
                    };
                    puts.push((
                        key(id, args.key_bytes, args.key_shape, args.seed),
                        value(id, args.value_bytes, salt, args.value_pattern),
                    ));
                }

                let put_count = puts.len() as u64;
                let logical_bytes =
                    put_count.saturating_mul((args.key_bytes + args.value_bytes) as u64);
                let t = Instant::now();
                engine.write_batch(args.durability, &puts, &[]).await?;
                let latency = t.elapsed();
                record(&mut window_hist, latency);
                record(&mut total_hist, latency);
                done += n as u64;
                window_puts += put_count;
                window_txns += 1;
                window_logical_bytes = window_logical_bytes.saturating_add(logical_bytes);
            }
        }

        let elapsed = window_started.elapsed();
        active_elapsed += elapsed;
        let window_proc_after = ProcSnapshot::capture();
        let window_sys_after = SystemSnapshot::capture();
        let window_completed = done - window_start_ops;
        windows.push(WindowMeasurement {
            index: windows.len() as u64,
            ops_start: window_start_ops,
            ops_end: done,
            ops_completed: window_completed,
            elapsed_s: elapsed.as_secs_f64(),
            ops_per_s: window_completed as f64 / elapsed.as_secs_f64().max(f64::MIN_POSITIVE),
            puts: window_puts,
            deletes: window_deletes,
            transactions: window_txns,
            logical_mutated_bytes: window_logical_bytes,
            transaction_latency: quantiles(&window_hist),
            process: window_proc_before.delta(&window_proc_after, elapsed),
            system: window_sys_before.delta(&window_sys_after),
        });
        puts_total += window_puts;
        deletes_total += window_deletes;
        txn_total += window_txns;
        logical_bytes_total = logical_bytes_total.saturating_add(window_logical_bytes);
    }

    let foreground_wall_elapsed = foreground_started.elapsed();
    let process_after = ProcSnapshot::capture();
    let system_after = SystemSnapshot::capture();
    let db_bytes_after_foreground = dir_size(&path);
    let post_workload_settle = measure_settle(&path, &args).await;
    engine.close().await?;
    drop(engine);
    let db_bytes_final = dir_size(&path);

    let result = Measurement {
        format_version: 6,
        lane: String::from("kv-sustained"),
        engine: args.engine,
        engine_version: args.engine.version().to_string(),
        durability: args.durability,
        durability_mapping: mapping.to_string(),
        pattern: args.pattern,
        records: args.records,
        ops_requested: args.ops,
        ops_completed: done,
        window_ops: args.window_ops,
        value_bytes: args.value_bytes,
        value_pattern: args.value_pattern,
        key_bytes: args.key_bytes,
        key_shape: args.key_shape,
        txn_size: args.txn_size,
        trial: args.trial,
        seed: args.seed,
        scenario: args.scenario.clone(),
        open_s,
        prefill_s,
        warmup_s,
        elapsed_s: foreground_wall_elapsed.as_secs_f64(),
        active_elapsed_s: active_elapsed.as_secs_f64(),
        instrumentation_s: foreground_wall_elapsed
            .saturating_sub(active_elapsed)
            .as_secs_f64(),
        instrumentation_fraction_of_wall: foreground_wall_elapsed
            .saturating_sub(active_elapsed)
            .as_secs_f64()
            / foreground_wall_elapsed.as_secs_f64().max(f64::MIN_POSITIVE),
        ops_per_s: done as f64 / foreground_wall_elapsed.as_secs_f64().max(f64::MIN_POSITIVE),
        active_ops_per_s: done as f64 / active_elapsed.as_secs_f64().max(f64::MIN_POSITIVE),
        puts: puts_total,
        deletes: deletes_total,
        transactions: txn_total,
        logical_mutated_bytes: logical_bytes_total,
        transaction_latency: quantiles(&total_hist),
        db_bytes_before,
        db_bytes_after_foreground,
        db_bytes_final,
        windows,
        post_workload_settle,
        measured_process: process_before.delta(&process_after, foreground_wall_elapsed),
        system_before: system_before.clone(),
        system_after: system_after.clone(),
        measured_system_delta: system_before.delta(&system_after),
        peak_rss_kib: peak_rss_kib(),
        path: path.display().to_string(),
        db_kept: args.keep_db,
    };

    let json = serde_json::to_string_pretty(&result)?;
    if args.output == "-" {
        println!("{json}");
    } else {
        let output = Path::new(&args.output);
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(output, format!("{json}\n"))?;
    }

    if !args.keep_db {
        fs::remove_dir_all(&path).context("remove sustained-run database")?;
    }
    Ok(())
}
