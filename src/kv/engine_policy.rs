use anyhow::{Result, bail};

use super::types::{Durability, EngineCompression, EngineKind};

impl EngineKind {
    pub(crate) fn version(self) -> &'static str {
        match self {
            Self::Redb => "4.4.0",
            Self::Fjall => "3.1.12",
            Self::Surrealkv => "1.0.0",
            Self::Heed => "0.22.1",
            Self::Sled => "1.0.0-alpha.124",
            Self::Lkv => "0.2.1",
            Self::Manifold | Self::ManifoldWal => "3.1.0",
            Self::Turbokv => "0.6.0",
            Self::ParitydbHash | Self::ParitydbBtree => "0.5.7",
            #[cfg(feature = "kv-external")]
            Self::Rocksdb => "0.25.0",
            #[cfg(feature = "kv-external")]
            Self::Mdbx => "0.9.0",
            #[cfg(feature = "kv-external")]
            Self::Persy => "1.8.1",
            #[cfg(feature = "kv-experimental")]
            Self::Roughdb => "0.11.1",
            #[cfg(feature = "kv-experimental")]
            Self::Jammdb => "0.11.0",
            #[cfg(feature = "kv-experimental")]
            Self::Lsmdb => "1.1.0",
        }
    }

    pub(crate) fn durability_mapping(self, d: Durability) -> Result<&'static str> {
        Ok(match (self, d) {
            (Self::Redb, Durability::Relaxed) => "redb Durability::None",
            (Self::Redb, Durability::Sync) => "redb Durability::Immediate",
            (Self::Fjall, Durability::Relaxed) => "fjall commit + PersistMode::Buffer",
            (Self::Fjall, Durability::Sync) => "fjall commit + PersistMode::SyncAll",
            (Self::Surrealkv, Durability::Relaxed) => "surrealkv Durability::Eventual",
            (Self::Surrealkv, Durability::Sync) => "surrealkv Durability::Immediate",
            (Self::Heed, Durability::Relaxed) => "LMDB MDB_NOSYNC via heed EnvFlags::NO_SYNC",
            (Self::Heed, Durability::Sync) => "LMDB default synchronous commit",
            (Self::Sled, Durability::Relaxed) => "sled apply_batch without explicit flush",
            (Self::Sled, Durability::Sync) => "sled apply_batch + Db::flush before acknowledgement",
            (Self::Lkv, Durability::Sync) => "lkv commit; sync_data before publication",
            (Self::Lkv, Durability::Relaxed) => bail!("lkv 0.2.1 has no relaxed commit mode"),
            (Self::Manifold, Durability::Sync) => {
                "Manifold column-family without WAL + Durability::Immediate; direct durable commit"
            }
            (Self::Manifold, Durability::Relaxed) => {
                bail!("Manifold no-WAL comparison is sync-only")
            }
            (Self::ManifoldWal, Durability::Sync) => {
                "Manifold default column-family WAL + Durability::Immediate; diagnostic configuration (3.1.0 fails abrupt-process recovery)"
            }
            (Self::ManifoldWal, Durability::Relaxed) => bail!(
                "Manifold WAL path fsyncs even non-durable transaction records; relaxed lane intentionally omitted"
            ),
            (Self::Turbokv, Durability::Relaxed) => {
                "TurboKV DbOptions::durable; WAL without per-write fsync"
            }
            (Self::Turbokv, Durability::Sync) => {
                "TurboKV DbOptions::paranoid; sync WAL before acknowledgement"
            }
            (Self::ParitydbHash | Self::ParitydbBtree, Durability::Relaxed) => {
                "ParityDB default background durability pipeline; commit publishes to the in-memory overlay before WAL/data fsync completes"
            }
            (Self::ParitydbHash | Self::ParitydbBtree, Durability::Sync) => {
                bail!("ParityDB 0.5.7 has no public durable-before-return commit API")
            }
            #[cfg(feature = "kv-external")]
            (Self::Rocksdb, Durability::Relaxed) => {
                "RocksDB WAL enabled; WriteOptions::set_sync(false)"
            }
            #[cfg(feature = "kv-external")]
            (Self::Rocksdb, Durability::Sync) => {
                "RocksDB WAL enabled; WriteOptions::set_sync(true)"
            }
            #[cfg(feature = "kv-external")]
            (Self::Mdbx, Durability::Relaxed) => {
                "libmdbx NoWriteMap + SyncMode::SafeNoSync; crash rolls back to last steady commit"
            }
            #[cfg(feature = "kv-external")]
            (Self::Mdbx, Durability::Sync) => "libmdbx NoWriteMap + SyncMode::Durable",
            #[cfg(feature = "kv-external")]
            (Self::Persy, Durability::Relaxed) => {
                "Persy TransactionConfig background_sync=true; fsync queued after acknowledgement"
            }
            #[cfg(feature = "kv-external")]
            (Self::Persy, Durability::Sync) => {
                "Persy foreground transaction sync; file sync_all before commit completes"
            }
            #[cfg(feature = "kv-experimental")]
            (Self::Roughdb, Durability::Relaxed) => "RoughDB WAL + WriteOptions { sync: false }",
            #[cfg(feature = "kv-experimental")]
            (Self::Roughdb, Durability::Sync) => "RoughDB WAL + WriteOptions { sync: true }",
            #[cfg(feature = "kv-experimental")]
            (Self::Jammdb, Durability::Sync) => {
                "jammdb writable transaction commit; file sync_all before publication"
            }
            #[cfg(feature = "kv-experimental")]
            (Self::Jammdb, Durability::Relaxed) => {
                bail!("jammdb 0.11.0 exposes no relaxed commit mode")
            }
            #[cfg(feature = "kv-experimental")]
            (Self::Lsmdb, Durability::Sync) => {
                "lsm-db 1.1.0 durability feature; wal-db durable log before acknowledgement"
            }
            #[cfg(feature = "kv-experimental")]
            (Self::Lsmdb, Durability::Relaxed) => {
                bail!("lsm-db durability is compile-time; this adapter is the durable build")
            }
        })
    }
}

