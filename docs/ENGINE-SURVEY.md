# Engine survey — 2026-09-27

## In the benchmark

### redb 4.3.0
Pure-Rust copy-on-write B+tree, ACID/MVCC. This remains the main B+tree-style reference.

### Fjall 3.1.12
Pure-Rust LSM engine. Its durability API is explicit: Buffer, SyncData, SyncAll. The old benchmark accidentally measured commit without the required persist call; this suite does not.

### SurrealKV 0.21.4
SurrealDB's low-level Rust storage engine, tested directly in the raw KV lane. This is distinct from SurrealDB itself.

### SurrealDB 3.3.0
Tested in the record/document lane through the embedded SurrealKV backend. SurrealDB 3.3 exposes storage sync modes, so sync=every and sync=never can be selected explicitly.

### TurboKV 0.6.0
Recent async Rust LSM-style embedded store. It explicitly distinguishes fast(), durable() (WAL without per-write sync), and paranoid() (sync before acknowledgement). The benchmark maps the primary power-loss-durable comparison to paranoid(), not to the misleadingly named durable() preset.

### lkv 0.2.1
Recent compact Rust embedded KV. File-backed commits call sync_data before publication. No equivalent relaxed-commit mode is exposed, so it participates in sync results only.

### sled 1.0.0-alpha.124
The sled project has an active 1.0 alpha line again. Included because the implementation has materially moved beyond the long-stagnant 0.34 release commonly used in older comparisons. Sync results explicitly call Db::flush after each measured write transaction.

### heed 0.22.1 / LMDB
Not a pure-Rust storage engine, but still a valuable mmap/B+tree reference with a thin Rust API. Kept as a reference rather than described as a Rust-native engine.

### Manifold 3.1.0

Crate: manifold-db 3.1.0, published 2026-08-28.

Manifold is explicitly a fork of redb with column families and a WAL/group-commit design. The benchmark uses the column-family API rather than its inherited redb-compatible single-database surface: the interesting path writes a WAL entry, waits for group-commit fsync, then makes the prepared changes visible. It participates in the sync lane only because that is the novel path worth comparing on the current one-CPU host; its WAL implementation performs the sync step even when the underlying transaction durability enum is None.


### RocksDB 0.25.0
Mature C++ LSM reference exposed through the current Rust rocksdb crate. Included despite not being Rust-native because it is the most useful production LSM baseline. The neutral raw-KV lane explicitly disables compression while retaining WAL; relaxed/sync differ only in the RocksDB write sync flag.

### libmdbx 0.9.0 / MDBX
Mature mmap/B+tree-family reference beside LMDB/heed. Uses NoWriteMap. Sync maps to MDBX Durable; relaxed maps to SafeNoSync rather than the corruption-prone UtterlyNoSync mode.

### Persy 1.8.1
Pure-Rust transactional single-file copy-on-write/journal engine. The adapter uses a ByteVec -> ByteVec Replace index to provide true KV semantics. Sync uses foreground transaction fsync; relaxed uses Persy's background-sync transaction mode.

### SQLite 3.53.4 / rusqlite 0.40.2
Record/SQL lane baseline. rusqlite 0.40.2 normally bundles SQLite 3.53.2, so the benchmark links it against a project-local build of the actual current SQLite 3.53.4. WAL mode is fixed; synchronous=NORMAL/FULL provides relaxed/sync comparison.

### SurrealDB 3.3.0 / RocksDB backend
Same SurrealDB record workload with RocksDB underneath, isolating backend contribution from query/document-layer cost. Kept in a separate Cargo package because SurrealDB's forked RocksDB native sys crate and standalone rocksdb 0.25.0 both declare links="rocksdb" and Cargo correctly refuses to link both into one package graph.

### RoughDB 0.10.1 — implemented experimental lane
Current 2026 pure-Rust LevelDB port with WAL, MANIFEST, SSTables, background compaction and a runtime per-write sync flag. It maps cleanly to both durability lanes. Compression is explicitly disabled in the neutral comparison so it matches the standalone RocksDB baseline policy.

### jammdb 0.11.0 — implemented sync-only lane
BoltDB-style mmap single-file B+tree. Writable transaction commit calls sync_all and there is no relaxed commit switch, so it participates only in the primary sync lane.

### lsm-db 1.0.0 — implemented sync-only experimental lane
Current stable 2026 Rust LSM. The benchmark enables its durability and bloom features. Durability is a Cargo feature rather than a per-write runtime mode, so this adapter participates only in sync comparisons.

### sanakirja 2.0.0-beta.3 — surveyed, not enabled
This is now the latest published sanakirja line. It remains a lower-level persistent data-structure library: the benchmark would need to define and persist its own root/database mapping rather than merely adapt a byte-KV API. That would make benchmark-specific storage design part of the measured engine. Keep it out until a clean common-denominator adapter can be justified.

## Useful older baselines, not first-wave additions

### jammdb 0.11.0
Rust BoltDB-style mmap single-file B+tree with serializable transactions and lock-free readers. Mature and relevant, but 0.11.0 is roughly three years old; lower priority than current engines above.

### sanakirja 1.4.3 stable / newer beta line
Transactional on-disk data structures and a useful historical pure-Rust reference. API and storage model are lower-level than the byte-KV common denominator, so adding it needs more adapter work and less directly answers the current-engine question.

## Excluded from the first matrix

- tiny educational/new-one-night stores: excluded until they have a credible durability/concurrency story and enough implementation maturity to make the comparison useful.
- redb repackages without a materially different storage path: excluded to avoid measuring the same engine twice.
- redb-turbo 0.2.0: a redb fork whose differentiators are AES-256-GCM page encryption and zstd compression. Interesting for encrypted/compressed-storage testing, but not a clean default-performance peer for this first matrix.

### Concurrency note

Manifold's advertised advantage is parallel writes across column families. The current cold-storage allocation exposes one logical CPU, so this first matrix measures its WAL/column-family path but is not a meaningful test of its headline multi-writer scaling. That requires a separate multi-core run.
