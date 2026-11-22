use crate::dataset::{RecordDataset, Warmup};
use all_db_bench_common::run_directory::{ReportMode, finish_database, prepare_database};
use all_db_bench_common::{
    measurement::SetupTimings,
    measurement::measure_settle,
    record_payload::PayloadPattern,
    sustained::{Foreground, MutationStream, SustainedEvidence, SustainedPattern, WorkPlan},
};
use anyhow::Result;
use clap::Args as ClapArgs;
use rand::{RngExt, rngs::SmallRng};
use serde::Serialize;
use std::{path::PathBuf, time::Instant};

use crate::{
    Durability, READ_MATERIALIZATION, WRITE_MATERIALIZATION, backend::RecordBackend, dir_size,
    peak_rss_kib,
};

#[derive(Clone, Debug, ClapArgs)]
pub struct SustainedArgs {
    #[arg(long, value_enum)]
    durability: Durability,
    #[arg(long, value_enum)]
    pattern: SustainedPattern,
    #[arg(long, default_value_t = 20_000)]
    records: u64,
    #[arg(long, default_value_t = 50_000)]
    ops: u64,
    #[arg(long, default_value_t = 2_000)]
    window_ops: u64,
    #[arg(long, default_value_t = 512)]
    payload_bytes: usize,
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
    #[arg(long, default_value = "record-sustained")]
    scenario: String,
    #[arg(long, default_value = "data")]
    root: PathBuf,
    #[arg(long, default_value = "-")]
    output: String,
    #[arg(long, default_value_t = false)]
    keep_db: bool,
}

#[derive(Debug, Serialize)]
struct Measurement<K> {
    format_version: u32,
    lane: &'static str,
    engine: K,
    engine_version: &'static str,
    durability: Durability,
    durability_mapping: &'static str,
    pattern: SustainedPattern,
    records: u64,
    ops_requested: u64,
    window_ops: u64,
    payload_bytes: usize,
    txn_size: usize,
    read_materialization: &'static str,
    write_materialization: &'static str,
    trial: u32,
    seed: u64,
    scenario: String,
    #[serde(flatten)]
    evidence: SustainedEvidence,
}

fn logical_put_bytes(payload_bytes: usize) -> u64 {
    12u64.saturating_add(u64::try_from(payload_bytes).unwrap_or(u64::MAX))
}

fn hot95_id(rng: &mut SmallRng, records: u64) -> u64 {
    let hot = (records / 20).max(1).min(records);
    if hot == records || rng.random_range(0..100u32) < 95 {
        rng.random_range(0..hot)
    } else {
        rng.random_range(hot..records)
    }
}

fn prepare_path<K: std::fmt::Debug>(engine_kind: K, args: &SustainedArgs) -> Result<PathBuf> {
    let durability = &args.durability;
    let pattern = &args.pattern;
    let records = args.records;
    let ops = args.ops;
    let window_ops = args.window_ops;
    let payload_bytes = args.payload_bytes;
    let txn_size = args.txn_size;
    let trial = args.trial;
    let name = format!(
            "record-sustained-{engine_kind:?}-{durability:?}-{pattern:?}-n{records}-o{ops}-w{window_ops}-p{payload_bytes}-tx{txn_size}-t{trial}"
        )
        .to_lowercase()
        .replace('_', "-");
    prepare_database(&args.root, &name, false)
}

fn report<B: RecordBackend>(
    engine_kind: B::Kind,
    args: &SustainedArgs,
    evidence: SustainedEvidence,
) -> Measurement<B::Kind> {
    Measurement {
        format_version: 7,
        lane: "record-sustained",
        engine: engine_kind,
        engine_version: B::version(engine_kind),
        durability: args.durability,
        durability_mapping: B::mapping(engine_kind, args.durability),
        pattern: args.pattern,
        records: args.records,
        ops_requested: args.ops,
        window_ops: args.window_ops,
        payload_bytes: args.payload_bytes,
        txn_size: args.txn_size,
        read_materialization: READ_MATERIALIZATION,
        write_materialization: WRITE_MATERIALIZATION,
        trial: args.trial,
        seed: args.seed,
        scenario: args.scenario.clone(),
        evidence,
    }
}

/// Run the shared sustained record benchmark harness.
///
/// # Errors
/// Returns an error for invalid benchmark arguments, backend failures, or result I/O failures.
pub async fn run<B: RecordBackend>(engine_kind: B::Kind, args: SustainedArgs) -> Result<()> {
    let plan = WorkPlan::new(args.records, args.ops, args.window_ops, args.txn_size)?;
    let path = prepare_path(engine_kind, &args)?;

    let dataset = RecordDataset {
        records: args.records,
        payload_bytes: args.payload_bytes,
        payload_pattern: PayloadPattern::HexRandom,
        seed: args.seed,
    };
    let open_started = Instant::now();
    let mut engine = B::open(engine_kind, args.durability, &path).await?;
    let open_s = open_started.elapsed().as_secs_f64();
    let prefill_started = Instant::now();
    dataset.prefill(&mut engine, 1_000).await?;
    let prefill_s = prefill_started.elapsed().as_secs_f64();
    let warmup_started = Instant::now();
    dataset
        .warm(
            &mut engine,
            args.warmup_reads,
            Warmup::Random {
                seed: args.seed ^ u64::from(args.trial) ^ 0x7777,
            },
        )
        .await?;
    let warmup_s = warmup_started.elapsed().as_secs_f64();

    let db_bytes_before = dir_size(&path);
    let mut run = Foreground::start(plan)?;
    let mut stream = MutationStream::new(args.pattern, plan, args.seed, args.trial);
    let churn = matches!(args.pattern, SustainedPattern::Churn);
    for work in plan.windows() {
        let mut window = run.start_window(work)?;
        for start in work.transactions() {
            let batch = stream.batch(
                work,
                start,
                hot95_id,
                |id, salt| (id, dataset.row(id, salt)),
                |id| id,
            )?;
            let started = Instant::now();
            if churn {
                engine.write_batch(&batch.puts, &batch.deletes).await?;
            } else {
                engine.upsert_batch(&batch.puts).await?;
            }
            run.record_transaction(
                &mut window,
                &batch,
                logical_put_bytes(args.payload_bytes),
                8,
                started.elapsed(),
            )?;
        }
        run.end_window(window)?;
    }
    let foreground = run.finish()?;
    let db_bytes_after_foreground = dir_size(&path);
    let post_workload_settle = measure_settle(
        &path,
        args.settle_ms,
        args.settle_sample_ms,
        tokio::time::sleep,
    )
    .await;
    drop(engine);
    let db_bytes_final = dir_size(&path);

    let evidence = SustainedEvidence {
        setup: SetupTimings {
            open: open_s,
            prefill: prefill_s,
            warmup: warmup_s,
        },
        foreground: foreground.measurement(),
        db_bytes_before,
        db_bytes_after_foreground,
        db_bytes_final,
        post_workload_settle,
        peak_rss_kib: peak_rss_kib(),
        path: path.display().to_string(),
        db_kept: args.keep_db,
    };
    let result = report::<B>(engine_kind, &args, evidence);

    finish_database(
        &path,
        args.keep_db,
        &result,
        &args.output,
        ReportMode::ReplacePretty,
    )
}
