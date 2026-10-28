use anyhow::{Context, Result, bail};
use clap::Parser;
use hdrhistogram::Histogram;
use rand::{RngExt, SeedableRng, rngs::SmallRng, seq::SliceRandom};
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use super::{
    data::{key, sample_existing_id, sample_read_id, value},
    engine::{Engine, compression_mapping, compression_override_supported},
    metrics::{self as bench_metrics, Quantiles, dir_size, hist, peak_rss_kib, quantiles, record},
    types::{
        AccessPattern, Durability, EngineCompression, EngineKind, KeyShape, ValuePattern, Workload,
        WritePattern,
    },
};
use bench_metrics::metrics::{ProcDelta, ProcSnapshot, SystemDelta, SystemSnapshot};

#[derive(Debug, Parser)]
#[command(about = "One fresh database per invocation; output is one JSON record")]
struct Args {
    #[arg(long, value_enum)]
    engine: EngineKind,
    #[arg(long, value_enum)]
    durability: Durability,
    #[arg(long, value_enum)]
    workload: Workload,
    #[arg(long, default_value_t = 100_000)]
    records: u64,
    #[arg(long, default_value_t = 50_000)]
    ops: u64,
    #[arg(long, default_value_t = 256)]
    value_bytes: usize,
    #[arg(long, value_enum, default_value_t = ValuePattern::PseudoRandom)]
    value_pattern: ValuePattern,
    #[arg(long, value_enum, default_value_t = EngineCompression::Default)]
    engine_compression: EngineCompression,
    #[arg(long, default_value_t = 8)]
    key_bytes: usize,
    #[arg(long, value_enum, default_value_t = KeyShape::Sequential)]
    key_shape: KeyShape,
    #[arg(long, value_enum, default_value_t = AccessPattern::Auto)]
    access_pattern: AccessPattern,
    #[arg(long, default_value_t = 0)]
    miss_percent: u8,
    #[arg(long, value_enum, default_value_t = WritePattern::Append)]
    write_pattern: WritePattern,
    #[arg(long, default_value_t = 100)]
    txn_size: usize,
    #[arg(long, default_value_t = 100)]
    scan_len: u64,
    #[arg(long, default_value_t = 1)]
    trial: u32,
    #[arg(long, default_value_t = 0x5eed_2026)]
    seed: u64,
    #[arg(long, default_value = "baseline")]
    scenario: String,
    #[arg(long, default_value = "data")]
    root: PathBuf,
    #[arg(long, default_value = "-")]
    output: String,
    #[arg(long, default_value_t = false)]
    keep_db: bool,
    #[arg(long, default_value_t = false)]
    reuse_db: bool,
    #[arg(long, default_value_t = false)]
    skip_prefill: bool,
    #[arg(long, default_value_t = 5_000)]
    warmup_reads: u64,
    #[arg(long)]
    db_name: Option<String>,
    #[arg(long)]
    progress_file: Option<PathBuf>,
    #[arg(long)]
    verify_prefix_records: Option<u64>,
    #[arg(long, default_value_t = 0)]
    verify_tail_records: u64,
    #[arg(long, default_value_t = 0)]
    settle_ms: u64,
    #[arg(long, default_value_t = 250)]
    settle_sample_ms: u64,
}

