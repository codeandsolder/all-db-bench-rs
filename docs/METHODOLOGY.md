# Benchmark methodology

## Comparison lanes

The suite deliberately does not put all database-shaped software into one leaderboard.

### Raw embedded KV lane

Comparable logical interface: byte-string key/value point reads, atomic write batches, deletes, and sustained update/churn traffic.

Engines:

- redb 4.3.0
- Fjall 3.1.12
- SurrealKV 0.21.4
- heed 0.22.1 / LMDB reference
- sled 1.0.0-alpha.124
- lkv 0.2.1
- TurboKV 0.6.0
- Manifold 3.1.0 (sync lane; column-family WAL path)
- RocksDB 0.25.0 (mature C++ LSM reference; compression disabled in the neutral raw-KV baseline)
- libmdbx 0.9.0 / MDBX (NoWriteMap mmap/B+tree reference)
- Persy 1.8.1 (pure-Rust copy-on-write + journal/index engine)
- RoughDB 0.10.1 (experimental 2026 pure-Rust LevelDB-compatible LSM; compression disabled in neutral baseline)
- jammdb 0.11.0 (BoltDB-style mmap B+tree; sync-only)
- lsm-db 1.0.0 (new stable Rust LSM with durability+bloom features; sync-only in this build)

lkv is sync-only because 0.2.1 deliberately syncs file commits before publication and exposes no relaxed commit mode.

### Full database / record lane

SurrealDB 3.3.0, Turso 0.8.2-pre.2, and SQLite 3.53.4 via rusqlite 0.40.2 are tested separately with record/document-style primary-key CRUD and transactions. Their results must not be ranked directly against raw byte-KV calls: they include query/document/SQL layers by design. SurrealDB-on-RocksDB is built in a separate Cargo package because its vendored RocksDB native library cannot coexist in one Cargo link graph with the latest standalone rocksdb 0.25.0 crate; its output schema remains identical.

## Durability

The primary cross-engine axes are:

- **relaxed**: acknowledgement without a per-transaction power-loss durability barrier. Exact semantics differ and are written into every result record.
- **sync**: acknowledgement only after the engine's strongest ordinary per-commit disk sync barrier.

Mappings in the raw KV lane:

| Engine | relaxed | sync |
|---|---|---|
| redb | Durability::None | Durability::Immediate |
| Fjall | commit + PersistMode::Buffer | commit + PersistMode::SyncAll |
| SurrealKV | Durability::Eventual | Durability::Immediate |
| heed/LMDB | MDB_NOSYNC | default synchronous commit |
| sled | apply_batch, no explicit flush | apply_batch + Db::flush |
| lkv | unsupported | commit (sync_data before publication) |
| TurboKV | DbOptions::durable | DbOptions::paranoid |
| Manifold | omitted | column-family WAL + Durability::Immediate |
| ParityDB hash / B-tree | default background commit/WAL/data-sync pipeline; commit acknowledges before persistence completes | unsupported: no public durable-before-return commit API |
| RocksDB | WAL, sync=false | WAL, sync=true |
| MDBX | NoWriteMap + SafeNoSync | NoWriteMap + Durable |
| Persy | background-sync transaction | foreground-sync transaction |
| RoughDB | WAL + WriteOptions sync=false | WAL + WriteOptions sync=true |
| jammdb | unsupported | committed writable transaction + sync_all |
| lsm-db | unsupported in durable build | durability feature / wal-db |

These labels are not claims that all relaxed modes have identical crash semantics. The mapping string is part of every JSON result so incompatible guarantees cannot be silently conflated.

For matrix classes that are otherwise sync-only (dimensional, out-of-core, memory-limit and I/O-contention), engines without a durable-before-return API are not silently dropped. Those runners use a strongest-supported durability identity: sync for engines that provide it, and relaxed/background for ParityDB. Durability remains explicit in every result and case ID, so these rows are coverage data rather than claims of identical durability semantics.

