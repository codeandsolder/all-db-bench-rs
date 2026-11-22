use crate::{
    dataset::{RecordDataset, Warmup},
    workload::{WorkExecutor, WorkMode, WorkSpec, WorkStats},
};
use all_db_bench_common::run_directory::{ReportMode, finish_database, prepare_database};
use all_db_bench_common::{
    client_summary::{ClientSummary, ClientTiming},
    concurrency::{WorkPartition, WorkerMeasurement, partition_work, run_workers},
    measurement::SetupTimings,
    metrics::{ProcDelta, SystemDelta, SystemSnapshot},
    record_payload::PayloadPattern,
};
use anyhow::{Result, bail};
use clap::{Args as ClapArgs, ValueEnum};
use hdrhistogram::Histogram;
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    time::Instant,
};

use crate::{
    Durability, Quantiles, READ_MATERIALIZATION, WRITE_MATERIALIZATION, Workload, count_as_f64,
    dir_size, hist, peak_rss_kib, quantiles,
};

use crate::backend::RecordBackend;

#[derive(Clone, Copy, Debug, Serialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum StateEvolution {
    Growth,
    Bounded,
}

#[derive(Clone, Debug, ClapArgs)]
pub struct ConcurrencyArgs {
    #[arg(long, value_enum)]
    durability: Durability,
    #[arg(long, value_enum)]
    workload: Workload,
    #[arg(long, value_enum, default_value_t = StateEvolution::Growth)]
    state_evolution: StateEvolution,
    #[arg(long, default_value_t = 10_000)]
    records: u64,
    /// Total logical operations across all clients, not operations per client.
    #[arg(long, default_value_t = 10_000)]
    ops: u64,
    #[arg(long, default_value_t = 512)]
    payload_bytes: usize,
    #[arg(long, default_value_t = 100)]
    txn_size: usize,
    #[arg(long, default_value_t = 1)]
    clients: usize,
    #[arg(long, default_value_t = 1)]
    trial: u32,
    #[arg(long, default_value_t = 0x5eed_2026)]
    seed: u64,
    #[arg(long, default_value = "record-concurrency")]
    scenario: String,
    #[arg(long, default_value = "data")]
    root: PathBuf,
    #[arg(long, default_value = "-")]
    output: String,
    #[arg(long, default_value_t = false)]
    keep_db: bool,
    #[arg(long, default_value_t = 5_000)]
    warmup_reads: u64,
}

#[derive(Debug, Serialize)]
struct ClientWorkload {
    reads: u64,
    writes: u64,
    transactions: u64,
    write_conflict_retries: u64,
    operation_latency: Quantiles,
    transaction_latency: Quantiles,
}

#[derive(Debug)]
struct ClientRun {
    timing: ClientTiming,
    reads: u64,
    writes: u64,
    transactions: u64,
    write_conflict_retries: u64,
    operation_hist: Histogram<u64>,
    transaction_hist: Histogram<u64>,
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
    state_evolution: StateEvolution,
    records: u64,
    ops_requested: u64,
    ops_completed: u64,
    clients: usize,
    payload_bytes: usize,
    txn_size: usize,
    read_materialization: &'static str,
    write_materialization: &'static str,
    trial: u32,
    seed: u64,
    scenario: String,
    #[serde(flatten)]
    setup: SetupTimings,
    client_setup_s: f64,
    elapsed_s: f64,
    ops_per_s: f64,
    reads: u64,
    writes: u64,
    transactions: u64,
    write_conflict_retries: u64,
    operation_latency: Quantiles,
    transaction_latency: Quantiles,
    #[serde(flatten)]
    client_summary: ClientSummary<ClientWorkload>,
    db_bytes: u64,
    peak_rss_kib: u64,
    measured_process: ProcDelta,
    system_before: SystemSnapshot,
    system_after: SystemSnapshot,
    measured_system_delta: SystemDelta,
    path: String,
    db_kept: bool,
    warmup_reads: u64,
    concurrency_handle: &'static str,
}

fn partition_ops(args: &ConcurrencyArgs) -> Result<Vec<WorkPartition>> {
    if args.clients == 0 || args.clients as u64 > args.ops {
        bail!("--clients must be in 1..=ops");
    }
    if matches!(args.workload, Workload::WriteBurst) {
        let txn = args.txn_size as u64;
        if txn == 0 || !args.ops.is_multiple_of(txn) {
            bail!("write-burst requires --ops to be a multiple of --txn-size");
        }
        if args.clients as u64 > args.ops / txn {
            bail!("write-burst requires at least one full transaction per client");
        }
        partition_work(args.ops, args.clients, txn)
    } else {
        partition_work(args.ops, args.clients, 1)
    }
}

