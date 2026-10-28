use all_db_bench_record_harness::{Durability, IndexedRead, ReadDigest, RecordData};
use anyhow::Result;
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::path::Path;
use surrealdb::{
    Surreal,
    engine::local::{Db as SurrealLocalDb, RocksDb},
    types::{RecordId, SurrealValue},
};

#[derive(Clone, Copy, Debug, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum EngineKind {
    SurrealdbRocksdb,
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

pub struct Engine(Surreal<SurrealLocalDb>);

impl EngineKind {
    fn version(self) -> &'static str {
        "SurrealDB 3.3.4 / surrealdb-kvs-rocksdb 3.3.4"
    }

    fn mapping(self, d: Durability) -> &'static str {
        match d {
            Durability::Relaxed => "SurrealDB embedded RocksDB sync=never",
            Durability::Sync => "SurrealDB embedded RocksDB sync=every",
        }
    }
}

impl Engine {
    async fn open(kind: EngineKind, durability: Durability, path: &Path) -> Result<Self> {
        let EngineKind::SurrealdbRocksdb = kind;
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
        Ok(Self(db))
    }

    async fn get(&self, id: u64) -> Result<Option<ReadDigest>> {
        let mut response = self
            .0
            .query("SELECT bucket, payload FROM ONLY $id")
            .bind(("id", RecordId::new("item", id as i64)))
            .await?;
        let row: Option<SurrealRecord> = response.take(0)?;
        Ok(row.map(|record| ReadDigest::new(record.bucket, record.payload.len())))
    }

    async fn indexed_read(&self, group: u32) -> Result<IndexedRead> {
        let mut response = self
            .0
            .query("SELECT bucket, payload FROM item WHERE bucket = $bucket LIMIT 100")
            .bind(("bucket", group))
            .await?;
        let rows: Vec<SurrealRecord> = response.take(0)?;
        let checksum = rows.iter().fold(0u64, |acc, row| {
            acc.wrapping_add(ReadDigest::new(row.bucket, row.payload.len()).checksum())
        });
        Ok(IndexedRead {
            rows: rows.len(),
            checksum,
        })
    }

    async fn upsert_one(&self, id: u64, data: &RecordData) -> Result<()> {
        self.0
            .query("UPSERT $id CONTENT $data RETURN NONE")
            .bind(("id", RecordId::new("item", id as i64)))
            .bind(("data", SurrealRecord::from(data)))
            .await?
            .check()?;
        Ok(())
    }

    async fn write_batch(&self, rows: &[(u64, RecordData)], deletes: &[u64]) -> Result<()> {
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
        let mut query = self.0.query(sql);
        for (index, (id, data)) in rows.iter().enumerate() {
            query = query
                .bind((format!("id_{index}"), RecordId::new("item", *id as i64)))
                .bind((format!("data_{index}"), SurrealRecord::from(data)));
        }
        for (index, id) in deletes.iter().enumerate() {
            query = query.bind((format!("delete_{index}"), RecordId::new("item", *id as i64)));
        }
        query.await?.check()?;
        Ok(())
    }

    async fn upsert_batch(&self, rows: &[(u64, RecordData)]) -> Result<()> {
        self.write_batch(rows, &[]).await
    }
}

impl all_db_bench_record_harness::backend::RecordBackend for Engine {
    type Kind = EngineKind;

    fn version(kind: Self::Kind) -> &'static str {
        kind.version()
    }

    fn mapping(kind: Self::Kind, durability: Durability) -> &'static str {
        kind.mapping(durability)
    }

    fn is_transaction_conflict(error: &anyhow::Error) -> bool {
        error
            .downcast_ref::<surrealdb::Error>()
            .is_some_and(|error| {
                matches!(
                    error.query_details(),
                    Some(surrealdb::types::QueryError::TransactionConflict)
                )
            })
    }

    fn open(
        kind: Self::Kind,
        durability: Durability,
        path: &Path,
    ) -> impl std::future::Future<Output = Result<Self>> {
        Engine::open(kind, durability, path)
    }

    fn get(&self, id: u64) -> impl std::future::Future<Output = Result<Option<ReadDigest>>> {
        Engine::get(self, id)
    }

    fn indexed_read(&self, group: u32) -> impl std::future::Future<Output = Result<IndexedRead>> {
        Engine::indexed_read(self, group)
    }

    fn upsert_one(
        &self,
        id: u64,
        data: &RecordData,
    ) -> impl std::future::Future<Output = Result<()>> {
        Engine::upsert_one(self, id, data)
    }

    fn upsert_batch(
        &self,
        rows: &[(u64, RecordData)],
    ) -> impl std::future::Future<Output = Result<()>> {
        Engine::upsert_batch(self, rows)
    }

    fn write_batch(
        &self,
        rows: &[(u64, RecordData)],
        deletes: &[u64],
    ) -> impl std::future::Future<Output = Result<()>> {
        Engine::write_batch(self, rows, deletes)
    }

    fn make_clients(
        &self,
        clients: usize,
        _path: &Path,
        _durability: Durability,
    ) -> impl std::future::Future<Output = Result<(Vec<Self>, &'static str)>> {
        let db = &self.0;
        std::future::ready(Ok((
            (0..clients).map(|_| Engine(db.clone())).collect(),
            "native cloned SurrealDB/RocksDB client handles",
        )))
    }
}
