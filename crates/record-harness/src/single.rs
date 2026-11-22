use all_db_bench_common::{
    measurement::{MeasurementInterval, SetupTimings},
    single::{SingleEvidence, SingleOptions},
};
use all_db_bench_common::{
    record_payload::{PayloadPattern, payload_for},
    recovery::{PresenceCounts, RecoveryVerification},
};
use anyhow::{Result, bail};
use clap::Args as ClapArgs;
use hdrhistogram::Histogram;
use rand::{RngExt, SeedableRng, rngs::SmallRng};
use serde::Serialize;
use std::time::Instant;

use crate::{
    Durability, Quantiles, READ_MATERIALIZATION, RecordData, WRITE_MATERIALIZATION, Workload,
    backend::RecordBackend, hist, quantiles, record,
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

#[expect(
    clippy::future_not_send,
    reason = "record backends are thread-confined: each backend is owned and polled to completion on one worker thread, so requiring Sync would reject valid backends such as rusqlite"
)]
async fn prefill<B: RecordBackend>(engine: &B, args: &SingleArgs) -> Result<()> {
    let batch = args.txn_size.clamp(100, 1000);
    let mut rows = Vec::with_capacity(batch);
    for id in 0..args.records {
        rows.push((
            id,
            RecordData {
                bucket: (id % 100) as u32,
                payload: payload_for(args.payload_pattern, id, args.payload_bytes, args.seed),
            },
        ));
        if rows.len() == batch {
            engine.upsert_batch(&rows).await?;
            rows.clear();
        }
    }
    if !rows.is_empty() {
        engine.upsert_batch(&rows).await?;
    }
    Ok(())
}

#[expect(
    clippy::too_many_lines,
    reason = "the workload state machine is clearer as one exhaustive workload match"
)]
#[expect(
    clippy::future_not_send,
    reason = "record backends are thread-confined: each backend is owned and polled to completion on one worker thread, so requiring Sync would reject valid backends such as rusqlite"
)]
async fn run_workload<B: RecordBackend>(
    engine: &B,
    args: &SingleArgs,
    op_hist: &mut Histogram<u64>,
    tx_hist: &mut Histogram<u64>,
) -> Result<u64> {
    let mut rng = SmallRng::seed_from_u64(args.seed ^ u64::from(args.trial));
    let mut done = 0u64;
    let mut next_id = args.records;

    match args.workload {
        Workload::PointRead => {
            while done < args.ops {
                let id = rng.random_range(0..args.records);
                let t = Instant::now();
                let read = engine.get(id).await?;
                if read.is_none() {
                    bail!("prefilled record missing");
                }
                std::hint::black_box(read);
                record(op_hist, t.elapsed());
                done += 1;
            }
        }
        Workload::IndexedRead => {
            while done < args.ops {
                let group = rng.random_range(0..100);
                let t = Instant::now();
                let read = engine.indexed_read(group).await?;
                std::hint::black_box(read);
                record(op_hist, t.elapsed());
                done += 1;
            }
        }
        Workload::ReadHeavy => {
            while done < args.ops {
                for _ in 0..19 {
                    if done >= args.ops {
                        break;
                    }
                    let id = rng.random_range(0..args.records);
                    let t = Instant::now();
                    let read = engine.get(id).await?;
                    if read.is_none() {
                        bail!("prefilled record missing");
                    }
                    std::hint::black_box(read);
                    record(op_hist, t.elapsed());
                    done += 1;
                }
                if done < args.ops {
                    let id = rng.random_range(0..args.records);
                    let data = RecordData {
                        bucket: (id % 100) as u32,
                        payload: payload_for(
                            args.payload_pattern,
                            id,
                            args.payload_bytes,
                            args.seed ^ done,
                        ),
                    };
                    let t = Instant::now();
                    engine.upsert_one(id, &data).await?;
                    record(op_hist, t.elapsed());
                    done += 1;
                }
            }
        }
        Workload::TinyTxn => {
            while done < args.ops {
                let id = next_id;
                next_id += 1;
                let data = RecordData {
                    bucket: (id % 100) as u32,
                    payload: payload_for(
                        args.payload_pattern,
                        id,
                        args.payload_bytes,
                        args.seed ^ done,
                    ),
                };
                let t = Instant::now();
                engine.upsert_one(id, &data).await?;
                record(tx_hist, t.elapsed());
                done += 1;
                args.single.checkpoint(done)?;
            }
        }
        Workload::WriteBurst => {
            let batch = args.txn_size.max(1);
            while done < args.ops {
                let n = usize::try_from(args.ops - done)
                    .unwrap_or(usize::MAX)
                    .min(batch);
                let mut rows = Vec::with_capacity(n);
                for _ in 0..n {
                    let id = next_id;
                    next_id += 1;
                    rows.push((
                        id,
                        RecordData {
                            bucket: (id % 100) as u32,
                            payload: payload_for(
                                args.payload_pattern,
                                id,
                                args.payload_bytes,
                                args.seed ^ done,
                            ),
                        },
                    ));
                }
                let t = Instant::now();
                engine.upsert_batch(&rows).await?;
                record(tx_hist, t.elapsed());
                done = done.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
                args.single.checkpoint(done)?;
            }
        }
    }
    Ok(done)
}

#[expect(
    clippy::future_not_send,
    reason = "record backends are thread-confined: each backend is owned and polled to completion on one worker thread, so requiring Sync would reject valid backends such as rusqlite"
)]
async fn verify_recovery<B: RecordBackend>(
    engine: &B,
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

    let durability = &args.durability;
    let workload = &args.workload;
    let records = args.records;
    let payload_bytes = args.payload_bytes;
    let txn_size = args.txn_size;
    let trial = args.trial;
    let run_name = format!(
        "{engine_kind:?}-{durability:?}-{workload:?}-n{records}-p{payload_bytes}-tx{txn_size}-trial{trial}"
    )
    .to_lowercase()
    .replace('_', "-");
    let path = args.single.prepare(&run_name)?;

    let open_started = Instant::now();
    let engine = B::open(engine_kind, args.durability, &path).await?;
    let open_s = open_started.elapsed().as_secs_f64();
    let prefill_s = if args.single.skip_prefill {
        0.0
    } else {
        let prefill_started = Instant::now();
        prefill(&engine, &args).await?;
        prefill_started.elapsed().as_secs_f64()
    };
    let verification = if let Some(expected_prefix_records) = args.single.verify_prefix_records {
        Some(
            verify_recovery(
                &engine,
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
    for id in 0..args.records.min(args.single.warmup_reads) {
        std::hint::black_box(engine.get(id).await?);
    }
    let warmup_s = warmup_started.elapsed().as_secs_f64();

    let mut op_hist = hist()?;
    let mut tx_hist = hist()?;
    let foreground = MeasurementInterval::start();
    let completed = run_workload(&engine, &args, &mut op_hist, &mut tx_hist).await?;
    let work = foreground.finish().fixed_work(args.ops, completed)?;
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
        operation_latency: quantiles(&op_hist),
        transaction_latency: quantiles(&tx_hist),
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