## Isolation and ordering

Every benchmark invocation creates a fresh database directory. There is no batch-size or workload state carry-over.

The matrix driver launches one engine/workload/durability trial per process and shuffles the engine/workload order independently for every trial. This removes the fixed-order confounder from the previous benchmark.

## Profiles

- smoke: 100 prefilled records, 100 measured logical operations, 1 trial
- quick: 100k records, 50k operations, 3 trials
- full: 1M records, 250k operations, 7 trials

The compact defaults are intentionally small enough for modest hosts and do not assume one specific machine. Every campaign captures CPU, memory, filesystem, block-device and pressure metadata; cross-host results must remain separate. The current primary execution host is the 8-logical-CPU, ~14 GiB RAM laptop clone, while cold-storage results are retained only for historical methodology work because that host was heavily I/O-contended.

## KV workloads

- **point-read**: uniformly random primary-key point reads from the prefilled working set.
- **range-scan**: ordered 100-row range reads over big-endian integer keys; this exercises B-tree/LSM ordered iteration and resembles short pagination/index walks. lkv 0.2.1 is omitted because it exposes full iteration but no keyed seek/range API. ParityDB's hash-column configuration is also omitted because hash columns are intentionally unordered; its separate B-tree configuration participates using the native seekable ordered iterator. Emulating range seek with an O(N) scan would be a fake comparison.
- **read-heavy**: 95% reads, 5% updates. Existing-key accesses use an 80/20 hot-set distribution.
- **balanced**: 50% reads, 30% updates, 10% inserts, 10% deletes, also using an 80/20 hot set for existing-key access.
- **tiny-txn**: one new record per committed transaction; exposes commit and sync overhead.
- **write-burst**: append-like inserts in configurable transaction batches.
- **churn**: 40% updates, 30% inserts, 30% deletes with uniform existing-key selection; intended to exercise free-space management, tombstones and compaction.

Mixed workloads are generated in fixed 1,000-logical-operation epochs. Write operations are shuffled, grouped into explicit transactions of the configured size, then those write-transaction units are shuffled together with individual reads. Every engine receives the same workload construction rules. This avoids the previous synthetic pattern of doing all reads followed by all writes while still respecting the common API denominator: point reads plus atomic write batches.

## Latency and throughput

Results are NDJSON, one object per isolated run. Each object records:

- total logical operations and wall time
- ops/s
- p50, p95, p99, p99.9 and max point-read latency
- p50, p95, p99, p99.9 and max write-transaction latency
- read/write/delete operation counts
- exact engine version and durability mapping
- on-disk bytes after the measured phase
- seed, trial, value size, dataset size and transaction size

Histograms are actual latency samples. There are no rolling standard-deviation bands across changing database sizes.

## Cache semantics

The normal throughput workloads are warm-cache/application-steady-state tests. The prefilled database receives a short deterministic read warmup before timing.

Cold-start/reopen latency is a separate benchmark class and must not be simulated by dropping Linux page cache inside the normal matrix. If true cold-cache I/O is required later, run it as an explicitly privileged/root-controlled experiment and label it separately.

## Reproducibility

The suite pins a Rust 1.98.1 toolchain and exact database crate versions. Workload generation uses a fixed seed plus trial number. The matrix stores host/kernel/filesystem/toolchain metadata beside the result file.

No benchmark stops because of one slow observation. Tail stalls remain samples in the latency distribution instead of censoring the rest of the series.

## Which comparisons are actually apples-to-apples?

The **sync lane is the primary apples-to-apples durability comparison**: every acknowledged measured write is behind the engine's ordinary power-loss durability barrier.

The relaxed lane is deliberately secondary. It answers "what does this engine cost in its practical non-fsync mode?" but exact crash guarantees differ. Never rank relaxed results as though they promise identical recovery. In particular, redb Durability::None is an in-process non-durable commit, while several WAL/LSM engines push acknowledged data at least into operating-system buffers.

## Build isolation on cold-storage