async fn run_client<B: RecordBackend>(
    engine: &mut B,
    args: &ConcurrencyArgs,
    client: usize,
    ops: u64,
    global_offset: u64,
) -> Result<ClientRun> {
    let dataset = RecordDataset {
        records: args.records,
        payload_bytes: args.payload_bytes,
        payload_pattern: PayloadPattern::HexRandom,
        seed: args.seed,
    };
    let rng_seed = args.seed
        ^ u64::from(args.trial)
        ^ (u64::try_from(client)?
            .saturating_add(1)
            .wrapping_mul(0x9e37_79b9));
    let mut executor = WorkExecutor::new(
        WorkSpec {
            dataset,
            workload: args.workload,
            ops,
            txn_size: args.txn_size,
            rng_seed,
            mode: WorkMode::Concurrent {
                offset: global_offset,
                bounded: args.state_evolution == StateEvolution::Bounded,
            },
        },
        WorkStats::new()?,
    )?;
    let started = Instant::now();
    executor.run(engine).await?;
    let elapsed_s = started.elapsed().as_secs_f64();
    let stats = executor.stats;
    Ok(ClientRun {
        timing: ClientTiming {
            client,
            ops_requested: ops,
            ops_completed: stats.completed,
            elapsed_s,
        },
        reads: stats.reads,
        writes: stats.writes,
        transactions: stats.transactions,
        write_conflict_retries: stats.write_conflict_retries,
        operation_hist: stats.operation_hist,
        transaction_hist: stats.transaction_hist,
    })
}

struct AggregateRun {
    completed: u64,
    reads: u64,
    writes: u64,
    transactions: u64,
    write_conflict_retries: u64,
    operation_hist: Histogram<u64>,
    transaction_hist: Histogram<u64>,
    clients: Vec<ClientRun>,
    elapsed_s: f64,
    process: ProcDelta,
    system_before: SystemSnapshot,
    system_after: SystemSnapshot,
    system_delta: SystemDelta,
}

fn run_clients<B: RecordBackend>(
    clients: Vec<B>,
    args: &ConcurrencyArgs,
    runtime: &tokio::runtime::Handle,
    partitions: Vec<WorkPartition>,
) -> Result<AggregateRun> {
    if clients.len() != args.clients {
        bail!("client handle count mismatch");
    }
    let tasks = clients
        .into_iter()
        .zip(partitions)
        .enumerate()
        .map(|(client_index, (mut engine, partition))| {
            let args = args.clone();
            let runtime = runtime.clone();
            move || {
                runtime.block_on(run_client(
                    &mut engine,
                    &args,
                    client_index,
                    partition.count,
                    partition.offset,
                ))
            }
        })
        .collect();
    let WorkerMeasurement {
        clients: client_runs,
        elapsed_s,
        process,
        system_before,
        system_after,
        system_delta,
    } = run_workers("recordbench", tasks)?;

    let mut op_hist = hist()?;
    let mut tx_hist = hist()?;
    let mut completed = 0u64;
    let mut reads = 0u64;
    let mut writes = 0u64;
    let mut transactions = 0u64;
    let mut write_conflict_retries = 0u64;
    for c in &client_runs {
        op_hist += &c.operation_hist;
        tx_hist += &c.transaction_hist;
        completed += c.timing.ops_completed;
        reads += c.reads;
        writes += c.writes;
        transactions += c.transactions;
        write_conflict_retries += c.write_conflict_retries;
    }

    Ok(AggregateRun {
        completed,
        reads,
        writes,
        transactions,
        write_conflict_retries,
        operation_hist: op_hist,
        transaction_hist: tx_hist,
        clients: client_runs,
        elapsed_s,
        process,
        system_delta,
        system_before,
        system_after,
    })
}

