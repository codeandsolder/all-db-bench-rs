pub mod kv;

#[cfg(feature = "record")]
pub mod record;

// The record sustained driver is source-shared with the intentionally
// standalone SurrealDB/RocksDB package. Both crate roots expose the backend
// contract as `crate::base`, so the driver stays backend-agnostic without
// making the benchmark internals part of the public package API.
#[cfg(feature = "record")]
pub(crate) use record::bench as base;