Cargo build artifacts are directed to `/tmp/rust-db-realistic-bench-target`, and the benchmark-local Cargo registry/source cache defaults to `/tmp/db-bench-cargo-home`. This keeps both generated objects and crate-source/registry metadata off the measured `/srv/scratch` filesystem and avoids the shared Sentinel Cargo cache. `scripts/cargo-local-1.98.1.sh` pins rustc 1.98.1, disables `RUSTC_WRAPPER`, and forces native CC/CXX to `/usr/bin/cc` and `/usr/bin/c++`, because `target-cpu=native` makes heterogeneous distributed compilation invalid. The benchmark data itself remains under `/srv/scratch/db-bench-2026-09-27/data`.

## Durability syscall sanity check

`scripts/check-sync-syscalls.sh` is an untimed validation probe. It traces a tiny single-transaction run and compares Linux sync-barrier syscall counts where syscall-count ordering is semantically meaningful. redb/Fjall/SurrealKV/heed/sled/TurboKV/RocksDB/MDBX/RoughDB use a hard relaxed-vs-sync count check; lkv/Manifold/jammdb/lsm-db are sync-only and must show a barrier. Persy is traced but informational because its relaxed mode deliberately performs the durability sync in the background after acknowledgement, so the same eventual syscall can occur before process exit. This is not proof of hardware-level persistence; crash/recovery tests validate the acknowledgement boundary.

## Result/data retention

Result NDJSON, summaries and provenance metadata are durable. Per-run database directories are deleted after their final on-disk size has been recorded, unless the individual benchmark is run with `--keep-db`. This prevents the full repeated matrix from consuming storage merely to retain equivalent scratch databases.

## Concurrency

The primary throughput matrices remain single-client so their historical semantics do not change. The current laptop execution host exposes 8 logical CPUs, so a separate multi-client lane is now worthwhile, but it must be adapter-aware rather than forcing every database through an external lock.

A compile-time probe showed that sled 1.0.0-alpha.124's current handle is Clone but not Sync, while lkv's write transaction API requires mutable database access. Hiding either behind one global mutex would make a graph that looks like engine scaling but actually measures benchmark-side serialization. The concurrency lane is therefore being implemented separately using only native share/clone/multi-handle mechanisms that preserve one logical database, and unsupported combinations will be reported explicitly. Concurrency results must never be merged with this single-client baseline.

## Comprehensive matrix expansion — 2026-10-04

The wide campaign is deliberately a core matrix plus targeted sweeps, not a full Cartesian product. A full product of engine × durability × workload × dataset size × value size × transaction size × scan width would spend most of its runtime repeating uninformative combinations.

run-kv-wide-matrix.sh now covers:

- core: every supported raw-KV engine, both durability lanes where the engine exposes them, and every established workload;
- working-set scale: 1k through 1M records in quick mode and through 5M in full mode for point-read, read-heavy and churn;
- value size: metadata-dominated empty/tiny values through 16 KiB values on point reads, append-like writes and churn;
- transaction size: 1, 4, 16, 64, 256 and 1024 writes per committed batch;
- scan width: 1 through 2048 ordered rows, with measured scan-count reduced at large widths so the campaign remains wide rather than being dominated by a few enormous scans;
- deliberately awkward but still plausible combinations such as synchronous 4 KiB values committed one at a time, huge batches of tiny values, and large-value churn.

run-record-wide-matrix.sh applies the same idea to the SurrealDB/Turso record lane with dataset scale, payload size and transaction-size sweeps. These remain a separate product-level lane.

Every long campaign is resumable at case granularity. A successful case is one JSON file under the run directory; a restarted campaign skips those files and reconstructs results.ndjson from the completed case set. A single failing adapter therefore does not erase a multi-hour campaign.

## Measured-phase resource accounting

Schema version 2 records resource deltas around only the measured workload, excluding open, prefill, deterministic warmup and close:

