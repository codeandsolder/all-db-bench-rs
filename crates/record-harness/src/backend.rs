use anyhow::Result;
use serde::Serialize;
use std::{fmt::Debug, future::Future, path::Path};

use crate::{Durability, IndexedRead, ReadDigest, RecordData};

pub trait RecordBackend: Sized + Send + 'static {
    type Kind: Copy + Debug + Serialize + Send + Sync + 'static;

    fn version(kind: Self::Kind) -> &'static str;
    fn mapping(kind: Self::Kind, durability: Durability) -> &'static str;
    fn is_transaction_conflict(error: &anyhow::Error) -> bool;

    fn open(
        kind: Self::Kind,
        durability: Durability,
        path: &Path,
    ) -> impl Future<Output = Result<Self>>;

    fn get(&self, id: u64) -> impl Future<Output = Result<Option<ReadDigest>>>;
    fn indexed_read(&self, group: u32) -> impl Future<Output = Result<IndexedRead>>;
    fn upsert_one(&self, id: u64, data: &RecordData) -> impl Future<Output = Result<()>>;
    fn upsert_batch(&self, rows: &[(u64, RecordData)]) -> impl Future<Output = Result<()>>;
    fn write_batch(
        &self,
        rows: &[(u64, RecordData)],
        deletes: &[u64],
    ) -> impl Future<Output = Result<()>>;
    fn make_clients(
        &self,
        clients: usize,
        path: &Path,
        durability: Durability,
    ) -> impl Future<Output = Result<(Vec<Self>, &'static str)>>;
}