fn report_measurement<B: RecordBackend>(
    engine_kind: B::Kind,
    args: &ConcurrencyArgs,
    path: &Path,
    setup: SetupTimings,
    client_setup_s: f64,
    concurrency_handle: &'static str,
    run: AggregateRun,
) -> Result<Measurement<B::Kind>> {
    let db_bytes = dir_size(path);
    let client_summary = ClientSummary::from_clients(run.clients.into_iter().map(|client| {
        (
            client.timing,
            ClientWorkload {
                reads: client.reads,
                writes: client.writes,
                transactions: client.transactions,
                write_conflict_retries: client.write_conflict_retries,
                operation_latency: quantiles(&client.operation_hist),
                transaction_latency: quantiles(&client.transaction_hist),
            },
        )
    }))?;

    Ok(Measurement {
        format_version: 8,
        lane: "record-concurrency",
        engine: engine_kind,
        engine_version: B::version(engine_kind),
        durability: args.durability,
        durability_mapping: B::mapping(engine_kind, args.durability),
        workload: args.workload,
        state_evolution: args.state_evolution,
        records: args.records,
        ops_requested: args.ops,
        ops_completed: run.completed,
        clients: args.clients,
        payload_bytes: args.payload_bytes,
        txn_size: args.txn_size,
        read_materialization: READ_MATERIALIZATION,
        write_materialization: WRITE_MATERIALIZATION,
        trial: args.trial,
        seed: args.seed,
        scenario: args.scenario.clone(),
        setup,
        client_setup_s,
        elapsed_s: run.elapsed_s,
        ops_per_s: count_as_f64(run.completed) / run.elapsed_s.max(f64::MIN_POSITIVE),
        reads: run.reads,
        writes: run.writes,
        transactions: run.transactions,
        write_conflict_retries: run.write_conflict_retries,
        operation_latency: quantiles(&run.operation_hist),
        transaction_latency: quantiles(&run.transaction_hist),
        client_summary,
        db_bytes,
        peak_rss_kib: peak_rss_kib(),
        measured_process: run.process,
        system_before: run.system_before,
        system_after: run.system_after,
        measured_system_delta: run.system_delta,
        path: path.display().to_string(),
        db_kept: args.keep_db,
        warmup_reads: args.warmup_reads,
        concurrency_handle,
    })
}

fn validate_args(args: &ConcurrencyArgs) -> Result<Vec<WorkPartition>> {
    if args.records == 0 {
        bail!("--records must be greater than zero");
    }
    if args.ops == 0 {
        bail!("--ops must be greater than zero");
    }
    if args.txn_size == 0 {
        bail!("--txn-size must be greater than zero");
    }
    if args.state_evolution == StateEvolution::Bounded
        && !matches!(args.workload, Workload::TinyTxn | Workload::WriteBurst)
    {
        bail!("bounded state evolution is supported only for tiny-txn and write-burst");
    }
    if args.state_evolution == StateEvolution::Bounded
        && matches!(args.workload, Workload::WriteBurst)
        && args.txn_size as u64 > args.records
    {
        bail!("bounded write-burst requires txn-size <= records");
    }
    if args.state_evolution == StateEvolution::Growth
        && matches!(args.workload, Workload::TinyTxn | Workload::WriteBurst)
        && args.records.checked_add(args.ops - 1).is_none()
    {
        bail!("append record ID range overflow");
    }
    partition_ops(args)
}

/// Run the shared record-concurrency benchmark harness.
///
/// # Errors
/// Returns an error for invalid benchmark arguments, backend failures, thread failures, or result I/O failures.
pub async fn run<B: RecordBackend>(engine_kind: B::Kind, args: ConcurrencyArgs) -> Result<()> {
    let partitions = validate_args(&args)?;

    let durability = &args.durability;
    let workload = &args.workload;
    let state_evolution = &args.state_evolution;
    let clients = args.clients;
    let records = args.records;
    let payload_bytes = args.payload_bytes;
    let txn_size = args.txn_size;
    let trial = args.trial;
    let run_name = format!(
        "{engine_kind:?}-{durability:?}-{workload:?}-se{state_evolution:?}-c{clients}-n{records}-p{payload_bytes}-tx{txn_size}-trial{trial}"
    )
    .to_lowercase()
    .replace('_', "-");
    let path = prepare_database(&args.root, &run_name, false)?;

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
    dataset
        .prefill(&mut engine, args.txn_size.clamp(100, 1000))
        .await?;
    let prefill_s = prefill_started.elapsed().as_secs_f64();

    let warmup_started = Instant::now();
    dataset
        .warm(&mut engine, args.warmup_reads, Warmup::Sequential)
        .await?;
    let warmup_s = warmup_started.elapsed().as_secs_f64();

    let client_setup_started = Instant::now();
    let (clients, concurrency_handle) = engine
        .make_clients(args.clients, &path, args.durability)
        .await?;
    let client_setup_s = client_setup_started.elapsed().as_secs_f64();
    drop(engine);
    let runtime = tokio::runtime::Handle::current();
    let run = tokio::task::block_in_place(|| run_clients(clients, &args, &runtime, partitions))?;
    if run.completed != args.ops {
        bail!(
            "record concurrency completed {} operations, expected {}",
            run.completed,
            args.ops
        );
    }

    let result = report_measurement::<B>(
        engine_kind,
        &args,
        &path,
        SetupTimings {
            open: open_s,
            prefill: prefill_s,
            warmup: warmup_s,
        },
        client_setup_s,
        concurrency_handle,
        run,
    )?;
    finish_database(
        &path,
        args.keep_db,
        &result,
        &args.output,
        ReportMode::AppendLine,
    )
}
