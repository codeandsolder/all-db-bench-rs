use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use hdrhistogram::Histogram;
use rand::{Rng, SeedableRng, rngs::SmallRng};
use serde::{Deserialize, Serialize};
pub(crate) mod metrics;
use metrics::{ProcDelta, ProcSnapshot, SystemDelta, SystemSnapshot};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use surrealdb::{
    Surreal,
    engine::local::{Db as SurrealLocalDb, RocksDb},
    types::SurrealValue,
};

const HIST_MAX_NS: u64 = 60_000_000_000;

#[derive(Clone, Copy, Debug, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum EngineKind {
    SurrealdbRocksdb,
}

#[derive(Clone, Copy, Debug, Serialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Durability {
    Relaxed,
    Sync,
}

#[derive(Clone, Copy, Debug, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Workload {
    PointRead,
    IndexedRead,
    ReadHeavy,
    TinyTxn,
    WriteBurst,
}

#[derive(Debug, Parser)]
#[command(about = "End-to-end record/document DB benchmark; separate from raw KV results")]
struct Args {
    #[arg(long, value_enum)]
    engine: EngineKind,
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
    #[arg(long, default_value_t = 100)]
    txn_size: usize,
    #[arg(long, default_value_t = 1)]
    trial: u32,
    #[arg(long, default_value_t = 0x5eed_2026)]
    seed: u64,
    #[arg(long, default_value = "baseline")]
    scenario: String,
    #[arg(long, default_value = "data")]
    root: PathBuf,
    #[arg(long, default_value = "-")]
    output: String,
    #[arg(long, default_value_t = false)]
    keep_db: bool,
    #[arg(long, default_value_t = false)]
    reuse_db: bool,
    #[arg(long, default_value_t = false)]
    skip_prefill: bool,
    #[arg(long, default_value_t = 5_000)]
    warmup_reads: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, SurrealValue)]
pub(crate) struct RecordData {
    pub(crate) bucket: u32,
    pub(crate) payload: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct Quantiles {
    count: u64,
    p50_us: f64,
    p95_us: f64,
    p99_us: f64,
    p999_us: f64,
    max_us: f64,
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
    payload_bytes: usize,
    txn_size: usize,
    trial: u32,
    seed: u64,
    scenario: String,
    open_s: f64,
    prefill_s: f64,
    warmup_s: f64,
    elapsed_s: f64,
    ops_per_s: f64,
    operation_latency: Quantiles,
    transaction_latency: Quantiles,
    db_bytes: u64,
    peak_rss_kib: u64,
    measured_process: ProcDelta,
    system_before: SystemSnapshot,
    system_after: SystemSnapshot,
    measured_system_delta: SystemDelta,
    path: String,
    db_kept: bool,
    reused_db: bool,
    prefill_skipped: bool,
    warmup_reads: u64,
}

pub(crate) enum Engine {
    SurrealRocksdb(Surreal<SurrealLocalDb>),
}

impl EngineKind {
    pub(crate) fn version(self) -> &'static str {
        "SurrealDB 3.3.0 / surrealdb-rocksdb 0.24.0-surreal.5"
    }

    pub(crate) fn mapping(self, d: Durability) -> &'static str {
        match d {
            Durability::Relaxed => "SurrealDB embedded RocksDB sync=never",
            Durability::Sync => "SurrealDB embedded RocksDB sync=every",
        }
    }
}

pub(crate) fn payload(id: u64, len: usize, salt: u64) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut x = id ^ salt ^ 0x9e37_79b9_7f4a_7c15;
    let mut out = String::with_capacity(len);
    for _ in 0..len {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        x = x.wrapping_mul(0x2545_f491_4f6c_dd1d);
        out.push(HEX[(x as usize) & 15] as char);
    }
    out
}

pub(crate) fn hist() -> Histogram<u64> {
    Histogram::new_with_bounds(1, HIST_MAX_NS, 3).unwrap()
}
pub(crate) fn record(h: &mut Histogram<u64>, d: Duration) {
    let ns = d.as_nanos().min(HIST_MAX_NS as u128) as u64;
    let _ = h.record(ns.max(1));
}
pub(crate) fn quantiles(h: &Histogram<u64>) -> Quantiles {
    let us = |v: u64| v as f64 / 1000.0;
    if h.is_empty() {
        return Quantiles {
            count: 0,
            p50_us: 0.0,
            p95_us: 0.0,
            p99_us: 0.0,
            p999_us: 0.0,
            max_us: 0.0,
        };
    }
    Quantiles {
        count: h.len(),
        p50_us: us(h.value_at_quantile(0.50)),
        p95_us: us(h.value_at_quantile(0.95)),
        p99_us: us(h.value_at_quantile(0.99)),
        p999_us: us(h.value_at_quantile(0.999)),
        max_us: us(h.max()),
    }
}
pub(crate) fn peak_rss_kib() -> u64 {
    let Ok(status) = fs::read_to_string("/proc/self/status") else {
        return 0;
    };
    status
        .lines()
        .find_map(|line| {
            let rest = line.strip_prefix("VmHWM:")?;
            rest.split_whitespace().next()?.parse().ok()
        })
        .unwrap_or(0)
}

