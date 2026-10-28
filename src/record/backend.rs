use super::engine::{Engine, EngineKind};
use all_db_bench_record_harness::{
    Durability, IndexedRead, ReadDigest, RecordData, backend::RecordBackend,
};
use anyhow::{Context, Result};
use rusqlite::Connection as SqliteConnection;
use std::{path::Path, time::Duration};
use surrealdb::{Error as SurrealError, types::QueryError};

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
    conn.busy_timeout(Duration::from_secs(60))?;
    Ok(conn)
}

impl RecordBackend for Engine {
    type Kind = EngineKind;

    fn version(kind: Self::Kind) -> &'static str {
        kind.version()
    }

    fn mapping(kind: Self::Kind, durability: Durability) -> &'static str {
        kind.mapping(durability)
    }

    fn is_transaction_conflict(error: &anyhow::Error) -> bool {
        error.downcast_ref::<SurrealError>().is_some_and(|error| {
            matches!(error.query_details(), Some(QueryError::TransactionConflict))
        })
    }

    fn open(
        kind: Self::Kind,
        durability: Durability,
        path: &Path,
    ) -> impl Future<Output = Result<Self>> {
        Engine::open(kind, durability, path)
    }

    fn get(&self, id: u64) -> impl Future<Output = Result<Option<ReadDigest>>> {
        Engine::get(self, id)
    }

    fn indexed_read(&self, group: u32) -> impl Future<Output = Result<IndexedRead>> {
        Engine::indexed_read(self, group)
    }

    fn upsert_one(&self, id: u64, data: &RecordData) -> impl Future<Output = Result<()>> {
        Engine::upsert_one(self, id, data)
    }

    fn upsert_batch(&self, rows: &[(u64, RecordData)]) -> impl Future<Output = Result<()>> {
        Engine::upsert_batch(self, rows)
    }

    fn write_batch(
        &self,
        rows: &[(u64, RecordData)],
        deletes: &[u64],
    ) -> impl Future<Output = Result<()>> {
        Engine::write_batch(self, rows, deletes)
    }

    async fn make_clients(
        &self,
        clients: usize,
        path: &Path,
        durability: Durability,
    ) -> Result<(Vec<Self>, &'static str)> {
        match self {
            Engine::Surreal(db) => Ok((
                (0..clients).map(|_| Engine::Surreal(db.clone())).collect(),
                "native Surreal client clones",
            )),
            Engine::Turso(_) => {
                let db_path = path.join("turso.db");
                let db_path = db_path
                    .to_str()
                    .context("Turso database path is not valid UTF-8")?;
                let db = turso::Builder::new_local(db_path).build().await?;
                let mut handles = Vec::with_capacity(clients);
                for _ in 0..clients {
                    let conn = db.connect()?;
                    conn.busy_timeout(Duration::from_secs(60))?;
                    conn.execute(
                        &format!(
                            "PRAGMA synchronous={}",
                            match durability {
                                Durability::Relaxed => "NORMAL",
                                Durability::Sync => "FULL",
                            }
                        ),
                        (),
                    )
                    .await?;
                    handles.push(Engine::Turso(conn));
                }
                Ok((handles, "independent Turso Database::connect() connections"))
            }
            Engine::Sqlite(_) => {
                let handles = (0..clients)
                    .map(|_| sqlite_client(path, durability).map(Engine::Sqlite))
                    .collect::<Result<Vec<_>>>()?;
                Ok((
                    handles,
                    "independent SQLite WAL connections; 60s busy timeout",
                ))
            }
        }
    }
}