pub(crate) fn compression_override_supported(kind: EngineKind) -> bool {
    match kind {
        EngineKind::Fjall | EngineKind::Surrealkv | EngineKind::Turbokv => true,
        #[cfg(feature = "kv-external")]
        EngineKind::Rocksdb => true,
        _ => false,
    }
}

pub(crate) fn compression_mapping(
    kind: EngineKind,
    compression: EngineCompression,
) -> Result<&'static str> {
    Ok(match (kind, compression) {
        (EngineKind::Fjall, EngineCompression::Default) => {
            "Fjall default: L0/L1 data blocks uncompressed, L2+ LZ4; journal default unchanged"
        }
        (EngineKind::Fjall, EngineCompression::None) => {
            "Fjall data blocks uncompressed; journal compression disabled"
        }
        (EngineKind::Fjall, EngineCompression::Lz4) => {
            "Fjall data blocks LZ4 at every level; journal LZ4"
        }
        (EngineKind::Fjall, EngineCompression::Snappy | EngineCompression::Zstd) => {
            bail!("Fjall 3.1.12 exposes LZ4 but not the requested codec")
        }
        (EngineKind::Surrealkv, EngineCompression::Default | EngineCompression::None) => {
            "SurrealKV SSTable compression disabled; WAL compression unchanged"
        }
        (EngineKind::Surrealkv, EngineCompression::Snappy) => {
            "SurrealKV SSTable blocks Snappy at every level; WAL compression unchanged"
        }
        (EngineKind::Surrealkv, EngineCompression::Zstd) => {
            "SurrealKV SSTable blocks Zstd at every level; WAL compression unchanged"
        }
        (EngineKind::Surrealkv, EngineCompression::Lz4) => {
            bail!("SurrealKV 1.0.0 SSTables do not expose LZ4 compression")
        }
        (EngineKind::Turbokv, EngineCompression::Default | EngineCompression::Lz4) => {
            "TurboKV SSTable compression LZ4; WAL compression unchanged"
        }
        (EngineKind::Turbokv, EngineCompression::None) => {
            "TurboKV SSTable compression disabled; WAL compression unchanged"
        }
        (EngineKind::Turbokv, EngineCompression::Snappy) => {
            "TurboKV SSTable compression Snappy; WAL compression unchanged"
        }
        (EngineKind::Turbokv, EngineCompression::Zstd) => {
            "TurboKV SSTable compression Zstd; WAL compression unchanged"
        }
        #[cfg(feature = "kv-external")]
        (EngineKind::Rocksdb, EngineCompression::Default | EngineCompression::None) => {
            "RocksDB benchmark SSTable compression disabled; WAL compression unchanged"
        }
        #[cfg(feature = "kv-external")]
        (EngineKind::Rocksdb, EngineCompression::Lz4) => {
            "RocksDB SSTable compression LZ4; WAL compression unchanged"
        }
        #[cfg(feature = "kv-external")]
        (EngineKind::Rocksdb, EngineCompression::Snappy) => {
            "RocksDB SSTable compression Snappy; WAL compression unchanged"
        }
        #[cfg(feature = "kv-external")]
        (EngineKind::Rocksdb, EngineCompression::Zstd) => {
            "RocksDB SSTable compression Zstd; WAL compression unchanged"
        }
        (_, EngineCompression::Default) => {
            "engine native/default compression configuration; no benchmark override"
        }
        _ => bail!("engine {kind:?} does not expose the requested benchmark compression override"),
    })
}
