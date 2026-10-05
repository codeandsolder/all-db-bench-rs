#![allow(clippy::too_many_lines)]
#![cfg(feature = "record")]

#[allow(dead_code)]
#[path = "recordbench.rs"]
mod base;

use anyhow::{Context, Result, bail};
use base::{Durability, Engine, EngineKind, RecordData, Workload};
use clap::Parser;
use hdrhistogram::Histogram;
use rand::{Rng, SeedableRng, rngs::SmallRng};
use rusqlite::Connection as SqliteConnection;
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Barrier, Condvar, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};
use surrealdb::{Error as SurrealError, types::QueryError};

#[derive(Clone, Debug, Parser)]
#[command(about = "Shared-database multi-client record/document DB scaling benchmark")]
struct Args {
    #[arg(long, value_enum)]
    engine: EngineKind,
    #[arg(long, value_enum)]
    durability: Durability,
    #[arg(long, value_enum)]
    workload: Workload,
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
struct ClientMeasurement {
    client: usize,
    ops_requested: u64,
    ops_completed: u64,
    elapsed_s: f64,
    ops_per_s: f64,
    reads: u64,
    writes: u64,
    transactions: u64,
    write_conflict_retries: u64,
    operation_latency: base::Quantiles,
    transaction_latency: base::Quantiles,
}

#[derive(Debug)]
struct ClientRun {
    client: usize,
    ops_requested: u64,
    ops_completed: u64,
    elapsed_s: f64,
    reads: u64,
    writes: u64,
    transactions: u64,
    write_conflict_retries: u64,
    operation_hist: Histogram<u64>,
    transaction_hist: Histogram<u64>,
}

#[derive(Debug, Serialize)]
struct Measurement {
    format_version: u32,
    lane: &'static str,
    engine: EngineKind,
    engine_version: &'static str,
    durability: Durability,
    durability_mapping: &'static str,
    workload: Workload,
    records: u64,
    ops_requested: u64,
    ops_completed: u64,
    clients: usize,
    payload_bytes: usize,
    txn_size: usize,
    trial: u32,
    seed: u64,
    scenario: String,
    open_s: f64,
    prefill_s: f64,
    warmup_s: f64,
    client_setup_s: f64,
    elapsed_s: f64,
    ops_per_s: f64,
    reads: u64,
    writes: u64,
    transactions: u64,
    write_conflict_retries: u64,
    operation_latency: base::Quantiles,
    transaction_latency: base::Quantiles,
    client_elapsed_s_min: f64,
    client_elapsed_s_median: f64,
    client_elapsed_s_max: f64,
    client_ops_per_s_min: f64,
    client_ops_per_s_median: f64,
    client_ops_per_s_max: f64,
    client_throughput_max_min_ratio: f64,
    client_measurements: Vec<ClientMeasurement>,
    db_bytes: u64,
    peak_rss_kib: u64,
    measured_process: base::metrics::ProcDelta,
    system_before: base::metrics::SystemSnapshot,
    system_after: base::metrics::SystemSnapshot,
    measured_system_delta: base::metrics::SystemDelta,
    path: String,
    db_kept: bool,
    warmup_reads: u64,
    concurrency_handle: &'static str,
}

async fn prefill(engine: &Engine, args: &Args) -> Result<()> {
    let batch = args.txn_size.clamp(100, 1000);
    let mut rows = Vec::with_capacity(batch);
    for id in 0..args.records {
        rows.push((
            id,
            RecordData {
                bucket: (id % 100) as u32,
                payload: base::payload(id, args.payload_bytes, args.seed),
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

fn partition_ops(args: &Args) -> Result<Vec<(u64, u64)>> {
    if args.clients == 0 || args.clients as u64 > args.ops {
        bail!("--clients must be in 1..=ops");
    }
    if matches!(args.workload, Workload::WriteBurst) {
        let txn = args.txn_size as u64;
        if !args.ops.is_multiple_of(txn) {
            bail!("write-burst requires --ops to be a multiple of --txn-size");
        }
        let transactions = args.ops / txn;
        if args.clients as u64 > transactions {
            bail!("write-burst requires at least one full transaction per client");
        }
        let base = transactions / args.clients as u64;
        let extra = transactions % args.clients as u64;
        let mut offset = 0u64;
        return Ok((0..args.clients)
            .map(|client| {
                let n_tx = base + u64::from((client as u64) < extra);
                let ops = n_tx * txn;
                let out = (ops, offset);
                offset += ops;
                out
            })
            .collect());
    }

    let base = args.ops / args.clients as u64;
    let extra = args.ops % args.clients as u64;
    let mut offset = 0u64;
    Ok((0..args.clients)
        .map(|client| {
            let ops = base + u64::from((client as u64) < extra);
            let out = (ops, offset);
            offset += ops;
            out
        })
        .collect())
}

const MAX_CONFLICT_RETRIES: u64 = 10_000;

fn is_surreal_transaction_conflict(err: &anyhow::Error) -> bool {
    err.downcast_ref::<SurrealError>()
        .is_some_and(|err| matches!(err.query_details(), Some(QueryError::TransactionConflict)))
}

async fn upsert_one_with_retry(engine: &Engine, id: u64, data: &RecordData) -> Result<u64> {
    let mut retries = 0u64;
    loop {
        match engine.upsert_one(id, data).await {
            Ok(()) => return Ok(retries),
            Err(err) if is_surreal_transaction_conflict(&err) && retries < MAX_CONFLICT_RETRIES => {
                retries += 1;
                if retries <= 8 {
                    std::hint::spin_loop();
                } else {
                    tokio::task::yield_now().await;
                }
            }
            Err(err) => return Err(err),
        }
    }
}

async fn upsert_batch_with_retry(engine: &Engine, rows: &[(u64, RecordData)]) -> Result<u64> {
    let mut retries = 0u64;
    loop {
        match engine.upsert_batch(rows).await {
            Ok(()) => return Ok(retries),
            Err(err) if is_surreal_transaction_conflict(&err) && retries < MAX_CONFLICT_RETRIES => {
                retries += 1;
                if retries <= 8 {
                    std::hint::spin_loop();
                } else {
                    tokio::task::yield_now().await;
                }
            }
            Err(err) => return Err(err),
        }
    }
}

async fn run_client(
    engine: &Engine,
    args: &Args,
    client: usize,
    ops: u64,
    global_offset: u64,
) -> Result<ClientRun> {
    let mut rng = SmallRng::seed_from_u64(
        args.seed ^ args.trial as u64 ^ ((client as u64 + 1).wrapping_mul(0x9e37_79b9)),
    );
    let mut done = 0u64;
    let mut reads = 0u64;
    let mut writes = 0u64;
    let mut transactions = 0u64;
    let mut write_conflict_retries = 0u64;
    let mut op_hist = base::hist();
    let mut tx_hist = base::hist();
    let mut next_id = args.records + global_offset;
    let started = Instant::now();

    match args.workload {
        Workload::PointRead => {
            while done < ops {
                let id = rng.random_range(0..args.records);
                let t = Instant::now();
                if !engine.get(id).await? {
                    bail!("prefilled record missing");
                }
                base::record(&mut op_hist, t.elapsed());
                done += 1;
                reads += 1;
            }
        }
        Workload::IndexedRead => {
            while done < ops {
                let group = rng.random_range(0..100);
                let t = Instant::now();
                let _ = engine.indexed_read(group).await?;
                base::record(&mut op_hist, t.elapsed());
                done += 1;
                reads += 1;
            }
        }
        Workload::ReadHeavy => {
            while done < ops {
                for _ in 0..19 {
                    if done >= ops {
                        break;
                    }
                    let id = rng.random_range(0..args.records);
                    let t = Instant::now();
                    if !engine.get(id).await? {
                        bail!("prefilled record missing");
                    }
                    base::record(&mut op_hist, t.elapsed());
                    done += 1;
                    reads += 1;
                }
                if done < ops {
                    let id = rng.random_range(0..args.records);
                    let data = RecordData {
                        bucket: (id % 100) as u32,
                        payload: base::payload(
                            id,
                            args.payload_bytes,
                            args.seed ^ global_offset ^ done,
                        ),
                    };
                    let t = Instant::now();
                    write_conflict_retries += upsert_one_with_retry(engine, id, &data).await?;
                    base::record(&mut op_hist, t.elapsed());
                    done += 1;
                    writes += 1;
                    transactions += 1;
                }
            }
        }
        Workload::TinyTxn => {
            while done < ops {
                let id = next_id;
                next_id += 1;
                let data = RecordData {
                    bucket: (id % 100) as u32,
                    payload: base::payload(
                        id,
                        args.payload_bytes,
                        args.seed ^ global_offset ^ done,
                    ),
                };
                let t = Instant::now();
                write_conflict_retries += upsert_one_with_retry(engine, id, &data).await?;
                base::record(&mut tx_hist, t.elapsed());
                done += 1;
                writes += 1;
                transactions += 1;
            }
        }
        Workload::WriteBurst => {
            let batch = args.txn_size as u64;
            while done < ops {
                let mut rows = Vec::with_capacity(args.txn_size);
                for _ in 0..batch {
                    let id = next_id;
                    next_id += 1;
                    rows.push((
                        id,
                        RecordData {
                            bucket: (id % 100) as u32,
                            payload: base::payload(
                                id,
                                args.payload_bytes,
                                args.seed ^ global_offset ^ done,
                            ),
                        },
                    ));
                }
                let t = Instant::now();
                write_conflict_retries += upsert_batch_with_retry(engine, &rows).await?;
                base::record(&mut tx_hist, t.elapsed());
                done += batch;
                writes += batch;
                transactions += 1;
            }
        }
    }

    Ok(ClientRun {
        client,
        ops_requested: ops,
        ops_completed: done,
        elapsed_s: started.elapsed().as_secs_f64(),
        reads,
        writes,
        transactions,
        write_conflict_retries,
        operation_hist: op_hist,
        transaction_hist: tx_hist,
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
    process: base::metrics::ProcDelta,
    system_before: base::metrics::SystemSnapshot,
    system_after: base::metrics::SystemSnapshot,
    system_delta: base::metrics::SystemDelta,
}

fn run_clients(
    clients: Vec<Engine>,
    args: &Args,
    runtime: &tokio::runtime::Handle,
) -> Result<AggregateRun> {
    let partitions = partition_ops(args)?;
    if clients.len() != args.clients {
        bail!("client handle count mismatch");
    }
    let start_barrier = Arc::new(Barrier::new(args.clients + 1));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let (tx, rx) = mpsc::channel::<(usize, Result<ClientRun>)>();
    let mut joins = Vec::with_capacity(args.clients);

    for (client_index, (engine, (ops, offset))) in clients.into_iter().zip(partitions).enumerate() {
        let thread_args = args.clone();
        let start_barrier = Arc::clone(&start_barrier);
        let release = Arc::clone(&release);
        let tx = tx.clone();
        let runtime = runtime.clone();
        joins.push(
            thread::Builder::new()
                .name(format!("recordbench-c{client_index}"))
                .spawn(move || {
                    start_barrier.wait();
                    let outcome = runtime.block_on(run_client(
                        &engine,
                        &thread_args,
                        client_index,
                        ops,
                        offset,
                    ));
                    let _ = tx.send((client_index, outcome));
                    let (lock, cv) = &*release;
                    let mut released = lock.lock().unwrap_or_else(|e| e.into_inner());
                    while !*released {
                        released = cv.wait(released).unwrap_or_else(|e| e.into_inner());
                    }
                })?,
        );
    }
    drop(tx);

    let system_before = base::metrics::SystemSnapshot::capture();
    let process_before = base::metrics::ProcSnapshot::capture();
    let started = Instant::now();
    start_barrier.wait();

    let mut client_runs = Vec::with_capacity(args.clients);
    let receive_result: Result<()> = (|| {
        for _ in 0..args.clients {
            let (client, result) = rx
                .recv()
                .context("client result channel closed before all clients completed")?;
            client_runs.push(result.with_context(|| format!("client {client} failed"))?);
        }
        Ok(())
    })();
    let elapsed = started.elapsed();
    let process_after = base::metrics::ProcSnapshot::capture();
    let system_after = base::metrics::SystemSnapshot::capture();

    {
        let (lock, cv) = &*release;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cv.notify_all();
    }
    for join in joins {
        join.join()
            .map_err(|_| anyhow::anyhow!("record concurrency client thread panicked"))?;
    }
    receive_result?;

    client_runs.sort_by_key(|r| r.client);
    let mut op_hist = base::hist();
    let mut tx_hist = base::hist();
    let mut completed = 0u64;
    let mut reads = 0u64;
    let mut writes = 0u64;
    let mut transactions = 0u64;
    let mut write_conflict_retries = 0u64;
    for c in &client_runs {
        op_hist += &c.operation_hist;
        tx_hist += &c.transaction_hist;
        completed += c.ops_completed;
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
        elapsed_s: elapsed.as_secs_f64(),
        process: process_before.delta(&process_after, elapsed),
        system_delta: system_before.delta(&system_after),
        system_before,
        system_after,
    })
}

fn sqlite_client(path: &Path, durability: Durability) -> Result<SqliteConnection> {
    let conn = SqliteConnection::open(path.join("sqlite.db"))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(
        None,
        "synchronous",
        match durability {
            Durability::Relaxed => "NORMAL",
            Durability::Sync => "FULL",
        },
    )?;
    // Contending writers should wait inside SQLite so lock wait is measured as
    // operation latency rather than converted into benchmark-side retries.
    conn.busy_timeout(Duration::from_secs(60))?;
    Ok(conn)
}

async fn make_clients(
    engine: &Engine,
    args: &Args,
    path: &Path,
) -> Result<(Vec<Engine>, &'static str)> {
    match engine {
        Engine::Surreal(db) => Ok((
            (0..args.clients)
                .map(|_| Engine::Surreal(db.clone()))
                .collect(),
            "native Surreal client clones",
        )),
        Engine::Turso(_) => {
            let db_path = path.join("turso.db");
            let db_path = db_path
                .to_str()
                .context("Turso database path is not valid UTF-8")?;
            let db = turso::Builder::new_local(db_path).build().await?;
            let mut clients = Vec::with_capacity(args.clients);
            for _ in 0..args.clients {
                let conn = db.connect()?;
                conn.busy_timeout(Duration::from_secs(60))?;
                conn.execute(
                    &format!(
                        "PRAGMA synchronous={}",
                        match args.durability {
                            Durability::Relaxed => "NORMAL",
                            Durability::Sync => "FULL",
                        }
                    ),
                    (),
                )
                .await?;
                clients.push(Engine::Turso(conn));
            }
            Ok((clients, "independent Turso Database::connect() connections"))
        }
        Engine::Sqlite(_) => {
            let mut clients = Vec::with_capacity(args.clients);
            for _ in 0..args.clients {
                clients.push(Engine::Sqlite(sqlite_client(path, args.durability)?));
            }
            Ok((
                clients,
                "independent SQLite WAL connections; 60s busy timeout",
            ))
        }
    }
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    let n = values.len();
    if n.is_multiple_of(2) {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    } else {
        values[n / 2]
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    if args.records == 0 {
        bail!("--records must be greater than zero");
    }
    if args.ops == 0 {
        bail!("--ops must be greater than zero");
    }
    if args.txn_size == 0 {
        bail!("--txn-size must be greater than zero");
    }
    let _ = partition_ops(&args)?;

    let run_name = format!(
        "{:?}-{:?}-{:?}-c{}-n{}-p{}-tx{}-trial{}",
        args.engine,
        args.durability,
        args.workload,
        args.clients,
        args.records,
        args.payload_bytes,
        args.txn_size,
        args.trial,
    )
    .to_lowercase()
    .replace('_', "-");
    let path = args.root.join(run_name);
    if path.exists() {
        fs::remove_dir_all(&path).context("remove stale record concurrency run directory")?;
    }
    fs::create_dir_all(&path)?;

    let open_started = Instant::now();
    let engine = Engine::open(args.engine, args.durability, &path).await?;
    let open_s = open_started.elapsed().as_secs_f64();

    let prefill_started = Instant::now();
    prefill(&engine, &args).await?;
    let prefill_s = prefill_started.elapsed().as_secs_f64();

    let warmup_started = Instant::now();
    for id in 0..args.records.min(args.warmup_reads) {
        let _ = engine.get(id).await?;
    }
    let warmup_s = warmup_started.elapsed().as_secs_f64();

    let client_setup_started = Instant::now();
    let (clients, concurrency_handle) = make_clients(&engine, &args, &path).await?;
    let client_setup_s = client_setup_started.elapsed().as_secs_f64();
    drop(engine);
    let runtime = tokio::runtime::Handle::current();
    let run = tokio::task::block_in_place(|| run_clients(clients, &args, &runtime))?;
    if run.completed != args.ops {
        bail!(
            "record concurrency completed {} operations, expected {}",
            run.completed,
            args.ops
        );
    }

    let db_bytes = base::dir_size(&path);
    let mut client_elapsed = Vec::with_capacity(run.clients.len());
    let mut client_rates = Vec::with_capacity(run.clients.len());
    let mut client_measurements = Vec::with_capacity(run.clients.len());
    for c in run.clients {
        let rate = c.ops_completed as f64 / c.elapsed_s.max(f64::MIN_POSITIVE);
        client_elapsed.push(c.elapsed_s);
        client_rates.push(rate);
        client_measurements.push(ClientMeasurement {
            client: c.client,
            ops_requested: c.ops_requested,
            ops_completed: c.ops_completed,
            elapsed_s: c.elapsed_s,
            ops_per_s: rate,
            reads: c.reads,
            writes: c.writes,
            transactions: c.transactions,
            write_conflict_retries: c.write_conflict_retries,
            operation_latency: base::quantiles(&c.operation_hist),
            transaction_latency: base::quantiles(&c.transaction_hist),
        });
    }
    let elapsed_min = client_elapsed.iter().copied().fold(f64::INFINITY, f64::min);
    let elapsed_max = client_elapsed.iter().copied().fold(0.0, f64::max);
    let rate_min = client_rates.iter().copied().fold(f64::INFINITY, f64::min);
    let rate_max = client_rates.iter().copied().fold(0.0, f64::max);

    let result = Measurement {
        format_version: 6,
        lane: "record-concurrency",
        engine: args.engine,
        engine_version: args.engine.version(),
        durability: args.durability,
        durability_mapping: args.engine.mapping(args.durability),
        workload: args.workload,
        records: args.records,
        ops_requested: args.ops,
        ops_completed: run.completed,
        clients: args.clients,
        payload_bytes: args.payload_bytes,
        txn_size: args.txn_size,
        trial: args.trial,
        seed: args.seed,
        scenario: args.scenario.clone(),
        open_s,
        prefill_s,
        warmup_s,
        client_setup_s,
        elapsed_s: run.elapsed_s,
        ops_per_s: run.completed as f64 / run.elapsed_s.max(f64::MIN_POSITIVE),
        reads: run.reads,
        writes: run.writes,
        transactions: run.transactions,
        write_conflict_retries: run.write_conflict_retries,
        operation_latency: base::quantiles(&run.operation_hist),
        transaction_latency: base::quantiles(&run.transaction_hist),
        client_elapsed_s_min: elapsed_min,
        client_elapsed_s_median: median(client_elapsed),
        client_elapsed_s_max: elapsed_max,
        client_ops_per_s_min: rate_min,
        client_ops_per_s_median: median(client_rates),
        client_ops_per_s_max: rate_max,
        client_throughput_max_min_ratio: rate_max / rate_min.max(f64::MIN_POSITIVE),
        client_measurements,
        db_bytes,
        peak_rss_kib: base::peak_rss_kib(),
        measured_process: run.process,
        system_before: run.system_before,
        system_after: run.system_after,
        measured_system_delta: run.system_delta,
        path: path.display().to_string(),
        db_kept: args.keep_db,
        warmup_reads: args.warmup_reads,
        concurrency_handle,
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
        fs::remove_dir_all(&path).context("remove completed record concurrency database")?;
    }
    Ok(())
}
