use crate::{
    dataset::{RecordDataset, Warmup},
    workload::{WorkExecutor, WorkMode, WorkSpec, WorkStats},
};
use all_db_bench_common::{
    measurement::{MeasurementInterval, SetupTimings},
    single::{SingleEvidence, SingleOptions},
};
use all_db_bench_common::{
    record_payload::PayloadPattern,
    recovery::{PresenceCounts, RecoveryVerification},
};
use anyhow::{Result, bail};
use clap::Args as ClapArgs;
use serde::Serialize;
use std::time::Instant;

use crate::{
    Durability, Quantiles, READ_MATERIALIZATION, WRITE_MATERIALIZATION, Workload,
    backend::RecordBackend, quantiles,
};

#[derive(Clone, Debug, ClapArgs)]
pub struct SingleArgs {
    #[arg(long, value_enum)]
    durability: Durability,
    #[arg(long, value_enum)]
    workload: Workload,
    #[arg(long, default_value_t = 10_000)]
    records: u64,
    #[arg(long, default_value_t = 10_000)]
    ops: u64,
    #[arg(long, default_value_t = 512)]
    payload_bytes: usize,
    #[arg(long, value_enum, default_value = "hex-random")]
    payload_pattern: PayloadPattern,
    #[arg(long, default_value_t = 100)]
    txn_size: usize,
    #[arg(long, default_value_t = 1)]
    trial: u32,
    #[arg(long, default_value_t = 0x5eed_2026)]
    seed: u64,
    #[arg(long, default_value = "baseline")]
    scenario: String,
    #[command(flatten)]
    single: SingleOptions,
}

#[derive(Debug, Serialize)]
struct Measurement<K> {
    format_version: u32,
    lane: &'static str,
    engine: K,
    engine_version: &'static str,
    durability: Durability,
    durability_mapping: &'static str,
    workload: Workload,
    records: u64,
    payload_bytes: usize,
    payload_pattern: PayloadPattern,
    txn_size: usize,
    read_materialization: &'static str,
    write_materialization: &'static str,
    trial: u32,
    seed: u64,
    scenario: String,
    operation_latency: Quantiles,
    transaction_latency: Quantiles,

    #[serde(flatten)]
    evidence: SingleEvidence,
}

async fn verify_recovery<B: RecordBackend>(
    engine: &mut B,
    args: &SingleArgs,
    expected_prefix_records: u64,
    tail_records: u64,
) -> Result<RecoveryVerification> {
    let started = Instant::now();
    let mut prefix = PresenceCounts::default();
    for id in 0..expected_prefix_records {
        prefix.observe(engine.get(id).await?.is_some());
    }
    let mut tail = PresenceCounts::default();
    for id in expected_prefix_records..expected_prefix_records.saturating_add(tail_records) {
        tail.observe(engine.get(id).await?.is_some());
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

fn run_name<K: std::fmt::Debug>(engine_kind: K, args: &SingleArgs) -> String {
    let durability = &args.durability;
    let workload = &args.workload;
    let records = args.records;
    let payload_bytes = args.payload_bytes;
    let txn_size = args.txn_size;
    let trial = args.trial;
    format!(
        "{engine_kind:?}-{durability:?}-{workload:?}-n{records}-p{payload_bytes}-tx{txn_size}-trial{trial}"
    )
    .to_lowercase()
    .replace('_', "-")
}

/// Run the shared single-client record benchmark harness.
///
/// # Errors
/// Returns an error for invalid benchmark arguments, backend failures, or result I/O failures.
pub async fn run<B: RecordBackend>(engine_kind: B::Kind, args: SingleArgs) -> Result<()> {
    if args.records == 0 || args.ops == 0 || args.txn_size == 0 {
        bail!("--records, --ops and --txn-size must be greater than zero");
    }
    if args.single.progress_file.is_some()
        && !matches!(args.workload, Workload::TinyTxn | Workload::WriteBurst)
    {
        bail!("--progress-file requires tiny-txn or write-burst");
    }

    let dataset = RecordDataset {
        records: args.records,
        payload_bytes: args.payload_bytes,
        payload_pattern: args.payload_pattern,
        seed: args.seed,
    };
    let spec = WorkSpec {
        dataset,
        workload: args.workload,
        ops: args.ops,
        txn_size: args.txn_size,
        rng_seed: args.seed ^ u64::from(args.trial),
        mode: WorkMode::Single(&args.single),
    };
    spec.validate()?;
    let run_name = run_name(engine_kind, &args);
    let path = args.single.prepare(&run_name)?;

    let open_started = Instant::now();
    let mut engine = B::open(engine_kind, args.durability, &path).await?;
    let open_s = open_started.elapsed().as_secs_f64();
    let prefill_s = if args.single.skip_prefill {
        0.0
    } else {
        let prefill_started = Instant::now();
        dataset
            .prefill(&mut engine, args.txn_size.clamp(100, 1000))
            .await?;
        prefill_started.elapsed().as_secs_f64()
    };
    let verification = if let Some(expected_prefix_records) = args.single.verify_prefix_records {
        Some(
            verify_recovery(
                &mut engine,
                &args,
                expected_prefix_records,
                args.single.verify_tail_records,
            )
            .await?,
        )
    } else {
        None
    };

    let warmup_started = Instant::now();
    dataset
        .warm(&mut engine, args.single.warmup_reads, Warmup::Sequential)
        .await?;
    let warmup_s = warmup_started.elapsed().as_secs_f64();

    let stats = WorkStats::new()?;
    let foreground = MeasurementInterval::start();
    let mut executor = WorkExecutor::new(spec, stats)?;
    executor.run(&mut engine).await?;
    let work = foreground
        .finish()
        .fixed_work(args.ops, executor.stats.completed)?;
    drop(engine);

    let result = Measurement {
        format_version: 8,
        lane: "record",
        engine: engine_kind,
        engine_version: B::version(engine_kind),
        durability: args.durability,
        durability_mapping: B::mapping(engine_kind, args.durability),
        workload: args.workload,
        records: args.records,
        payload_bytes: args.payload_bytes,
        payload_pattern: args.payload_pattern,
        txn_size: args.txn_size,
        read_materialization: READ_MATERIALIZATION,
        write_materialization: WRITE_MATERIALIZATION,
        trial: args.trial,
        seed: args.seed,
        scenario: args.scenario.clone(),
        operation_latency: quantiles(&executor.stats.operation_hist),
        transaction_latency: quantiles(&executor.stats.transaction_hist),
        evidence: args.single.evidence(
            &path,
            SetupTimings {
                open: open_s,
                prefill: prefill_s,
                warmup: warmup_s,
            },
            work,
            verification,
        ),
    };
    args.single.finish(&path, &result)
}
