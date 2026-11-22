use all_db_bench_common::recovery::{PresenceCounts, RecoveryVerification};
use all_db_bench_common::{
    measurement::{MeasurementInterval, SetupTimings},
    single::{SingleEvidence, SingleOptions},
};
use anyhow::{Result, bail};
use clap::Parser;
use hdrhistogram::Histogram;
use rand::{RngExt, SeedableRng, rngs::SmallRng, seq::SliceRandom};
use serde::Serialize;
use std::{path::Path, time::Instant};

use super::{
    data::{key, sample_existing_id, sample_read_id, value},
    engine::Engine,
    engine_policy::compression_mapping,
    lifecycle::{DatasetSpec, prefill, warm_existing_reads},
    metrics::{
        ProcDelta, Quantiles, SettleMeasurement, SystemDelta, hist, measure_settle, quantiles,
        record,
    },
    types::{
        AccessPattern, Durability, EngineCompression, EngineKind, KeyShape, ValuePattern, Workload,
        WritePattern,
    },
    workload::mixed_epoch_targets,
};

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
    #[command(flatten)]
    single: SingleOptions,
    #[arg(long, default_value_t = 0)]
    settle_ms: u64,
    #[arg(long, default_value_t = 250)]
    settle_sample_ms: u64,
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
    open_process: ProcDelta,
    open_system_delta: SystemDelta,
    reads: u64,
    writes: u64,
    deletes: u64,
    read_latency: Quantiles,
    write_txn_latency: Quantiles,
    post_workload_settle: Option<SettleMeasurement>,

    #[serde(flatten)]
    evidence: SingleEvidence,
}

