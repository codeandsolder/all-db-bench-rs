use all_db_bench_common::recovery::{PresenceCounts, RecoveryVerification};
use all_db_bench_common::{
    measurement::{MeasurementInterval, SetupTimings},
    single::{SingleEvidence, SingleOptions},
};
use anyhow::{Result, bail};
use clap::Parser;
use serde::Serialize;
use std::{path::Path, time::Instant};

use super::{
    backend_ops::BackendOps,
    data::key,
    engine::Engine,
    engine_policy::compression_mapping,
    fixed_work::{FixedWorkArgs, WorkExecutor, WorkMode, WorkStats},
    lifecycle::{prefill, warm_existing_reads},
    metrics::{ProcDelta, Quantiles, SettleMeasurement, SystemDelta, measure_settle, quantiles},
    types::{
        AccessPattern, Durability, EngineCompression, EngineKind, KeyShape, ValuePattern, Workload,
        WritePattern,
    },
};

#[derive(Debug, Parser)]
#[command(about = "One fresh database per invocation; output is one JSON record")]
struct Args {
    #[command(flatten)]
    work: FixedWorkArgs,
    #[arg(long, value_enum, default_value_t = EngineCompression::Default)]
    engine_compression: EngineCompression,
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
                .read(&key(
                    id,
                    args.work.key_bytes,
                    args.work.key_shape,
                    args.work.seed,
                ))
                .await?
                .is_some(),
        );
    }
    let mut tail = PresenceCounts::default();
    for id in expected_prefix_records..expected_prefix_records.saturating_add(tail_records) {
        tail.observe(
            engine
                .read(&key(
                    id,
                    args.work.key_bytes,
                    args.work.key_shape,
                    args.work.seed,
                ))
                .await?
                .is_some(),
        );
    }
    Ok(RecoveryVerification::from_observations(
        expected_prefix_records,
        tail_records,
        &prefix,
        &tail,
        args.work.txn_size,
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
        Engine::open(args.work.engine, args.work.durability, path).await?
    } else {
        Engine::open_with_compression(
            args.work.engine,
            args.work.durability,
            path,
            args.engine_compression,
        )
        .await?
    };
    let open = open.finish();
    let dataset = args.work.spec().dataset;
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
        args.work.access_pattern,
        args.work.workload,
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

fn validate_args(args: &Args, spec: super::fixed_work::WorkSpec) -> Result<()> {
    spec.validate(args.work.engine, 0)?;
    if args.single.progress_file.is_some()
        && !matches!(args.work.workload, Workload::TinyTxn | Workload::WriteBurst)
    {
        bail!("--progress-file requires tiny-txn or write-burst");
    }
    if (args.single.progress_file.is_some() || args.single.verify_prefix_records.is_some())
        && args.work.write_pattern != WritePattern::Append
    {
        bail!("recovery progress/verification requires --write-pattern append");
    }

    Ok(())
}

/// Run one single-client benchmark invocation and emit its measurement record.
///
/// # Errors
/// Returns an error for invalid arguments, backend failures, verification failures, or result I/O failures.
pub async fn run() -> Result<()> {
    let args = Args::parse();
    let spec = args.work.spec();
    validate_args(&args, spec)?;

    let mapping = args.work.engine.durability_mapping(args.work.durability)?;
    let compression_mapping = compression_mapping(args.work.engine, args.engine_compression)?;
    let run_name = format!(
        "{:?}-{:?}-{:?}-c{:?}-n{}-k{}-{:?}-v{}-{:?}-tx{}-scan{}-trial{}",
        args.work.engine,
        args.work.durability,
        args.work.workload,
        args.engine_compression,
        args.work.records,
        args.work.key_bytes,
        args.work.key_shape,
        args.work.value_bytes,
        args.work.value_pattern,
        args.work.txn_size,
        args.work.scan_len,
        args.work.trial
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

    let stats = WorkStats::new()?;
    let foreground = MeasurementInterval::start();
    let mut executor = WorkExecutor::new(
        spec,
        all_db_bench_common::concurrency::WorkPartition {
            offset: 0,
            count: args.work.ops,
        },
        WorkMode::Single(&args.single),
        args.work.seed ^ u64::from(args.work.trial),
        None,
        stats,
    )?;
    executor.run(&mut engine).await?;
    let stats = executor.stats;
    let work = foreground
        .finish()
        .fixed_work(args.work.ops, stats.completed)?;
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
        engine: args.work.engine,
        engine_version: args.work.engine.version(),
        durability: args.work.durability,
        durability_mapping: mapping,
        workload: args.work.workload,
        records: args.work.records,
        value_bytes: args.work.value_bytes,
        value_pattern: args.work.value_pattern,
        engine_compression: args.engine_compression,
        engine_compression_mapping: compression_mapping,
        key_bytes: args.work.key_bytes,
        key_shape: args.work.key_shape,
        access_pattern: args.work.access_pattern,
        miss_percent: args.work.miss_percent,
        write_pattern: args.work.write_pattern,
        txn_size: args.work.txn_size,
        scan_len: args.work.scan_len,
        trial: args.work.trial,
        seed: args.work.seed,
        scenario: args.scenario.clone(),
        open_process: open.process,
        open_system_delta: open.system_delta,
        reads: stats.reads,
        writes: stats.writes,
        deletes: stats.deletes,
        read_latency: quantiles(&stats.read_hist),
        write_txn_latency: quantiles(&stats.write_hist),
        post_workload_settle,
        evidence: args.single.evidence(&path, setup, work, verification),
    };

    args.single.finish(&path, &result)
}