#[derive(Debug, Serialize)]
struct Verification {
    expected_prefix_records: u64,
    checked_prefix_records: u64,
    missing_prefix_records: u64,
    prefix_contiguous_present: u64,
    prefix_present_after_gap: u64,
    tail_checked_records: u64,
    tail_prefix_present: u64,
    tail_total_present: u64,
    tail_present_after_gap: u64,
    transaction_atomic_tail: bool,
    verification_ok: bool,
    verify_s: f64,
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
    engine: EngineKind,
    engine_version: &'static str,
    durability: Durability,
    durability_mapping: &'static str,
    workload: Workload,
    records: u64,
    ops_requested: u64,
    ops_completed: u64,
    value_bytes: usize,
    value_pattern: ValuePattern,
    engine_compression: EngineCompression,
    engine_compression_mapping: &'static str,
    key_bytes: usize,
    key_shape: KeyShape,
    access_pattern: AccessPattern,
    miss_percent: u8,
    write_pattern: WritePattern,
    txn_size: usize,
    scan_len: u64,
    trial: u32,
    seed: u64,
    scenario: String,
    open_s: f64,
    open_process: ProcDelta,
    open_system_delta: SystemDelta,
    prefill_s: f64,
    warmup_s: f64,
    elapsed_s: f64,
    ops_per_s: f64,
    reads: u64,
    writes: u64,
    deletes: u64,
    read_latency: Quantiles,
    write_txn_latency: Quantiles,
    db_bytes: u64,
    post_workload_settle: Option<SettleMeasurement>,
    peak_rss_kib: u64,
    measured_process: ProcDelta,
    system_before: SystemSnapshot,
    system_after: SystemSnapshot,
    measured_system_delta: SystemDelta,
    path: String,
    db_kept: bool,
    reused_db: bool,
    prefill_skipped: bool,
    warmup_reads: u64,
    verification: Option<Verification>,
}

impl Args {
    fn range_scan_supported(&self) -> bool {
        !matches!(self.engine, EngineKind::Lkv | EngineKind::ParitydbHash)
    }
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
    let mut rng = SmallRng::seed_from_u64(args.seed ^ 0x7777);
    for _ in 0..args.records.min(args.warmup_reads) {
        let id = sample_existing_id(&mut rng, args.records, args.access_pattern, args.workload);
        let _ = engine
            .get(&key(id, args.key_bytes, args.key_shape, args.seed))
            .await?;
    }
    Ok(())
}