pub(crate) fn dir_size(path: &Path) -> u64 {
    fn walk(p: &Path, sum: &mut u64) {
        let Ok(md) = fs::symlink_metadata(p) else {
            return;
        };
        if md.is_file() {
            *sum = sum.saturating_add(md.len());
            return;
        }
        let Ok(rd) = fs::read_dir(p) else { return };
        for e in rd.flatten() {
            walk(&e.path(), sum);
        }
    }
    let mut n = 0;
    walk(path, &mut n);
    n
}

impl Engine {
    pub(crate) async fn open(
        kind: EngineKind,
        durability: Durability,
        path: &Path,
    ) -> Result<Self> {
        match kind {
            EngineKind::SurrealdbRocksdb => {
                let sync_mode = match durability {
                    Durability::Relaxed => "never",
                    Durability::Sync => "every",
                };
                let db = Surreal::new::<RocksDb>(path.join("surrealdb-rocksdb"))
                    .sync(sync_mode)
                    .await?;
                db.use_ns("bench").use_db("bench").await?;
                db.query("DEFINE INDEX IF NOT EXISTS idx_group ON TABLE item FIELDS bucket")
                    .await?
                    .check()?;
                Ok(Self::SurrealRocksdb(db))
            }
        }
    }

    pub(crate) async fn get(&self, id: u64) -> Result<bool> {
        match self {
            Self::SurrealRocksdb(db) => {
                let row: Option<RecordData> = db.select(("item", id as i64)).await?;
                Ok(row.is_some())
            }
        }
    }

    pub(crate) async fn indexed_read(&self, group: u32) -> Result<usize> {
        match self {
            Self::SurrealRocksdb(db) => {
                let mut resp = db
                    .query("SELECT bucket, payload FROM item WHERE bucket = $bucket LIMIT 100")
                    .bind(("bucket", group))
                    .await?;
                let rows: Vec<RecordData> = resp.take(0)?;
                Ok(rows.len())
            }
        }
    }

    pub(crate) async fn upsert_one(&self, id: u64, data: &RecordData) -> Result<()> {
        match self {
            Self::SurrealRocksdb(db) => {
                let _: Option<RecordData> =
                    db.upsert(("item", id as i64)).content(data.clone()).await?;
            }
        }
        Ok(())
    }

    pub(crate) async fn upsert_batch(&self, rows: &[(u64, RecordData)]) -> Result<()> {
        match self {
            Self::SurrealRocksdb(db) => {
                let mut sql = String::from("BEGIN TRANSACTION;\n");
                for (id, data) in rows {
                    sql.push_str(&format!(
                        "UPSERT item:{} CONTENT {{ bucket: {}, payload: '{}' }};\n",
                        id, data.bucket, data.payload
                    ));
                }
                sql.push_str("COMMIT TRANSACTION;");
                db.query(sql).await?.check()?;
            }
        }
        Ok(())
    }
}

