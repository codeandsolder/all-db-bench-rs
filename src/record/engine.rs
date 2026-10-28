use all_db_bench_record_harness::{Durability, IndexedRead, ReadDigest, RecordData};
use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use rusqlite::{Connection as SqliteConnection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::path::Path;
use surrealdb::{
    Surreal,
    engine::local::{Db as SurrealLocalDb, SurrealKv},
    types::{RecordId, SurrealValue},
};

#[derive(Clone, Copy, Debug, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum EngineKind {
    Surrealdb,
    Turso,
    Sqlite,
}

#[derive(Clone, Debug, Serialize, Deserialize, SurrealValue)]
struct SurrealRecord {
    bucket: u32,
    payload: String,
}

impl From<&RecordData> for SurrealRecord {
    fn from(value: &RecordData) -> Self {
        Self {
            bucket: value.bucket,
            payload: value.payload.clone(),
        }
    }
}

pub(crate) enum Engine {
    Surreal(Surreal<SurrealLocalDb>),
    Turso(turso::Connection),
    Sqlite(SqliteConnection),
}

impl EngineKind {
    pub(crate) fn version(self) -> &'static str {
        match self {
            Self::Surrealdb => "3.3.4 / SurrealKV 0.21.4",
            Self::Turso => "0.8.3",
            Self::Sqlite => "rusqlite 0.40.2 / SQLite 3.54.0",
        }
    }
    pub(crate) fn mapping(self, d: Durability) -> &'static str {
        match (self, d) {
            (Self::Surrealdb, Durability::Relaxed) => "SurrealDB embedded SurrealKV sync=never",
            (Self::Surrealdb, Durability::Sync) => "SurrealDB embedded SurrealKV sync=every",
            (Self::Turso, Durability::Relaxed) => "Turso PRAGMA synchronous=NORMAL",
            (Self::Turso, Durability::Sync) => "Turso PRAGMA synchronous=FULL",
            (Self::Sqlite, Durability::Relaxed) => {
                "SQLite 3.54.0 WAL journal_mode + synchronous=NORMAL"
            }
            (Self::Sqlite, Durability::Sync) => "SQLite 3.54.0 WAL journal_mode + synchronous=FULL",
        }
    }
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
                if rusqlite::version() != "3.54.0" {
                    bail!(
                        "SQLite runtime version mismatch: expected 3.54.0, got {}",
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
                let row: Option<SurrealRecord> = response.take(0)?;
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

    pub(crate) async fn indexed_read(&self, group: u32) -> Result<IndexedRead> {
        match self {
            Self::Surreal(db) => {
                let mut resp = db
                    .query("SELECT bucket, payload FROM item WHERE bucket = $bucket LIMIT 100")
                    .bind(("bucket", group))
                    .await?;
                let rows: Vec<SurrealRecord> = resp.take(0)?;
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
                    .prepare("SELECT grp, payload FROM item WHERE grp=?1 LIMIT 100")
                    .await?;
                let mut rows = stmt.query((group as i64,)).await?;
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
                    conn.prepare("SELECT grp, payload FROM item WHERE grp=?1 LIMIT 100")?;
                let mut rows = stmt.query([group as i64])?;
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
                    .bind(("data", SurrealRecord::from(data)))
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
                        .bind((format!("data_{index}"), SurrealRecord::from(data)));
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