async fn run_workload(
    engine: &mut Engine,
    args: &Args,
    read_hist: &mut Histogram<u64>,
    write_hist: &mut Histogram<u64>,
) -> Result<(u64, u64, u64, u64)> {
    let mut rng = SmallRng::seed_from_u64(args.seed ^ args.trial as u64);
    let mut reads = 0u64;
    let mut writes = 0u64;
    let mut deletes = 0u64;
    let mut done = 0u64;
    let txn_size = args.txn_size.max(1);
    let mut next_id = args.records;

    let make_key = |id| key(id, args.key_bytes, args.key_shape, args.seed);
    let make_value = |id, salt| value(id, args.value_bytes, salt, args.value_pattern);

    match args.workload {
        Workload::PointRead => {
            for _ in 0..args.ops {
                let (id, expect_hit) = sample_read_id(
                    &mut rng,
                    args.records,
                    args.ops,
                    args.miss_percent,
                    args.access_pattern,
                    args.workload,
                );
                let t = Instant::now();
                let got = engine.get(&make_key(id)).await?;
                record(read_hist, t.elapsed());
                if got.is_some() != expect_hit {
                    bail!("point-read hit/miss mismatch for id {id}: expected_hit={expect_hit}");
                }
                reads += 1;
                done += 1;
            }
        }
        Workload::RangeScan => {
            if !args.range_scan_supported() {
                bail!("selected engine/configuration has no ordered keyed range-scan API");
            }
            if args.key_shape == KeyShape::Hashed {
                bail!("hashed keys intentionally have no ID-ordered range semantics");
            }
            if args.scan_len == 0 || args.scan_len > args.records {
                bail!("scan_len must be in 1..=records");
            }
            let last_start = args.records - args.scan_len;
            for _ in 0..args.ops {
                let start_id = if last_start == 0 {
                    0
                } else {
                    rng.random_range(0..=last_start)
                };
                let end_id = start_id + args.scan_len;
                let start_key = make_key(start_id);
                let end_key = make_key(end_id);
                let t = Instant::now();
                let n = engine.scan_count(&start_key, &end_key).await?;
                record(read_hist, t.elapsed());
                if n != args.scan_len as usize {
                    bail!("range scan returned {n} rows, expected {}", args.scan_len);
                }
                reads += args.scan_len;
                done += 1;
            }
        }
        Workload::TinyTxn | Workload::WriteBurst => {
            let batch = if matches!(args.workload, Workload::TinyTxn) {
                1
            } else {
                txn_size
            };
            while done < args.ops {
                let n = (args.ops - done).min(batch as u64) as usize;
                let mut puts = Vec::with_capacity(n);
                for _ in 0..n {
                    let id = match args.write_pattern {
                        WritePattern::Append => {
                            let id = next_id;
                            next_id += 1;
                            id
                        }
                        WritePattern::UpdateUniform => rng.random_range(0..args.records),
                        WritePattern::UpdateHot => sample_existing_id(
                            &mut rng,
                            args.records,
                            match args.access_pattern {
                                AccessPattern::Auto | AccessPattern::Uniform => {
                                    AccessPattern::Hot80
                                }
                                other => other,
                            },
                            args.workload,
                        ),
                    };
                    puts.push((make_key(id), make_value(id, args.seed ^ 0x1111 ^ done)));
                }
                let t = Instant::now();
                engine.write_batch(args.durability, &puts, &[]).await?;
                record(write_hist, t.elapsed());
                writes += n as u64;
                done += n as u64;
                if let Some(progress_file) = &args.progress_file {
                    fs::write(progress_file, format!("{done}\n"))
                        .context("write recovery progress marker")?;
                }
            }
        }
        Workload::DeleteBurst => {
            if args.ops > args.records {
                bail!("delete-burst requires ops <= records to avoid repeated tombstones");
            }
            while done < args.ops {
                let n = (args.ops - done).min(txn_size as u64) as usize;
                let mut dels = Vec::with_capacity(n);
                for offset in 0..n {
                    let id = done + offset as u64;
                    dels.push(make_key(id));
                }
                let t = Instant::now();
                engine.write_batch(args.durability, &[], &dels).await?;
                record(write_hist, t.elapsed());
                deletes += n as u64;
                done += n as u64;
            }
        }
        Workload::ReadHeavy | Workload::Balanced | Workload::Churn => {
            enum WriteSpec {
                Put(Vec<u8>, Vec<u8>),
                Delete(Vec<u8>),
            }
            enum Unit {
                Read(Vec<u8>),
                Write {
                    puts: Vec<(Vec<u8>, Vec<u8>)>,
                    deletes: Vec<Vec<u8>>,
                },
            }

            while done < args.ops {
                let epoch = (args.ops - done).min(1_000);
                let (r_target, u_target, i_target, d_target) = match args.workload {
                    Workload::ReadHeavy => {
                        let u = epoch * 5 / 100;
                        (epoch - u, u, 0, 0)
                    }
                    Workload::Balanced => {
                        let u = epoch * 30 / 100;
                        let i = epoch * 10 / 100;
                        let d = epoch * 10 / 100;
                        (epoch - u - i - d, u, i, d)
                    }
                    Workload::Churn => {
                        let u = epoch * 40 / 100;
                        let i = epoch * 30 / 100;
                        (0, u, i, epoch - u - i)
                    }
                    _ => unreachable!(),
                };

                let mut units = Vec::with_capacity((r_target as usize) + 16);
                for _ in 0..r_target {
                    let (id, _expect_hit) = sample_read_id(
                        &mut rng,
                        args.records,
                        args.ops,
                        args.miss_percent,
                        args.access_pattern,
                        args.workload,
                    );
                    units.push(Unit::Read(make_key(id)));
                }

                let mut write_specs = Vec::with_capacity((u_target + i_target + d_target) as usize);
                for _ in 0..u_target {
                    let id = sample_existing_id(
                        &mut rng,
                        args.records,
                        args.access_pattern,
                        args.workload,
                    );
                    write_specs.push(WriteSpec::Put(
                        make_key(id),
                        make_value(id, args.seed ^ done),
                    ));
                }
                for _ in 0..i_target {
                    let id = next_id;
                    next_id += 1;
                    write_specs.push(WriteSpec::Put(
                        make_key(id),
                        make_value(id, args.seed ^ done),
                    ));
                }
                for _ in 0..d_target {
                    let id = sample_existing_id(
                        &mut rng,
                        args.records,
                        args.access_pattern,
                        args.workload,
                    );
                    write_specs.push(WriteSpec::Delete(make_key(id)));
                }
                write_specs.shuffle(&mut rng);

                let mut puts = Vec::with_capacity(txn_size);
                let mut dels = Vec::with_capacity(txn_size);
                let mut batch_ops = 0usize;
                for spec in write_specs {
                    match spec {
                        WriteSpec::Put(k, v) => puts.push((k, v)),
                        WriteSpec::Delete(k) => dels.push(k),
                    }
                    batch_ops += 1;
                    if batch_ops == txn_size {
                        units.push(Unit::Write {
                            puts: std::mem::take(&mut puts),
                            deletes: std::mem::take(&mut dels),
                        });
                        batch_ops = 0;
                    }
                }
                if batch_ops != 0 {
                    units.push(Unit::Write {
                        puts,
                        deletes: dels,
                    });
                }
                units.shuffle(&mut rng);

                for unit in units {
                    match unit {
                        Unit::Read(k) => {
                            let t = Instant::now();
                            let _ = engine.get(&k).await?;
                            record(read_hist, t.elapsed());
                            reads += 1;
                            done += 1;
                        }
                        Unit::Write {
                            puts,
                            deletes: dels,
                        } => {
                            let n = puts.len() + dels.len();
                            let t = Instant::now();
                            engine.write_batch(args.durability, &puts, &dels).await?;
                            record(write_hist, t.elapsed());
                            writes += puts.len() as u64;
                            deletes += dels.len() as u64;
                            done += n as u64;
                        }
                    }
                }
            }
        }
    }
    Ok((done, reads, writes, deletes))
}