async fn prefill(engine: &Engine, args: &Args) -> Result<()> {
    let batch = args.txn_size.clamp(100, 1000);
    let mut rows = Vec::with_capacity(batch);
    for id in 0..args.records {
        rows.push((
            id,
            RecordData {
                bucket: (id % 100) as u32,
                payload: payload(id, args.payload_bytes, args.seed),
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

async fn run(
    engine: &Engine,
    args: &Args,
    op_hist: &mut Histogram<u64>,
    tx_hist: &mut Histogram<u64>,
) -> Result<u64> {
    let mut rng = SmallRng::seed_from_u64(args.seed ^ args.trial as u64);
    let mut done = 0u64;
    let mut next_id = args.records;

    match args.workload {
        Workload::PointRead => {
            while done < args.ops {
                let id = rng.random_range(0..args.records);
                let t = Instant::now();
                if !engine.get(id).await? {
                    bail!("prefilled record missing");
                }
                record(op_hist, t.elapsed());
                done += 1;
            }
        }
        Workload::IndexedRead => {
            while done < args.ops {
                let group = rng.random_range(0..100);
                let t = Instant::now();
                let _ = engine.indexed_read(group).await?;
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
                    let _ = engine.get(id).await?;
                    record(op_hist, t.elapsed());
                    done += 1;
                }
                if done < args.ops {
                    let id = rng.random_range(0..args.records);
                    let data = RecordData {
                        bucket: (id % 100) as u32,
                        payload: payload(id, args.payload_bytes, args.seed ^ done),
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
                    payload: payload(id, args.payload_bytes, args.seed ^ done),
                };
                let t = Instant::now();
                engine.upsert_one(id, &data).await?;
                record(tx_hist, t.elapsed());
                done += 1;
            }
        }
        Workload::WriteBurst => {
            let batch = args.txn_size.max(1);
            while done < args.ops {
                let n = (args.ops - done).min(batch as u64) as usize;
                let mut rows = Vec::with_capacity(n);
                for _ in 0..n {
                    let id = next_id;
                    next_id += 1;
                    rows.push((
                        id,
                        RecordData {
                            bucket: (id % 100) as u32,
                            payload: payload(id, args.payload_bytes, args.seed ^ done),
                        },
                    ));
                }
                let t = Instant::now();
                engine.upsert_batch(&rows).await?;
                record(tx_hist, t.elapsed());
                done += n as u64;
            }
        }
    }
    Ok(done)
}

#[tokio::main(flavor = "multi_thread", worker_threads = 1)]
async fn main() -> Result<()> {
    let args = Args::parse();
    let run_name = format!(
        "{:?}-{:?}-{:?}-n{}-p{}-tx{}-trial{}",
        args.engine,
        args.durability,
        args.workload,
        args.records,
        args.payload_bytes,
        args.txn_size,
        args.trial
    )
    .to_lowercase()
    .replace('_', "-");
    let path = args.root.join(run_name);
    if path.exists() && !args.reuse_db {
        fs::remove_dir_all(&path).context("remove stale run directory")?;
    }
    if args.reuse_db && !path.exists() {
        bail!(
            "--reuse-db requested but database path does not exist: {}",
            path.display()
        );
    }
    fs::create_dir_all(&path)?;

    let open_started = Instant::now();
    let engine = Engine::open(args.engine, args.durability, &path).await?;
    let open_s = open_started.elapsed().as_secs_f64();
    let prefill_s = if args.skip_prefill {
        0.0
    } else {
        let prefill_started = Instant::now();
        prefill(&engine, &args).await?;
        prefill_started.elapsed().as_secs_f64()
    };

    let warmup_started = Instant::now();
    for id in 0..args.records.min(args.warmup_reads) {
        let _ = engine.get(id).await?;
    }
    let warmup_s = warmup_started.elapsed().as_secs_f64();

    let mut op_hist = hist();
    let mut tx_hist = hist();
    let system_before = SystemSnapshot::capture();
    let process_before = ProcSnapshot::capture();
    let started = Instant::now();
    let completed = run(&engine, &args, &mut op_hist, &mut tx_hist).await?;
    let elapsed = started.elapsed();
    let process_after = ProcSnapshot::capture();
    let system_after = SystemSnapshot::capture();
    let measured_process = process_before.delta(&process_after, elapsed);
    let measured_system_delta = system_before.delta(&system_after);
    drop(engine);
    let db_bytes = dir_size(&path);

    let result = Measurement {
        format_version: 5,
        lane: "record",
        engine: args.engine,
        engine_version: args.engine.version(),
        durability: args.durability,
        durability_mapping: args.engine.mapping(args.durability),
        workload: args.workload,
        records: args.records,
        ops_requested: args.ops,
        ops_completed: completed,
        payload_bytes: args.payload_bytes,
        txn_size: args.txn_size,
        trial: args.trial,
        seed: args.seed,
        scenario: args.scenario.clone(),
        open_s,
        prefill_s,
        warmup_s,
        elapsed_s: elapsed.as_secs_f64(),
        ops_per_s: completed as f64 / elapsed.as_secs_f64(),
        operation_latency: quantiles(&op_hist),
        transaction_latency: quantiles(&tx_hist),
        db_bytes,
        peak_rss_kib: peak_rss_kib(),
        measured_process,
        system_before,
        system_after,
        measured_system_delta,
        path: path.display().to_string(),
        db_kept: args.keep_db,
        reused_db: args.reuse_db,
        prefill_skipped: args.skip_prefill,
        warmup_reads: args.warmup_reads,
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
        fs::remove_dir_all(&path).context("remove completed run database")?;
    }
    Ok(())
}
