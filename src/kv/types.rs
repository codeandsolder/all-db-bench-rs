use clap::ValueEnum;
use serde::Serialize;

#[derive(Clone, Copy, Debug, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum EngineKind {
    Redb,
    Fjall,
    Surrealkv,
    Heed,
    Sled,
    Lkv,
    Manifold,
    ManifoldWal,
    Turbokv,
    ParitydbHash,
    ParitydbBtree,
    #[cfg(feature = "kv-external")]
    Rocksdb,
    #[cfg(feature = "kv-external")]
    Mdbx,
    #[cfg(feature = "kv-external")]
    Persy,
    #[cfg(feature = "kv-experimental")]
    Roughdb,
    #[cfg(feature = "kv-experimental")]
    Jammdb,
    #[cfg(feature = "kv-experimental")]
    Lsmdb,
}

impl EngineKind {
    #[must_use]
    pub(crate) fn supports_range_scan(self) -> bool {
        !matches!(self, Self::Lkv | Self::ParitydbHash)
    }
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
    RangeScan,
    ReadHeavy,
    Balanced,
    TinyTxn,
    WriteBurst,
    DeleteBurst,
    Churn,
}

#[derive(Clone, Copy, Debug, Serialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum AccessPattern {
    Auto,
    Uniform,
    Hot80,
    Hot95,
}

#[derive(Clone, Copy, Debug, Serialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum KeyShape {
    Sequential,
    SharedPrefix,
    Hashed,
}

#[derive(Clone, Copy, Debug, Serialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ValuePattern {
    PseudoRandom,
    Zeros,
    Repeated,
    StructuredBinary,
    StructuredText,
}

#[derive(Clone, Copy, Debug, Serialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum EngineCompression {
    Default,
    None,
    Lz4,
    Snappy,
    Zstd,
}

#[derive(Clone, Copy, Debug, Serialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum WritePattern {
    Append,
    UpdateUniform,
    UpdateHot,
}