- scheduler CPU runtime and runnable-queue wait time from /proc/self/schedstat;
- minor/major faults;
- voluntary/involuntary context switches;
- rchar, wchar, read/write syscall counts and kernel-attributed read_bytes / write_bytes from /proc/self/io;
- measured-phase RSS before/after and thread count;
- system load and available memory before/after;
- CPU, I/O and memory PSI total deltas;
- system page-fault, swap-in/out and reclaim deltas.

Process I/O bytes are the primary attribution metric. System PSI and VM deltas are noise/context signals, not database-owned resource usage, because cold-storage is shared.

The wide KV runner additionally captures whole-device /sys/class/block/DEVICE/stat before and after every case. Those counters are intentionally not merged into process-attributed I/O; they are used to identify a run that overlapped unrelated device traffic.

The summarizer retains the raw records and adds cross-trial medians for CPU ns/op, runqueue wait fraction, read/write B/op, major faults, full-I/O PSI and swap activity alongside throughput, latency, DB size and RSS.

## Open/reopen and cache-state lanes

Database open time is recorded separately as open_s.

run-reopen-matrix.sh ... warm prepares a database, exits the creating process, then reopens it in a new process with no benchmark warmup. Linux page cache remains intact. This isolates engine reopen/recovery/application-cache behavior without calling the result a physical cold read.

run-reopen-matrix.sh ... cold performs the same sequence but requires a real root-owned:

    sync
    echo 3 > /proc/sys/vm/drop_caches

between prepare and reopen. The script refuses to emulate this with a large eviction file or other cache-thrashing workaround. Because dropping page cache affects the whole host, the cold-cache lane must be run on an otherwise quiet machine and never mixed into the normal warm-cache leaderboard.

## Sudden process termination and recovery

run-crash-recovery.sh is a process-crash test, not a simulated power failure.

For every selected engine/durability/batch-size/delay combination it:

1. prepares a known base database;
2. reopens it and issues sequential write batches;
3. writes a small external progress marker only after a database transaction has returned successfully;
4. waits until at least one transaction has been acknowledged, then sends the writer SIGKILL after the configured delay;
5. reopens the database in a fresh process;
6. verifies every base + externally acknowledged record is readable;
7. checks the unreported tail for holes and for a partially recovered multi-record transaction;
8. records reopen time and verification time.

One completely recovered extra batch is valid: the process may have committed a transaction and been killed before updating the external progress marker. A partial batch or a later key appearing after a gap is a recovery-consistency failure.

For **sync** configurations, losing any externally acknowledged prefix record is a benchmark failure because durable-before-return is the contract being tested. For **relaxed/background** configurations, acknowledged loss is a legitimate measured outcome: the result keeps verification_ok=false, and the runner records the case in relaxed-ack-losses.ndjson without failing the entire campaign. Reopen failure, corruption, holes, or partial multi-record transactions remain hard failures in either lane.

This lane establishes process-death consistency and directly measures acknowledged-write preservation with the kernel still alive. It does not establish power-loss durability because Linux page cache and the virtual block device survive SIGKILL. A true power-cut lane requires VM/block-device fault injection or reboot/power interruption and must be reported separately.

## Raw storage calibration

scripts/run-io-baseline.sh creates a disposable benchmark file under the same /srv/scratch filesystem and records fio JSON for:

- direct sequential 1 MiB QD1 read/write;
- direct random 4 KiB QD1 read/write and 70/30 mixed I/O;
- direct random 4 KiB QD16 read/write;
- buffered random/sequential reads;
- 4 KiB buffered writes with fdatasync after each write.

These are storage calibration numbers, not database scores. They let database CPU/op, process I/O bytes/op and commit latency be interpreted against the backing volume's current bandwidth/IOPS/fsync envelope.


## Schema-v3 dimensional KV campaign — 2026-10-04

scripts/run-kv-dimensional-matrix.sh adds explicit dimensions that the compact historical core intentionally left fixed:

