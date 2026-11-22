use super::{
    engine::Engine,
    lifecycle::{DatasetSpec, prefill, warm_existing_reads},
    metrics::{
        ProcDelta, Quantiles, SystemDelta, SystemSnapshot, count_as_f64, dir_size, hist,
        peak_rss_kib, quantiles,
    },
    types::{
        AccessPattern, Durability, EngineKind, KeyShape, ValuePattern, Workload, WritePattern,
    },
};
use all_db_bench_common::concurrency::{WorkerMeasurement, partition_work, run_workers};
use all_db_bench_common::{client_summary::ClientSummary, measurement::SetupTimings};
use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use hdrhistogram::Histogram;
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use super::concurrency_clients::{
    ClientOps, FjallClient, HeedClient, ManifoldClient, ParityClient, RedbClient, SledClient,
    SurrealClient, TurboClient,
};
#[cfg(feature = "kv-experimental")]
use super::concurrency_clients::{JammClient, LsmClient, RoughClient};
#[cfg(feature = "kv-external")]
use super::concurrency_clients::{MdbxClient, PersyClient, RocksClient};

mod workload;
use workload::{ClientRun, run_client, verify_bounded_churn_state};

#[derive(Clone, Copy, Debug, Serialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum StateEvolution {
    Growth,
    Bounded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DatabaseMode {
    Fresh,
    Prepare,
    Reuse,
}

#[derive(Clone, Debug, clap::Args)]
struct DatabasePreparation {
    /// Create and prefill a fresh database, close it cleanly, and exit without measuring.
    #[arg(long, default_value_t = false, conflicts_with = "reuse_db")]
    prepare_only: bool,
    /// Reopen an already prepared database and skip prefill.
    #[arg(long, default_value_t = false, conflicts_with = "prepare_only")]
    reuse_db: bool,
}

impl DatabasePreparation {
    const fn mode(&self) -> DatabaseMode {
        if self.prepare_only {
            DatabaseMode::Prepare
        } else if self.reuse_db {
            DatabaseMode::Reuse
        } else {
            DatabaseMode::Fresh
        }
    }
}

#[derive(Clone, Debug, Parser)]
#[command(about = "Shared-database multi-client scaling benchmark")]
struct Args {
    #[arg(long, value_enum)]
    engine: EngineKind,
    #[arg(long, value_enum)]
    durability: Durability,
    #[arg(long, value_enum)]
    workload: Workload,
    #[arg(long, default_value_t = 100_000)]
    records: u64,
    /// Total logical operations across all clients, not operations per client.
    #[arg(long, default_value_t = 50_000)]
    ops: u64,
    #[arg(long, default_value_t = 256)]
    value_bytes: usize,
    #[arg(long, value_enum, default_value_t = ValuePattern::PseudoRandom)]
    value_pattern: ValuePattern,
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
    #[arg(long, value_enum, default_value_t = StateEvolution::Growth)]
    state_evolution: StateEvolution,
    #[arg(long, default_value_t = 8_192)]
    bounded_churn_slots: u64,
    #[arg(long, default_value_t = false)]
    verify_bounded_state: bool,
    #[arg(long, default_value_t = 100)]
    txn_size: usize,
    #[arg(long, default_value_t = 100)]
    scan_len: u64,
    #[arg(long, default_value_t = 1)]
    clients: usize,
    #[arg(long, default_value_t = 1)]
    trial: u32,
    #[arg(long, default_value_t = 0x5eed_2026)]
    seed: u64,
    #[arg(long, default_value = "concurrency")]
    scenario: String,
    #[arg(long, default_value = "data")]
    root: PathBuf,
    #[arg(long, default_value = "-")]
    output: String,
    #[arg(long, default_value_t = false)]
    keep_db: bool,
    #[command(flatten)]
    database: DatabasePreparation,
    #[arg(long, default_value_t = 5_000)]
    warmup_reads: u64,
}

