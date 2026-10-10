#![cfg(feature = "record")]

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use hdrhistogram::Histogram;
use rand::{RngExt, SeedableRng, rngs::SmallRng};
use serde::{Deserialize, Serialize};
#[path = "../metrics.rs"]
pub(crate) mod metrics;
use metrics::{ProcDelta, ProcSnapshot, SystemDelta, SystemSnapshot};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use surrealdb::{
    Surreal,
    engine::local::{Db as SurrealLocalDb, SurrealKv},
    types::{RecordId, SurrealValue},
};

use rusqlite::{Connection as SqliteConnection, OptionalExtension, params};

const HIST_MAX_NS: u64 = 60_000_000_000;

#[derive(Clone, Copy, Debug, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum EngineKind {
    Surrealdb,
    Turso,
    Sqlite,
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
    #[arg(long, default_value_t = 100)]
    indexed_read_limit: usize,
    #[arg(long)]
    sql_cache_kib: Option<u32>,
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
    #[arg(long)]
    db_name: Option<String>,
    #[arg(long)]
    progress_file: Option<PathBuf>,
    #[arg(long)]
    verify_prefix_records: Option<u64>,
    #[arg(long, default_value_t = 0)]
    verify_tail_records: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, SurrealValue)]
pub(crate) struct RecordData {
    pub(crate) bucket: u32,
    pub(crate) payload: String,
}

pub(crate) const READ_MATERIALIZATION: &str = "full-record-v1";
pub(crate) const WRITE_MATERIALIZATION: &str = "no-return-v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReadDigest {
    bucket: u32,
    payload_len: usize,
}

impl ReadDigest {
    fn new(bucket: u32, payload_len: usize) -> Self {
        Self {
            bucket,
            payload_len,
        }
    }

    fn checksum(self) -> u64 {
        (u64::from(self.bucket) << 32) ^ self.payload_len as u64
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct IndexedRead {
    pub(crate) rows: usize,
    pub(crate) checksum: u64,
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
struct Verification {
    expected_prefix_records: u64,
    checked_prefix_records: u64,
    missing_prefix_records: u64,
    prefix_contiguous_present: u64,
    prefix_present_after_gap: u64,
    tail_checked_records: u64,
    tail_prefix_present: u64,
    tail_total_present: u64,
    tail_present_after_gap: u64,
    transaction_atomic_tail: bool,
    verification_ok: bool,
    verify_s: f64,
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
    indexed_read_limit: usize,
    sql_cache_kib: Option<u32>,
    sql_cache_pragma_value: Option<i64>,
    read_materialization: &'static str,
    write_materialization: &'static str,
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
    verification: Option<Verification>,
}

pub(crate) enum Engine {
    Surreal(Surreal<SurrealLocalDb>),
    Turso(turso::Connection),
    Sqlite(SqliteConnection),
}

impl EngineKind {
    pub(crate) fn version(self) -> &'static str {
        match self {
            Self::Surrealdb => "3.3.2 / SurrealKV 0.21.4",
            Self::Turso => "0.8.2",
            Self::Sqlite => "rusqlite 0.40.2 / SQLite 3.53.4",
        }
    }
    pub(crate) fn mapping(self, d: Durability) -> &'static str {
        match (self, d) {
            (Self::Surrealdb, Durability::Relaxed) => "SurrealDB embedded SurrealKV sync=never",
            (Self::Surrealdb, Durability::Sync) => "SurrealDB embedded SurrealKV sync=every",
            (Self::Turso, Durability::Relaxed) => "Turso PRAGMA synchronous=NORMAL",
            (Self::Turso, Durability::Sync) => "Turso PRAGMA synchronous=FULL",
            (Self::Sqlite, Durability::Relaxed) => {
                "SQLite 3.53.4 WAL journal_mode + synchronous=NORMAL"
            }
            (Self::Sqlite, Durability::Sync) => "SQLite 3.53.4 WAL journal_mode + synchronous=FULL",
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
            EngineKind::Surrealdb => {
                let sync_mode = match durability {
                    Durability::Relaxed => "never",
                    Durability::Sync => "every",
                };
                let db = Surreal::new::<SurrealKv>(path.join("surrealdb"))
                    .sync(sync_mode)
                    .await?;
                db.use_ns("bench").use_db("bench").await?;
                db.query("DEFINE INDEX IF NOT EXISTS idx_group ON TABLE item FIELDS bucket")
                    .await?
                    .check()?;
                Ok(Self::Surreal(db))
            }
            EngineKind::Turso => {
                let turso_path = path.join("turso.db");
                let turso_path = turso_path
                    .to_str()
                    .context("Turso database path is not valid UTF-8")?;
                let db = turso::Builder::new_local(turso_path).build().await?;
                let conn = db.connect()?;
                let sync = match durability {
                    Durability::Relaxed => "NORMAL",
                    Durability::Sync => "FULL",
                };
                conn.execute(&format!("PRAGMA synchronous={sync}"), ())
                    .await?;
                conn.execute("CREATE TABLE IF NOT EXISTS item (id INTEGER PRIMARY KEY, grp INTEGER NOT NULL, payload TEXT NOT NULL)", ()).await?;
                conn.execute("CREATE INDEX IF NOT EXISTS idx_group ON item(grp)", ())
                    .await?;
                Ok(Self::Turso(conn))
            }
            EngineKind::Sqlite => {
                if rusqlite::version() != "3.53.4" {
                    bail!(
                        "SQLite runtime version mismatch: expected 3.53.4, got {}",
                        rusqlite::version()
                    );
                }
                let conn = SqliteConnection::open(path.join("sqlite.db"))?;
                conn.pragma_update(None, "journal_mode", "WAL")?;
                let sync = match durability {
                    Durability::Relaxed => "NORMAL",
                    Durability::Sync => "FULL",
                };
                conn.pragma_update(None, "synchronous", sync)?;
                conn.execute_batch(
                    "CREATE TABLE IF NOT EXISTS item (
                        id INTEGER PRIMARY KEY,
                        grp INTEGER NOT NULL,
                        payload TEXT NOT NULL
                     );
                     CREATE INDEX IF NOT EXISTS idx_group ON item(grp);",
                )?;
                Ok(Self::Sqlite(conn))
            }
        }
    }

