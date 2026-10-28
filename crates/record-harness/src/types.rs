use clap::ValueEnum;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Durability {
    Relaxed,
    Sync,
}

#[derive(Clone, Copy, Debug, Serialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Workload {
    PointRead,
    IndexedRead,
    ReadHeavy,
    TinyTxn,
    WriteBurst,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordData {
    pub bucket: u32,
    pub payload: String,
}

pub const READ_MATERIALIZATION: &str = "full-record-v1";
pub const WRITE_MATERIALIZATION: &str = "no-return-v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadDigest {
    bucket: u32,
    payload_len: usize,
}

impl ReadDigest {
    #[must_use]
    pub fn new(bucket: u32, payload_len: usize) -> Self {
        Self {
            bucket,
            payload_len,
        }
    }

    #[must_use]
    pub fn checksum(self) -> u64 {
        (u64::from(self.bucket) << 32) ^ self.payload_len as u64
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexedRead {
    pub rows: usize,
    pub checksum: u64,
}