#[derive(Debug, Serialize)]
struct ClientWorkload {
    reads: u64,
    writes: u64,
    deletes: u64,
    write_conflict_retries: u64,
    read_latency: Quantiles,
    write_txn_latency: Quantiles,
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
    value_bytes: usize,
    value_pattern: ValuePattern,
    key_bytes: usize,
    key_shape: KeyShape,
    access_pattern: AccessPattern,
    miss_percent: u8,
    write_pattern: WritePattern,
    state_evolution: StateEvolution,
    bounded_churn_slots: u64,
    verify_bounded_state: bool,
    bounded_state_verified: bool,
    txn_size: usize,
    scan_len: u64,
    trial: u32,
    seed: u64,
    scenario: String,
    #[serde(flatten)]
    setup: SetupTimings,
    elapsed_s: f64,
    ops_per_s: f64,
    reads: u64,
    writes: u64,
    deletes: u64,
    write_conflict_retries: u64,
    read_latency: Quantiles,
    write_txn_latency: Quantiles,
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

struct AggregateRun {
    completed: u64,
    reads: u64,
    writes: u64,
    deletes: u64,
    write_conflict_retries: u64,
    read_hist: Histogram<u64>,
    write_hist: Histogram<u64>,
    clients: Vec<ClientRun>,
    bounded_state_verified: bool,
    elapsed_s: f64,
    process: ProcDelta,
    system_before: SystemSnapshot,
    system_after: SystemSnapshot,
    system_delta: SystemDelta,
}

fn run_clients<C: ClientOps>(
    prototype: C,
    args: &Args,
    runtime: &tokio::runtime::Handle,
) -> Result<AggregateRun> {
    let slots = args.churn_pool_slots();
    let tasks = partition_work(args.ops, args.clients, 1)?
        .into_iter()
        .zip(partition_work(slots, args.clients, 1)?)
        .enumerate()
        .map(|(client, (ops, slots))| {
            let mut engine = prototype.clone();
            let args = args.clone();
            let runtime = runtime.clone();
            move || {
                runtime.block_on(run_client(
                    &mut engine,
                    &args,
                    client,
                    ops.count,
                    ops.offset,
                    slots.offset,
                    slots.count,
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
    } = run_workers("dbbench", tasks)?;

    let bounded_state_verified = if args.verify_bounded_state {
        let mut verifier = prototype;
        runtime.block_on(verify_bounded_churn_state(&mut verifier, args))?;
        true
    } else {
        false
    };

    let mut read_hist = hist()?;
    let mut write_hist = hist()?;
    let mut completed = 0u64;
    let mut reads = 0u64;
    let mut writes = 0u64;
    let mut deletes = 0u64;
    let mut write_conflict_retries = 0u64;
    for client in &client_runs {
        read_hist += &client.read_hist;
        write_hist += &client.write_hist;
        completed = completed.saturating_add(client.timing.ops_completed);
        reads = reads.saturating_add(client.reads);
        writes = writes.saturating_add(client.writes);
        deletes = deletes.saturating_add(client.deletes);
        write_conflict_retries =
            write_conflict_retries.saturating_add(client.write_conflict_retries);
    }

    Ok(AggregateRun {
        completed,
        reads,
        writes,
        deletes,
        write_conflict_retries,
        read_hist,
        write_hist,
        clients: client_runs,
        bounded_state_verified,
        elapsed_s,
        process,
        system_delta,
        system_before,
        system_after,
    })
}

fn run_clients_without_starving_runtime<C: ClientOps>(
    prototype: C,
    args: &Args,
) -> Result<AggregateRun> {
    let runtime = tokio::runtime::Handle::current();
    tokio::task::block_in_place(|| run_clients(prototype, args, &runtime))
}

async fn dispatch(engine: Engine, args: &Args) -> Result<(&'static str, AggregateRun)> {
    match engine {
        Engine::Redb(db) => {
            let shared = Arc::new(db);
            Ok((
                "Arc<redb::Database>",
                run_clients_without_starving_runtime(RedbClient(shared), args)?,
            ))
        }
        Engine::Fjall { db, keyspace } => Ok((
            "native Clone handles",
            run_clients_without_starving_runtime(FjallClient { db, keyspace }, args)?,
        )),
        Engine::Surrealkv(tree) => {
            let run = run_clients_without_starving_runtime(SurrealClient(tree.clone()), args)?;
            // The last handle's Drop schedules an asynchronous shutdown. Wait for
            // it explicitly before the caller measures/removes the database files.
            tree.close().await?;
            Ok(("native Clone handle", run))
        }
        Engine::Heed { env, db } => Ok((
            "native Clone Env/Database handles",
            run_clients_without_starving_runtime(HeedClient { env, db }, args)?,
        )),
        Engine::Sled(db) => Ok((
            "one native sled::Db clone per client; handle is Send+Clone but !Sync",
            run_clients_without_starving_runtime(SledClient(*db), args)?,
        )),
        Engine::Lkv(_) => bail!(
            "lkv concurrency unsupported: write transactions require &mut Database and 0.2.1 exposes no cloneable/shared writer handle; benchmark-side mutex would fake concurrency"
        ),
        Engine::Manifold { _db: db, cf } => {
            let shared = Arc::new(db);
            Ok((
                "shared Arc<ColumnFamilyDatabase> owner + native ColumnFamily clones",
                run_clients_without_starving_runtime(ManifoldClient { _db: shared, cf }, args)?,
            ))
        }
        Engine::Turbokv(mut slot) => {
            let db = slot.take().context("TurboKV missing database handle")?;
            let shared = Arc::new(db);
            let run = run_clients_without_starving_runtime(TurboClient(Arc::clone(&shared)), args)?;
            let db = Arc::try_unwrap(shared)
                .map_err(|_| anyhow::anyhow!("TurboKV client retained shared handle"))?;
            db.close().await?;
            Ok(("Arc<turbokv::Db>", run))
        }
        Engine::ParitydbHash(db) => Ok((
            "Arc<parity_db::Db> hash column",
            run_clients_without_starving_runtime(
                ParityClient {
                    db: Arc::new(db),
                    ordered: false,
                },
                args,
            )?,
        )),
        Engine::ParitydbBtree(db) => Ok((
            "Arc<parity_db::Db> B-tree column",
            run_clients_without_starving_runtime(
                ParityClient {
                    db: Arc::new(db),
                    ordered: true,
                },
                args,
            )?,
        )),
        #[cfg(feature = "kv-external")]
        Engine::Rocksdb(db) => Ok((
            "Arc<rocksdb::DB>",
            run_clients_without_starving_runtime(RocksClient(Arc::new(db)), args)?,
        )),
        #[cfg(feature = "kv-external")]
        Engine::Mdbx(db) => Ok((
            "Arc<libmdbx::Database>",
            run_clients_without_starving_runtime(MdbxClient(Arc::new(db)), args)?,
        )),
        #[cfg(feature = "kv-external")]
        Engine::Persy(db) => Ok((
            "native Persy clone per client",
            run_clients_without_starving_runtime(PersyClient(db), args)?,
        )),
        #[cfg(feature = "kv-experimental")]
        Engine::Roughdb(db) => Ok((
            "Arc<roughdb::Db>",
            run_clients_without_starving_runtime(RoughClient(Arc::new(db)), args)?,
        )),
        #[cfg(feature = "kv-experimental")]
        Engine::Jammdb(db) => Ok((
            "native jammdb::DB clone per client",
            run_clients_without_starving_runtime(JammClient(db), args)?,
        )),
        #[cfg(feature = "kv-experimental")]
        Engine::Lsmdb(db) => Ok((
            "Arc<lsm_db::Lsm>",
            run_clients_without_starving_runtime(LsmClient(Arc::new(db)), args)?,
        )),
    }
}

fn report_measurement(
    args: &Args,
    path: &Path,
    setup: SetupTimings,
    durability_mapping: &'static str,
    concurrency_handle: &'static str,
    run: AggregateRun,
) -> Result<Measurement> {
    let db_bytes = dir_size(path);
    let client_summary = ClientSummary::from_clients(run.clients.into_iter().map(|client| {
        (
            client.timing,
            ClientWorkload {
                reads: client.reads,
                writes: client.writes,
                deletes: client.deletes,
                write_conflict_retries: client.write_conflict_retries,
                read_latency: quantiles(&client.read_hist),
                write_txn_latency: quantiles(&client.write_hist),
            },
        )
    }))?;

    Ok(Measurement {
        format_version: 6,
        lane: "kv-concurrency",
        engine: args.engine,
        engine_version: args.engine.version(),
        durability: args.durability,
        durability_mapping,
        workload: args.workload,
        records: args.records,
        ops_requested: args.ops,
        ops_completed: run.completed,
        clients: args.clients,
        value_bytes: args.value_bytes,
        value_pattern: args.value_pattern,
        key_bytes: args.key_bytes,
        key_shape: args.key_shape,
        access_pattern: args.access_pattern,
        miss_percent: args.miss_percent,
        write_pattern: args.write_pattern,
        state_evolution: args.state_evolution,
        bounded_churn_slots: if args.state_evolution == StateEvolution::Bounded {
            args.bounded_churn_slots
        } else {
            0
        },
        verify_bounded_state: args.verify_bounded_state,
        bounded_state_verified: run.bounded_state_verified,
        txn_size: args.txn_size,
        scan_len: args.scan_len,
        trial: args.trial,
        seed: args.seed,
        scenario: args.scenario.clone(),
        setup,
        elapsed_s: run.elapsed_s,
        ops_per_s: count_as_f64(run.completed) / run.elapsed_s.max(f64::MIN_POSITIVE),
        reads: run.reads,
        writes: run.writes,
        deletes: run.deletes,
        write_conflict_retries: run.write_conflict_retries,
        read_latency: quantiles(&run.read_hist),
        write_txn_latency: quantiles(&run.write_hist),
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

impl Args {
    const fn uses_churn_pool(&self) -> bool {
        matches!(self.state_evolution, StateEvolution::Bounded)
            && matches!(self.workload, Workload::Balanced | Workload::Churn)
    }

    const fn churn_pool_slots(&self) -> u64 {
        if self.uses_churn_pool() {
            self.bounded_churn_slots
        } else {
            0
        }
    }
}

fn validate_args(args: &Args) -> Result<()> {
    if args.records == 0 {
        bail!("--records must be greater than zero");
    }
    if args.ops == 0 {
        bail!("--ops must be greater than zero");
    }
    if args.clients == 0 || args.clients as u64 > args.ops {
        bail!("--clients must be in 1..=ops");
    }
    if args.key_bytes < 8 {
        bail!("--key-bytes must be at least 8");
    }
    if args.miss_percent > 100 {
        bail!("--miss-percent must be in 0..=100");
    }
    if args.key_shape == KeyShape::Hashed && matches!(args.workload, Workload::RangeScan) {
        bail!("hashed keys are incompatible with ID-ordered range-scan");
    }
    if matches!(args.workload, Workload::DeleteBurst) && args.ops > args.records {
        bail!("delete-burst requires total ops <= records");
    }
    if args.verify_bounded_state && !args.uses_churn_pool() {
        bail!("--verify-bounded-state requires bounded balanced or churn workload");
    }
    if args.state_evolution == StateEvolution::Bounded {
        if matches!(args.workload, Workload::DeleteBurst) {
            bail!(
                "bounded state evolution is unsupported for delete-burst; it is a finite transition diagnostic"
            );
        }
        if matches!(args.workload, Workload::TinyTxn | Workload::WriteBurst)
            && args.write_pattern == WritePattern::Append
        {
            bail!(
                "bounded tiny-txn/write-burst requires update-uniform or update-hot write pattern"
            );
        }
        if matches!(args.workload, Workload::Balanced | Workload::Churn) {
            if args.write_pattern != WritePattern::UpdateUniform {
                bail!("bounded balanced/churn requires canonical write-pattern=update-uniform");
            }
            if args.txn_size < 2 {
                bail!(
                    "bounded balanced/churn requires txn-size >= 2 for paired delete/insert toggles"
                );
            }
            let minimum_slots = (args.clients as u64).saturating_mul(300);
            if args.bounded_churn_slots < minimum_slots {
                bail!(
                    "bounded-churn-slots must be >= clients*300 ({minimum_slots}) to avoid slot reuse within a 1000-op epoch"
                );
            }
            args.records
                .checked_add(args.bounded_churn_slots.saturating_mul(2))
                .context("bounded churn key-id range overflow")?;
        }
    }
    if matches!(args.engine, EngineKind::Lkv) {
        bail!(
            "lkv concurrency unsupported: no native shared/cloneable writer handle; external serialization is intentionally not benchmarked"
        );
    }

    Ok(())
}

fn prepare_run_path(args: &Args, mode: DatabaseMode) -> Result<PathBuf> {
    let run_name = format!(
        "{:?}-{:?}-{:?}-c{}-n{}-k{}-{:?}-v{}-{:?}-tx{}-scan{}-trial{}",
        args.engine,
        args.durability,
        args.workload,
        args.clients,
        args.records,
        args.key_bytes,
        args.key_shape,
        args.value_bytes,
        args.value_pattern,
        args.txn_size,
        args.scan_len,
        args.trial,
    )
    .to_lowercase()
    .replace('_', "-");
    let path = args.root.join(run_name);
    if path.exists() && mode != DatabaseMode::Reuse {
        fs::remove_dir_all(&path).context("remove stale concurrency run directory")?;
    }
    if mode == DatabaseMode::Reuse && !path.exists() {
        bail!(
            "--reuse-db requested but prepared database path does not exist: {}",
            path.display()
        );
    }
    fs::create_dir_all(&path)?;

    Ok(path)
}

/// Run the shared-database multi-client scaling benchmark.
///
/// # Errors
/// Returns an error for invalid arguments, unsupported engine semantics, backend failures, or result I/O failures.
pub async fn run() -> Result<()> {
    let args = Args::parse();
    validate_args(&args)?;

    let durability_mapping = args.engine.durability_mapping(args.durability)?;
    let mode = args.database.mode();
    let path = prepare_run_path(&args, mode)?;

    let (setup, (concurrency_handle, run)) = {
        let open_started = Instant::now();
        let mut engine = Engine::open(args.engine, args.durability, &path).await?;
        let open_s = open_started.elapsed().as_secs_f64();
        let dataset = DatasetSpec {
            records: args.records,
            key_bytes: args.key_bytes,
            key_shape: args.key_shape,
            seed: args.seed,
            value_bytes: args.value_bytes,
            value_pattern: args.value_pattern,
            durability: args.durability,
        };

        let prefill_s = if mode == DatabaseMode::Reuse {
            0.0
        } else {
            let prefill_started = Instant::now();
            prefill(&mut engine, dataset, args.churn_pool_slots()).await?;
            prefill_started.elapsed().as_secs_f64()
        };

        if mode == DatabaseMode::Prepare {
            engine.close().await?;
            drop(engine);
            return Ok(());
        }

        let warmup_started = Instant::now();
        warm_existing_reads(
            &mut engine,
            dataset,
            args.warmup_reads,
            args.access_pattern,
            args.workload,
        )
        .await?;
        let warmup_s = warmup_started.elapsed().as_secs_f64();

        (
            SetupTimings {
                open: open_s,
                prefill: prefill_s,
                warmup: warmup_s,
            },
            dispatch(engine, &args).await?,
        )
    };
    if run.completed != args.ops {
        bail!(
            "concurrency run completed {} operations, expected {}",
            run.completed,
            args.ops
        );
    }

    let result = report_measurement(
        &args,
        &path,
        setup,
        durability_mapping,
        concurrency_handle,
        run,
    )?;
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
        fs::remove_dir_all(&path).context("remove completed concurrency database")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Args, DatabaseMode};
    use anyhow::Result;
    use clap::Parser;

    #[test]
    fn preparation_cli_resolves_to_one_database_mode() -> Result<()> {
        let base = [
            "kvconcurrency",
            "--engine",
            "redb",
            "--durability",
            "sync",
            "--workload",
            "point-read",
        ];
        assert_eq!(
            Args::try_parse_from(base)?.database.mode(),
            DatabaseMode::Fresh
        );
        assert_eq!(
            Args::try_parse_from(base.into_iter().chain(["--prepare-only"]))?
                .database
                .mode(),
            DatabaseMode::Prepare
        );
        assert_eq!(
            Args::try_parse_from(base.into_iter().chain(["--reuse-db"]))?
                .database
                .mode(),
            DatabaseMode::Reuse
        );
        assert!(
            Args::try_parse_from(base.into_iter().chain(["--prepare-only", "--reuse-db"])).is_err()
        );
        Ok(())
    }
}
