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
    fixed_work::{WorkExecutor, WorkMode, WorkSpec, WorkStats},
    lifecycle::{DatasetSpec, prefill, warm_existing_reads},
    metrics::{ProcDelta, Quantiles, SettleMeasurement, SystemDelta, measure_settle, quantiles},
    types::{
        AccessPattern, Durability, EngineCompression, EngineKind, KeyShape, ValuePattern, Workload,
        WritePattern,
    },
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
                .read(&key(id, args.key_bytes, args.key_shape, args.seed))
                .await?
                .is_some(),
        );
    }
    let mut tail = PresenceCounts::default();
    for id in expected_prefix_records..expected_prefix_records.saturating_add(tail_records) {
        tail.observe(
            engine
                .read(&key(id, args.key_bytes, args.key_shape, args.seed))
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
    let dataset = args.work_spec().dataset;
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

impl Args {
    const fn work_spec(&self) -> WorkSpec {
        WorkSpec {
            dataset: DatasetSpec {
                records: self.records,
                key_bytes: self.key_bytes,
                key_shape: self.key_shape,
                seed: self.seed,
                value_bytes: self.value_bytes,
                value_pattern: self.value_pattern,
                durability: self.durability,
            },
            workload: self.workload,
            total_ops: self.ops,
            access_pattern: self.access_pattern,
            miss_percent: self.miss_percent,
            write_pattern: self.write_pattern,
            txn_size: self.txn_size,
            scan_len: self.scan_len,
        }
    }
}

/// Run one single-client benchmark invocation and emit its measurement record.
///
/// # Errors
/// Returns an error for invalid arguments, backend failures, verification failures, or result I/O failures.
pub async fn run() -> Result<()> {
    let args = Args::parse();
    let spec = args.work_spec();
    spec.validate(args.engine, 0)?;
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

    let stats = WorkStats::new()?;
    let foreground = MeasurementInterval::start();
    let mut executor = WorkExecutor::new(
        spec,
        all_db_bench_common::concurrency::WorkPartition {
            offset: 0,
            count: args.ops,
        },
        WorkMode::Single(&args.single),
        args.seed ^ u64::from(args.trial),
        None,
        stats,
    )?;
    executor.run(&mut engine).await?;
    let stats = executor.stats;
    let work = foreground.finish().fixed_work(args.ops, stats.completed)?;
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