- locality: uniform, 80/20 hot-set and 95/5 hot-set read distributions;
- miss ratios: 0%, 1%, 10%, 50% and 100% negative point lookups;
- key sizes: 8, 16, 64 and 256 bytes;
- key shapes: ordered sequential IDs with deterministic suffixes, large shared prefixes with IDs in the trailing 8 bytes, and deterministically hashed/permuted keys;
- value entropy: high-entropy deterministic pseudo-random bytes, all-zero values and repeated 8-byte patterns;
- write placement: append/new-key, uniform in-place updates and hot-set in-place updates;
- delete-burst: unique batched deletes for tombstone/free-space/reclamation behavior;
- fixed post-workload settle windows to expose deferred/background compaction, checkpointing and writeback.

The historical default is preserved by access_pattern=auto, key_bytes=8, key_shape=sequential, value_pattern=pseudo-random, miss_percent=0 and write_pattern=append. Existing core cases therefore do not silently change semantics.

Hashed keys deliberately destroy ID order. They are valid for point/update tests but rejected for the ID-ordered range-scan workload rather than manufacturing a fake range interpretation.

Foreground workload timing ends before the settle window begins. During settling, the engine remains open and the harness samples process CPU/I/O, shared-host pressure context and database size at fixed intervals. This makes "fast foreground because work was deferred" visible without charging that work to the foreground latency distribution.

## Controlled I/O-dependence campaign

scripts/run-io-contention-matrix.sh takes a completed run-io-baseline.sh result and uses its measured 4 KiB QD1 70/30 random-I/O rate as the calibration point. It runs the same database cases with independent direct-I/O fio pressure capped at explicit fractions of that baseline (quick mode: 0/10/30/60%; full adds 90%).

The pressure workload uses a separate disposable file on the same filesystem. Every case retains fio JSON so requested pressure can be compared with delivered pressure. Results are labeled io-pressure-Npct and must not be mixed with no-pressure leaderboard cases.

The runner refuses to start on an already busy host unless explicitly overridden. A calibrated 30% pressure lane is meaningful only when there is not an uncontrolled second source of saturation already consuming the device.

## Memory/cache-budget campaign

scripts/run-memory-limit-matrix.sh is intentionally root-only. It uses transient systemd cgroups with explicit MemoryMax and MemorySwapMax=0 rather than simulating pressure by allocating anonymous junk memory in a sibling process.

Quick mode tests 25%, 50% and 75% of host RAM; full also includes 15%. Point-read, read-heavy and churn are run against the same logical dataset. cgroup result/status and MemoryPeak are stored per case, and OOM/failure is retained as a first-class failure result rather than silently retried with a larger budget.

The script refuses to build as root; the benchmark binary must be built first as the normal benchmark user. Root is used only for the cgroup control that actually requires it.

## Out-of-core / dataset-to-RAM dependence

scripts/run-kv-out-of-core.sh sizes a pseudo-random 4 KiB-value dataset by logical value footprint relative to host MemTotal. Quick mode targets 50% and 100% of RAM; full adds 200%.

This ratio is an input logical-footprint target, not a claim about exact database bytes. Every result still records the actual engine-specific on-disk size and RSS. Using pseudo-random values avoids accidentally turning a nominally out-of-core dataset into a tiny compressed database on engines with compression.

Before each campaign the runner requires substantial free disk headroom for WAL/SSTable/compaction amplification and refuses the run instead of filling the volume.

## Current execution host and concurrency follow-up

Code and canonical history now live at https://github.com/codeandsolder/all-db-bench-rs. Validation/execution moved from the I/O-contended cold-storage VPS to the laptop checkout under /srv/scratch/db-bench-2026-09-27. The laptop's ZFS-backed /srv/scratch has substantially more free space and 8 logical CPUs, making it suitable for out-of-core, compaction and future concurrency campaigns.

Concurrency remains a separate result class. It will use native engine concurrency primitives and explicit support metadata rather than an external benchmark mutex.
