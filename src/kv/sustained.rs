use super::backend_ops::{BackendOps, RetryMode};
use super::{
    data::{key, sample_existing_id, value},
    engine::Engine,
    lifecycle::{DatasetSpec, prefill},
    metrics::{dir_size, measure_settle, peak_rss_kib},
    types::{AccessPattern, Durability, EngineKind, KeyShape, ValuePattern, Workload},
};
use all_db_bench_common::run_directory::{ReportMode, finish_database, prepare_database};
use all_db_bench_common::{
    measurement::SetupTimings,
    sustained::{Foreground, MutationStream, SustainedEvidence, SustainedPattern, WorkPlan},
};
use anyhow::{Result, bail};
use clap::Parser;
use rand::{RngExt, SeedableRng, rngs::SmallRng};
use serde::Serialize;
use std::{path::PathBuf, time::Instant};

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
    window_ops: u64,
    value_bytes: usize,
    value_pattern: ValuePattern,
    key_bytes: usize,
    key_shape: KeyShape,
    txn_size: usize,
    trial: u32,
    seed: u64,
    scenario: String,
    #[serde(flatten)]
    evidence: SustainedEvidence,
}

async fn warm_reads(engine: &mut Engine, args: &Args) -> Result<()> {
    let mut rng = SmallRng::seed_from_u64(args.seed ^ u64::from(args.trial) ^ 0x7777);
    for _ in 0..args.records.min(args.warmup_reads) {
        let id = rng.random_range(0..args.records);
        let _ = engine
            .read(&key(id, args.key_bytes, args.key_shape, args.seed))
            .await?;
    }
    Ok(())
}

fn prepare_path(args: &Args) -> Result<PathBuf> {
    let name = format!(
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
    .replace('_', "-");
    prepare_database(&args.root, &name, false)
}

async fn prepare_data(engine: &mut Engine, args: &Args, open_s: f64) -> Result<SetupTimings> {
    let dataset = DatasetSpec {
        records: args.records,
        key_bytes: args.key_bytes,
        key_shape: args.key_shape,
        seed: args.seed,
        value_bytes: args.value_bytes,
        value_pattern: args.value_pattern,
        durability: args.durability,
    };
    let prefill_started = Instant::now();
    prefill(engine, dataset, 0).await?;
    let prefill_s = prefill_started.elapsed().as_secs_f64();
    let warmup_started = Instant::now();
    warm_reads(engine, args).await?;
    let warmup_s = warmup_started.elapsed().as_secs_f64();

    Ok(SetupTimings {
        open: open_s,
        prefill: prefill_s,
        warmup: warmup_s,
    })
}

fn report(args: &Args, mapping: String, evidence: SustainedEvidence) -> Measurement {
    Measurement {
        format_version: 6,
        lane: String::from("kv-sustained"),
        engine: args.engine,
        engine_version: args.engine.version().to_string(),
        durability: args.durability,
        durability_mapping: mapping,
        pattern: args.pattern,
        records: args.records,
        ops_requested: args.ops,
        window_ops: args.window_ops,
        value_bytes: args.value_bytes,
        value_pattern: args.value_pattern,
        key_bytes: args.key_bytes,
        key_shape: args.key_shape,
        txn_size: args.txn_size,
        trial: args.trial,
        seed: args.seed,
        scenario: args.scenario.clone(),
        evidence,
    }
}

/// Run the sustained-write benchmark and emit windowed measurements.
///
/// # Errors
/// Returns an error for invalid arguments, backend failures, or result I/O failures.
pub async fn run() -> Result<()> {
    let args = Args::parse();
    let plan = WorkPlan::new(args.records, args.ops, args.window_ops, args.txn_size)?;
    if args.key_bytes < 8 {
        bail!("--key-bytes must be at least 8");
    }
    if matches!(args.engine, EngineKind::ManifoldWal) {
        bail!(
            "manifold-wal is a known-broken recovery diagnostic and is excluded from sustained rankings"
        );
    }

    let mapping = args.engine.durability_mapping(args.durability)?;
    let path = prepare_path(&args)?;

    let open_started = Instant::now();
    let mut engine = Engine::open(args.engine, args.durability, &path).await?;
    let open_s = open_started.elapsed().as_secs_f64();
    let setup = prepare_data(&mut engine, &args, open_s).await?;

    let db_bytes_before = dir_size(&path);
    let mut run = Foreground::start(plan)?;
    let mut stream = MutationStream::new(args.pattern, plan, args.seed, args.trial);
    let put_bytes = u64::try_from(args.key_bytes.saturating_add(args.value_bytes))?;
    let delete_bytes = u64::try_from(args.key_bytes)?;
    for work in plan.windows() {
        let mut window = run.start_window(work)?;
        for start in work.transactions() {
            let batch = stream.batch(
                work,
                start,
                |rng, records| {
                    sample_existing_id(rng, records, AccessPattern::Hot95, Workload::WriteBurst)
                },
                |id, salt| {
                    (
                        key(id, args.key_bytes, args.key_shape, args.seed),
                        value(id, args.value_bytes, salt, args.value_pattern),
                    )
                },
                |id| key(id, args.key_bytes, args.key_shape, args.seed),
            )?;
            let started = Instant::now();
            engine
                .commit(
                    args.durability,
                    &batch.puts,
                    &batch.deletes,
                    RetryMode::SingleAttempt,
                )
                .await?;
            run.record_transaction(
                &mut window,
                &batch,
                put_bytes,
                delete_bytes,
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
    engine.close().await?;
    drop(engine);
    let db_bytes_final = dir_size(&path);

    let evidence = SustainedEvidence {
        setup,
        foreground: foreground.measurement(),
        db_bytes_before,
        db_bytes_after_foreground,
        db_bytes_final,
        post_workload_settle,
        peak_rss_kib: peak_rss_kib(),
        path: path.display().to_string(),
        db_kept: args.keep_db,
    };
    let result = report(&args, mapping.to_string(), evidence);

    finish_database(
        &path,
        args.keep_db,
        &result,
        &args.output,
        ReportMode::ReplacePretty,
    )
}
