use anyhow::Result;
use serde::Serialize;
use std::{fmt::Debug, future::Future, path::Path};

use crate::{Durability, IndexedRead, ReadDigest, RecordData};

/// One exclusively borrowed client handle per worker. The native connection
/// may be Send without Sync (for example rusqlite).
pub trait RecordBackend: Sized + Send + 'static {
    type Kind: Copy + Debug + Serialize + Send + Sync + 'static;

    fn version(kind: Self::Kind) -> &'static str;
    fn mapping(kind: Self::Kind, durability: Durability) -> &'static str;
    fn is_transaction_conflict(error: &anyhow::Error) -> bool;

    fn open(
        kind: Self::Kind,
        durability: Durability,
        path: &Path,
    ) -> impl Future<Output = Result<Self>> + Send;

    fn get(&mut self, id: u64) -> impl Future<Output = Result<Option<ReadDigest>>> + Send;
    fn indexed_read(&mut self, group: u32) -> impl Future<Output = Result<IndexedRead>> + Send;
    fn upsert_one(&mut self, id: u64, data: &RecordData)
    -> impl Future<Output = Result<()>> + Send;
    fn upsert_batch(
        &mut self,
        rows: &[(u64, RecordData)],
    ) -> impl Future<Output = Result<()>> + Send;
    fn write_batch(
        &mut self,
        rows: &[(u64, RecordData)],
        deletes: &[u64],
    ) -> impl Future<Output = Result<()>> + Send;
    fn make_clients(
        &mut self,
        clients: usize,
        path: &Path,
        durability: Durability,
    ) -> impl Future<Output = Result<(Vec<Self>, &'static str)>> + Send;
}