async fn measure_settle(path: &Path, args: &Args) -> Option<SettleMeasurement> {
    if args.settle_ms == 0 {
        return None;
    }
    let sample_ms = args.settle_sample_ms.max(1);
    let total_started = Instant::now();
    let total_proc_before = ProcSnapshot::capture();
    let total_sys_before = SystemSnapshot::capture();
    let db_bytes_before = dir_size(path);
    let mut samples = Vec::new();
    let mut prev_proc = total_proc_before.clone();
    let mut prev_sys = total_sys_before.clone();
    let mut prev_elapsed = Duration::ZERO;

    loop {
        let elapsed = total_started.elapsed();
        if elapsed.as_millis() as u64 >= args.settle_ms {
            break;
        }
        let remaining_ms = args
            .settle_ms
            .saturating_sub(elapsed.as_millis() as u64)
            .max(1);
        let sleep_ms = remaining_ms.min(sample_ms);
        tokio::time::sleep(Duration::from_millis(sleep_ms)).await;

        let now_elapsed = total_started.elapsed();
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

    let total_elapsed = total_started.elapsed();
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

async fn verify_recovery(
    engine: &mut Engine,
    args: &Args,
    expected_prefix_records: u64,
    tail_records: u64,
) -> Result<Verification> {
    let started = Instant::now();
    let mut missing_prefix_records = 0u64;
    let mut prefix_contiguous_present = 0u64;
    let mut prefix_present_after_gap = 0u64;
    let mut gap_seen = false;
    for id in 0..expected_prefix_records {
        let present = engine
            .get(&key(id, args.key_bytes, args.key_shape, args.seed))
            .await?
            .is_some();
        if present {
            if gap_seen {
                prefix_present_after_gap += 1;
            } else {
                prefix_contiguous_present += 1;
            }
        } else {
            missing_prefix_records += 1;
            gap_seen = true;
        }
    }

    let mut tail_prefix_present = 0u64;
    let mut tail_total_present = 0u64;
    let mut tail_present_after_gap = 0u64;
    gap_seen = false;
    for id in expected_prefix_records..expected_prefix_records.saturating_add(tail_records) {
        let present = engine
            .get(&key(id, args.key_bytes, args.key_shape, args.seed))
            .await?
            .is_some();
        if present {
            tail_total_present += 1;
            if gap_seen {
                tail_present_after_gap += 1;
            } else {
                tail_prefix_present += 1;
            }
        } else {
            gap_seen = true;
        }
    }

    let txn = args.txn_size.max(1) as u64;
    let transaction_atomic_tail = tail_prefix_present == 0 || tail_prefix_present % txn == 0;
    let verification_ok = missing_prefix_records == 0
        && prefix_present_after_gap == 0
        && tail_present_after_gap == 0
        && transaction_atomic_tail;
    Ok(Verification {
        expected_prefix_records,
        checked_prefix_records: expected_prefix_records,
        missing_prefix_records,
        prefix_contiguous_present,
        prefix_present_after_gap,
        tail_checked_records: tail_records,
        tail_prefix_present,
        tail_total_present,
        tail_present_after_gap,
        transaction_atomic_tail,
        verification_ok,
        verify_s: started.elapsed().as_secs_f64(),
    })
}

pub async fn run() -> Result<()> {
    let args = Args::parse();
    if args.records == 0 {
        bail!("--records must be greater than zero");
    }
    if args.key_bytes < 8 {
        bail!("--key-bytes must be at least 8");
    }
    if args.miss_percent > 100 {
        bail!("--miss-percent must be in 0..=100");
    }
    if args.key_shape == KeyShape::Hashed && matches!(args.workload, Workload::RangeScan) {
        bail!("--key-shape hashed is incompatible with ID-ordered range-scan");
    }
    if (args.progress_file.is_some() || args.verify_prefix_records.is_some())
        && args.write_pattern != WritePattern::Append
    {
        bail!("recovery progress/verification requires --write-pattern append");
    }

    let mapping = args.engine.durability_mapping(args.durability)?;
    let compression_mapping = compression_mapping(args.engine, args.engine_compression)?;
    let run_name = format!(
        "{:?}-{:?}-{:?}-c{:?}-n{}-k{}-{:?}-v{}-{:?}-tx{}-scan{}-trial{}",
        args.engine,
        args.durability,
        args.workload,
        args.engine_compression,
        args.records,
        args.key_bytes,
        args.key_shape,
        args.value_bytes,
        args.value_pattern,
        args.txn_size,
        args.scan_len,
        args.trial
    )
    .to_lowercase()
    .replace('_', "-");
    let path = if let Some(name) = &args.db_name {
        let candidate = Path::new(name);
        if candidate.components().count() != 1 || name == "." || name == ".." {
            bail!("--db-name must be one plain path component");
        }
        args.root.join(candidate)
    } else {
        args.root.join(run_name)
    };
    if path.exists() && !args.reuse_db {
        fs::remove_dir_all(&path).context("remove stale run directory")?;
    }
    if args.reuse_db && !path.exists() {
        bail!(
            "--reuse-db requested but database path does not exist: {}",
            path.display()
        );
    }
    fs::create_dir_all(&path)?;

    let open_system_before = SystemSnapshot::capture();
    let open_process_before = ProcSnapshot::capture();
    let open_started = Instant::now();
    let mut engine = if args.engine_compression == EngineCompression::Default {
        Engine::open(args.engine, args.durability, &path).await?
    } else {
        Engine::open_with_compression(args.engine, args.durability, &path, args.engine_compression)
            .await?
    };
    let open_elapsed = open_started.elapsed();
    let open_process_after = ProcSnapshot::capture();
    let open_system_after = SystemSnapshot::capture();
    let open_s = open_elapsed.as_secs_f64();
    let open_process = open_process_before.delta(&open_process_after, open_elapsed);
    let open_system_delta = open_system_before.delta(&open_system_after);
    let prefill_s = if args.skip_prefill {
        0.0
    } else {
        let prefill_started = Instant::now();
        prefill(&mut engine, &args).await?;
        prefill_started.elapsed().as_secs_f64()
    };
    let verification = if let Some(expected_prefix_records) = args.verify_prefix_records {
        Some(
            verify_recovery(
                &mut engine,
                &args,
                expected_prefix_records,
                args.verify_tail_records,
            )
            .await?,
        )
    } else {
        None
    };

    let warmup_started = Instant::now();
    warm_reads(&mut engine, &args).await?;
    let warmup_s = warmup_started.elapsed().as_secs_f64();

    let mut read_hist = hist();
    let mut write_hist = hist();
    let system_before = SystemSnapshot::capture();
    let process_before = ProcSnapshot::capture();
    let started = Instant::now();
    let (completed, reads, writes, deletes) =
        run_workload(&mut engine, &args, &mut read_hist, &mut write_hist).await?;
    let elapsed = started.elapsed();
    let process_after = ProcSnapshot::capture();
    let system_after = SystemSnapshot::capture();
    let measured_process = process_before.delta(&process_after, elapsed);
    let measured_system_delta = system_before.delta(&system_after);
    let post_workload_settle = measure_settle(&path, &args).await;
    engine.close().await?;
    drop(engine);
    let db_bytes = dir_size(&path);

    let result = Measurement {
        format_version: 8,
        engine: args.engine,
        engine_version: args.engine.version(),
        durability: args.durability,
        durability_mapping: mapping,
        workload: args.workload,
        records: args.records,
        ops_requested: args.ops,
        ops_completed: completed,
        value_bytes: args.value_bytes,
        value_pattern: args.value_pattern,
        engine_compression: args.engine_compression,
        engine_compression_mapping: compression_mapping,
        key_bytes: args.key_bytes,
        key_shape: args.key_shape,
        access_pattern: args.access_pattern,
        miss_percent: args.miss_percent,
        write_pattern: args.write_pattern,
        txn_size: args.txn_size,
        scan_len: args.scan_len,
        trial: args.trial,
        seed: args.seed,
        scenario: args.scenario.clone(),
        open_s,
        open_process,
        open_system_delta,
        prefill_s,
        warmup_s,
        elapsed_s: elapsed.as_secs_f64(),
        ops_per_s: completed as f64 / elapsed.as_secs_f64(),
        reads,
        writes,
        deletes,
        read_latency: quantiles(&read_hist),
        write_txn_latency: quantiles(&write_hist),
        db_bytes,
        post_workload_settle,
        peak_rss_kib: peak_rss_kib(),
        measured_process,
        system_before,
        system_after,
        measured_system_delta,
        path: path.display().to_string(),
        db_kept: args.keep_db,
        reused_db: args.reuse_db,
        prefill_skipped: args.skip_prefill,
        warmup_reads: args.warmup_reads,
        verification,
    };

    let line = serde_json::to_string(&result)?;
    if args.output == "-" {
        println!("{line}");
    } else {
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&args.output)?;
        writeln!(f, "{line}")?;
    }
    if !args.keep_db {
        fs::remove_dir_all(&path).context("remove completed run database")?;
    }
    Ok(())
}

#[cfg(test)]
mod compression_value_tests {
    use super::*;

    #[test]
    fn value_patterns_are_sized_and_deterministic() {
        let patterns = [
            ValuePattern::PseudoRandom,
            ValuePattern::Zeros,
            ValuePattern::Repeated,
            ValuePattern::StructuredBinary,
            ValuePattern::StructuredText,
        ];
        for pattern in patterns {
            let a = value(42, 4096, 7, pattern);
            let b = value(42, 4096, 7, pattern);
            assert_eq!(a.len(), 4096);
            assert_eq!(a, b);
        }
        assert!(
            value(42, 257, 7, ValuePattern::Zeros)
                .iter()
                .all(|&x| x == 0)
        );
        assert_ne!(
            value(42, 4096, 7, ValuePattern::PseudoRandom),
            value(42, 4096, 7, ValuePattern::StructuredBinary)
        );
        assert!(
            String::from_utf8(value(42, 4096, 7, ValuePattern::StructuredText))
                .expect("structured text pattern must be UTF-8")
                .contains("dbbench")
        );
    }

    #[test]
    fn explicit_compression_support_is_intentional() {
        assert!(compression_override_supported(EngineKind::Fjall));
        assert!(compression_override_supported(EngineKind::Surrealkv));
        assert!(compression_override_supported(EngineKind::Turbokv));
        assert!(!compression_override_supported(EngineKind::Redb));
    }
}