#[expect(
    clippy::too_many_lines,
    reason = "the benchmark workload state machine is clearer as one exhaustive workload match"
)]
async fn run_workload(
    engine: &mut Engine,
    args: &Args,
    read_hist: &mut Histogram<u64>,
    write_hist: &mut Histogram<u64>,
) -> Result<(u64, u64, u64, u64)> {
    let mut rng = SmallRng::seed_from_u64(args.seed ^ u64::from(args.trial));
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
            if !args.engine.supports_range_scan() {
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
                if n != usize::try_from(args.scan_len)? {
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
                let n = usize::try_from(args.ops - done)?.min(batch);
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
                args.single.checkpoint(done)?;
            }
        }
        Workload::DeleteBurst => {
            if args.ops > args.records {
                bail!("delete-burst requires ops <= records to avoid repeated tombstones");
            }
            while done < args.ops {
                let n = usize::try_from(args.ops - done)?.min(txn_size);
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
                let targets = mixed_epoch_targets(args.workload, epoch, false);
                debug_assert_eq!(targets.total(), epoch);

                let mut units =
                    Vec::with_capacity(usize::try_from(targets.reads)?.saturating_add(16));
                for _ in 0..targets.reads {
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

                let mut write_specs = Vec::with_capacity(usize::try_from(
                    targets.updates + targets.inserts + targets.deletes,
                )?);
                for _ in 0..targets.updates {
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
                for _ in 0..targets.inserts {
                    let id = next_id;
                    next_id += 1;
                    write_specs.push(WriteSpec::Put(
                        make_key(id),
                        make_value(id, args.seed ^ done),
                    ));
                }
                for _ in 0..targets.deletes {
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

async fn verify_recovery(
    engine: &mut Engine,
    args: &Args,
    expected_prefix_records: u64,
    tail_records: u64,
) -> Result<RecoveryVerification> {
    let started = Instant::now();
    let mut prefix = PresenceCounts::default();
    for id in 0..expected_prefix_records {
        prefix.observe(
            engine
                .get(&key(id, args.key_bytes, args.key_shape, args.seed))
                .await?
                .is_some(),
        );
    }
    let mut tail = PresenceCounts::default();
    for id in expected_prefix_records..expected_prefix_records.saturating_add(tail_records) {
        tail.observe(
            engine
                .get(&key(id, args.key_bytes, args.key_shape, args.seed))
                .await?
                .is_some(),
        );
    }
    Ok(RecoveryVerification::from_observations(
        expected_prefix_records,
        tail_records,
        &prefix,
        &tail,
        args.txn_size,
        started.elapsed().as_secs_f64(),
    ))
}

struct Prepared {
    engine: Engine,
    open: all_db_bench_common::measurement::MeasuredInterval,
    setup: SetupTimings,
    verification: Option<RecoveryVerification>,
}

async fn prepare(args: &Args, path: &Path) -> Result<Prepared> {
    let open = MeasurementInterval::start();
    let mut engine = if args.engine_compression == EngineCompression::Default {
        Engine::open(args.engine, args.durability, path).await?
    } else {
        Engine::open_with_compression(args.engine, args.durability, path, args.engine_compression)
            .await?
    };
    let open = open.finish();
    let dataset = DatasetSpec {
        records: args.records,
        key_bytes: args.key_bytes,
        key_shape: args.key_shape,
        seed: args.seed,
        value_bytes: args.value_bytes,
        value_pattern: args.value_pattern,
        durability: args.durability,
    };
    let prefill_s = if args.single.skip_prefill {
        0.0
    } else {
        let prefill_started = Instant::now();
        prefill(&mut engine, dataset, 0).await?;
        prefill_started.elapsed().as_secs_f64()
    };
    let verification = if let Some(expected_prefix_records) = args.single.verify_prefix_records {
        Some(
            verify_recovery(
                &mut engine,
                args,
                expected_prefix_records,
                args.single.verify_tail_records,
            )
            .await?,
        )
    } else {
        None
    };

    let warmup_started = Instant::now();
    warm_existing_reads(
        &mut engine,
        dataset,
        args.single.warmup_reads,
        args.access_pattern,
        args.workload,
    )
    .await?;
    let warmup_s = warmup_started.elapsed().as_secs_f64();

    let setup = SetupTimings {
        open: open.elapsed.as_secs_f64(),
        prefill: prefill_s,
        warmup: warmup_s,
    };
    Ok(Prepared {
        engine,
        open,
        setup,
        verification,
    })
}

/// Run one single-client benchmark invocation and emit its measurement record.
///
/// # Errors
/// Returns an error for invalid arguments, backend failures, verification failures, or result I/O failures.
pub async fn run() -> Result<()> {
    let args = Args::parse();
    if args.records == 0 || args.ops == 0 || args.txn_size == 0 {
        bail!("--records, --ops and --txn-size must be greater than zero");
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
    if args.single.progress_file.is_some()
        && !matches!(args.workload, Workload::TinyTxn | Workload::WriteBurst)
    {
        bail!("--progress-file requires tiny-txn or write-burst");
    }
    if (args.single.progress_file.is_some() || args.single.verify_prefix_records.is_some())
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
    let path = args.single.prepare(&run_name)?;

    let Prepared {
        mut engine,
        open,
        setup,
        verification,
    } = prepare(&args, &path).await?;

    let mut read_hist = hist()?;
    let mut write_hist = hist()?;
    let foreground = MeasurementInterval::start();
    let (completed, reads, writes, deletes) =
        run_workload(&mut engine, &args, &mut read_hist, &mut write_hist).await?;
    let work = foreground.finish().fixed_work(args.ops, completed)?;
    let post_workload_settle = measure_settle(
        &path,
        args.settle_ms,
        args.settle_sample_ms,
        tokio::time::sleep,
    )
    .await;
    engine.close().await?;
    drop(engine);

    let result = Measurement {
        format_version: 8,
        engine: args.engine,
        engine_version: args.engine.version(),
        durability: args.durability,
        durability_mapping: mapping,
        workload: args.workload,
        records: args.records,
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
        open_process: open.process,
        open_system_delta: open.system_delta,
        reads,
        writes,
        deletes,
        read_latency: quantiles(&read_hist),
        write_txn_latency: quantiles(&write_hist),
        post_workload_settle,
        evidence: args.single.evidence(&path, setup, work, verification),
    };

    args.single.finish(&path, &result)
}