    pub(crate) async fn get(&self, id: u64) -> Result<Option<ReadDigest>> {
        match self {
            Self::Surreal(db) => {
                let mut response = db
                    .query("SELECT bucket, payload FROM ONLY $id")
                    .bind(("id", RecordId::new("item", id as i64)))
                    .await?;
                let row: Option<RecordData> = response.take(0)?;
                Ok(row.map(|record| ReadDigest::new(record.bucket, record.payload.len())))
            }
            Self::Turso(conn) => {
                let mut stmt = conn
                    .prepare("SELECT grp, payload FROM item WHERE id=?1")
                    .await?;
                let mut rows = stmt.query((id as i64,)).await?;
                if let Some(row) = rows.next().await? {
                    let bucket: i64 = row.get(0)?;
                    let bucket = u32::try_from(bucket).context("Turso grp outside u32 range")?;
                    let payload: String = row.get(1)?;
                    Ok(Some(ReadDigest::new(bucket, payload.len())))
                } else {
                    Ok(None)
                }
            }
            Self::Sqlite(conn) => {
                let row: Option<(i64, String)> = conn
                    .query_row(
                        "SELECT grp, payload FROM item WHERE id=?1",
                        [id as i64],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                row.map(|(bucket, payload)| {
                    let bucket = u32::try_from(bucket).context("SQLite grp outside u32 range")?;
                    Ok(ReadDigest::new(bucket, payload.len()))
                })
                .transpose()
            }
        }
    }

    async fn sql_cache_pragma_value(&self) -> Result<Option<i64>> {
        match self {
            Self::Surreal(_) => Ok(None),
            Self::Turso(conn) => {
                let mut stmt = conn.prepare("PRAGMA cache_size").await?;
                let mut rows = stmt.query(()).await?;
                let row = rows
                    .next()
                    .await?
                    .context("Turso PRAGMA cache_size returned no row")?;
                Ok(Some(row.get(0)?))
            }
            Self::Sqlite(conn) => Ok(Some(
                conn.query_row("PRAGMA cache_size", [], |row| row.get(0))?,
            )),
        }
    }

    async fn set_sql_cache_kib(&self, cache_kib: Option<u32>) -> Result<()> {
        let Some(cache_kib) = cache_kib else {
            return Ok(());
        };
        match self {
            Self::Surreal(_) => bail!("--sql-cache-kib is only valid for sqlite/turso diagnostics"),
            Self::Turso(conn) => {
                conn.execute(&format!("PRAGMA cache_size=-{cache_kib}"), ())
                    .await?;
            }
            Self::Sqlite(conn) => {
                conn.pragma_update(None, "cache_size", -i64::from(cache_kib))?;
            }
        }
        Ok(())
    }

    pub(crate) async fn indexed_read(&self, group: u32) -> Result<IndexedRead> {
        self.indexed_read_with_limit(group, 100).await
    }

    async fn indexed_read_with_limit(&self, group: u32, limit: usize) -> Result<IndexedRead> {
        match self {
            Self::Surreal(db) => {
                let mut resp = db
                    .query("SELECT bucket, payload FROM item WHERE bucket = $bucket LIMIT $limit")
                    .bind(("bucket", group))
                    .bind(("limit", limit as i64))
                    .await?;
                let rows: Vec<RecordData> = resp.take(0)?;
                let checksum = rows.iter().fold(0u64, |acc, row| {
                    acc.wrapping_add(ReadDigest::new(row.bucket, row.payload.len()).checksum())
                });
                Ok(IndexedRead {
                    rows: rows.len(),
                    checksum,
                })
            }
            Self::Turso(conn) => {
                let mut stmt = conn
                    .prepare("SELECT grp, payload FROM item WHERE grp=?1 LIMIT ?2")
                    .await?;
                let mut rows = stmt.query((group as i64, limit as i64)).await?;
                let mut n = 0usize;
                let mut checksum = 0u64;
                while let Some(row) = rows.next().await? {
                    let bucket: i64 = row.get(0)?;
                    let bucket = u32::try_from(bucket).context("Turso grp outside u32 range")?;
                    let payload: String = row.get(1)?;
                    checksum =
                        checksum.wrapping_add(ReadDigest::new(bucket, payload.len()).checksum());
                    n += 1;
                }
                Ok(IndexedRead { rows: n, checksum })
            }
            Self::Sqlite(conn) => {
                let mut stmt =
                    conn.prepare("SELECT grp, payload FROM item WHERE grp=?1 LIMIT ?2")?;
                let mut rows = stmt.query(params![group as i64, limit as i64])?;
                let mut n = 0usize;
                let mut checksum = 0u64;
                while let Some(row) = rows.next()? {
                    let bucket: i64 = row.get(0)?;
                    let bucket = u32::try_from(bucket).context("SQLite grp outside u32 range")?;
                    let payload: String = row.get(1)?;
                    checksum =
                        checksum.wrapping_add(ReadDigest::new(bucket, payload.len()).checksum());
                    n += 1;
                }
                Ok(IndexedRead { rows: n, checksum })
            }
        }
    }

    pub(crate) async fn upsert_one(&self, id: u64, data: &RecordData) -> Result<()> {
        match self {
            Self::Surreal(db) => {
                db.query("UPSERT $id CONTENT $data RETURN NONE")
                    .bind(("id", RecordId::new("item", id as i64)))
                    .bind(("data", data.clone()))
                    .await?
                    .check()?;
            }
            Self::Turso(conn) => {
                let mut stmt = conn.prepare(
                    "INSERT INTO item(id, grp, payload) VALUES(?1, ?2, ?3) ON CONFLICT(id) DO UPDATE SET grp=excluded.grp, payload=excluded.payload"
                ).await?;
                stmt.execute((id as i64, data.bucket as i64, data.payload.as_str()))
                    .await?;
            }
            Self::Sqlite(conn) => {
                conn.execute(
                    "INSERT INTO item(id, grp, payload) VALUES(?1, ?2, ?3)
                     ON CONFLICT(id) DO UPDATE SET grp=excluded.grp, payload=excluded.payload",
                    params![id as i64, data.bucket as i64, &data.payload],
                )?;
            }
        }
        Ok(())
    }

    pub(crate) async fn write_batch(
        &self,
        rows: &[(u64, RecordData)],
        deletes: &[u64],
    ) -> Result<()> {
        match self {
            Self::Surreal(db) => {
                let mut sql = String::from("BEGIN TRANSACTION;\n");
                for index in 0..rows.len() {
                    sql.push_str(&format!(
                        "UPSERT $id_{index} CONTENT $data_{index} RETURN NONE;\n"
                    ));
                }
                for index in 0..deletes.len() {
                    sql.push_str(&format!("DELETE $delete_{index} RETURN NONE;\n"));
                }
                sql.push_str("COMMIT TRANSACTION;");
                let mut query = db.query(sql);
                for (index, (id, data)) in rows.iter().enumerate() {
                    query = query
                        .bind((format!("id_{index}"), RecordId::new("item", *id as i64)))
                        .bind((format!("data_{index}"), data.clone()));
                }
                for (index, id) in deletes.iter().enumerate() {
                    query =
                        query.bind((format!("delete_{index}"), RecordId::new("item", *id as i64)));
                }
                query.await?.check()?;
            }
            Self::Turso(conn) => {
                conn.execute("BEGIN IMMEDIATE TRANSACTION", ()).await?;
                let result: Result<()> = async {
                    if !rows.is_empty() {
                        let mut stmt = conn
                            .prepare(
                                "INSERT INTO item(id, grp, payload) VALUES(?1, ?2, ?3) ON CONFLICT(id) DO UPDATE SET grp=excluded.grp, payload=excluded.payload",
                            )
                            .await?;
                        for (id, data) in rows {
                            stmt.execute((*id as i64, data.bucket as i64, data.payload.as_str()))
                                .await?;
                        }
                    }
                    if !deletes.is_empty() {
                        let mut stmt = conn.prepare("DELETE FROM item WHERE id=?1").await?;
                        for id in deletes {
                            stmt.execute((*id as i64,)).await?;
                        }
                    }
                    Ok(())
                }
                .await;
                if result.is_ok() {
                    conn.execute("COMMIT", ()).await?;
                } else {
                    let _ = conn.execute("ROLLBACK", ()).await;
                    result?;
                }
            }
            Self::Sqlite(conn) => {
                conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
                let result: Result<()> = (|| {
                    if !rows.is_empty() {
                        let mut stmt = conn.prepare(
                            "INSERT INTO item(id, grp, payload) VALUES(?1, ?2, ?3)
                             ON CONFLICT(id) DO UPDATE SET grp=excluded.grp, payload=excluded.payload",
                        )?;
                        for (id, data) in rows {
                            stmt.execute(params![*id as i64, data.bucket as i64, &data.payload])?;
                        }
                    }
                    if !deletes.is_empty() {
                        let mut stmt = conn.prepare("DELETE FROM item WHERE id=?1")?;
                        for id in deletes {
                            stmt.execute(params![*id as i64])?;
                        }
                    }
                    Ok(())
                })();
                if result.is_ok() {
                    conn.execute_batch("COMMIT")?;
                } else {
                    let _ = conn.execute_batch("ROLLBACK");
                    result?;
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn upsert_batch(&self, rows: &[(u64, RecordData)]) -> Result<()> {
        self.write_batch(rows, &[]).await
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
                let read = engine
                    .indexed_read_with_limit(group, args.indexed_read_limit)
                    .await?;
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
                if let Some(progress_file) = &args.progress_file {
                    fs::write(progress_file, format!("{done}\n"))
                        .context("write record recovery progress marker")?;
                }
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
                if let Some(progress_file) = &args.progress_file {
                    fs::write(progress_file, format!("{done}\n"))
                        .context("write record recovery progress marker")?;
                }
            }
        }
    }
    Ok(done)
}

async fn verify_recovery(
    engine: &Engine,
    args: &Args,
    expected_prefix_records: u64,
    tail_records: u64,
) -> Result<Verification> {
    let started = Instant::now();
    let mut missing_prefix_records = 0u64;
    let mut prefix_contiguous_present = 0u64;
    let mut prefix_present_after_gap = 0u64;
    let mut gap_seen = false;
    for id in 0..expected_prefix_records {
        let present = engine.get(id).await?.is_some();
        if present {
            if gap_seen {
                prefix_present_after_gap += 1;
            } else {
                prefix_contiguous_present += 1;
            }
        } else {
            missing_prefix_records += 1;
            gap_seen = true;
        }
    }

    let mut tail_prefix_present = 0u64;
    let mut tail_total_present = 0u64;
    let mut tail_present_after_gap = 0u64;
    gap_seen = false;
    for id in expected_prefix_records..expected_prefix_records.saturating_add(tail_records) {
        let present = engine.get(id).await?.is_some();
        if present {
            tail_total_present += 1;
            if gap_seen {
                tail_present_after_gap += 1;
            } else {
                tail_prefix_present += 1;
            }
        } else {
            gap_seen = true;
        }
    }

    let txn = args.txn_size.max(1) as u64;
    let transaction_atomic_tail =
        tail_prefix_present == 0 || tail_prefix_present.is_multiple_of(txn);
    let verification_ok = missing_prefix_records == 0
        && prefix_present_after_gap == 0
        && tail_present_after_gap == 0
        && transaction_atomic_tail;

    Ok(Verification {
        expected_prefix_records,
        checked_prefix_records: expected_prefix_records,
        missing_prefix_records,
        prefix_contiguous_present,
        prefix_present_after_gap,
        tail_checked_records: tail_records,
        tail_prefix_present,
        tail_total_present,
        tail_present_after_gap,
        transaction_atomic_tail,
        verification_ok,
        verify_s: started.elapsed().as_secs_f64(),
    })
}

#[tokio::main(flavor = "multi_thread", worker_threads = 1)]
async fn main() -> Result<()> {
    let args = Args::parse();
    if args.records == 0 {
        bail!("--records must be greater than zero");
    }
    if args.indexed_read_limit == 0 {
        bail!("--indexed-read-limit must be greater than zero");
    }
    if args.sql_cache_kib == Some(0) {
        bail!("--sql-cache-kib must be greater than zero");
    }
    if args.progress_file.is_some()
        && !matches!(args.workload, Workload::TinyTxn | Workload::WriteBurst)
    {
        bail!("--progress-file requires tiny-txn or write-burst");
    }

    let run_name = format!(
        "{:?}-{:?}-{:?}-n{}-p{}-tx{}-il{}-ck{}-trial{}",
        args.engine,
        args.durability,
        args.workload,
        args.records,
        args.payload_bytes,
        args.txn_size,
        args.indexed_read_limit,
        args.sql_cache_kib
            .map_or_else(|| "default".to_string(), |value| value.to_string()),
        args.trial
    )
    .to_lowercase()
    .replace('_', "-");
    let path = if let Some(name) = &args.db_name {
        let candidate = Path::new(name);
        if candidate.components().count() != 1 || name == "." || name == ".." {
            bail!("--db-name must be one plain path component");
        }
        args.root.join(candidate)
    } else {
        args.root.join(run_name)
    };
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
    engine.set_sql_cache_kib(args.sql_cache_kib).await?;
    let sql_cache_pragma_value = engine.sql_cache_pragma_value().await?;
    if let Some(cache_kib) = args.sql_cache_kib {
        let expected = -i64::from(cache_kib);
        if sql_cache_pragma_value != Some(expected) {
            bail!(
                "SQL cache PRAGMA did not round-trip: requested {expected}, got {sql_cache_pragma_value:?}"
            );
        }
    }
    let open_s = open_started.elapsed().as_secs_f64();
    let prefill_s = if args.skip_prefill {
        0.0
    } else {
        let prefill_started = Instant::now();
        prefill(&engine, &args).await?;
        prefill_started.elapsed().as_secs_f64()
    };
    let verification = if let Some(expected_prefix_records) = args.verify_prefix_records {
        Some(
            verify_recovery(
                &engine,
                &args,
                expected_prefix_records,
                args.verify_tail_records,
            )
            .await?,
        )
    } else {
        None
    };

    let warmup_started = Instant::now();
    for id in 0..args.records.min(args.warmup_reads) {
        std::hint::black_box(engine.get(id).await?);
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
        format_version: 8,
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
        indexed_read_limit: args.indexed_read_limit,
        sql_cache_kib: args.sql_cache_kib,
        sql_cache_pragma_value,
        read_materialization: READ_MATERIALIZATION,
        write_materialization: WRITE_MATERIALIZATION,
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
        verification,
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
