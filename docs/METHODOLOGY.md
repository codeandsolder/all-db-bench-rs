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
- Manifold 3.1.0 (sync lane; column-family no-WAL + Durability::Immediate primary path; default-WAL path retained only as a diagnostic negative control)
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
| Manifold | omitted | column-family without WAL + Durability::Immediate |
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

The suite pins a Rust 1.99.0 toolchain and exact database crate versions. Workload generation uses a fixed seed plus trial number. The matrix stores host/kernel/filesystem/toolchain metadata beside the result file.

No benchmark stops because of one slow observation. Tail stalls remain samples in the latency distribution instead of censoring the rest of the series.

## Which comparisons are actually apples-to-apples?

The **sync lane is the primary apples-to-apples durability comparison**: every acknowledged measured write is behind the engine's ordinary power-loss durability barrier.

The relaxed lane is deliberately secondary. It answers "what does this engine cost in its practical non-fsync mode?" but exact crash guarantees differ. Never rank relaxed results as though they promise identical recovery. In particular, redb Durability::None is an in-process non-durable commit, while several WAL/LSM engines push acknowledged data at least into operating-system buffers.

## Build isolation on cold-storage

Cargo build artifacts are directed to `/tmp/rust-db-realistic-bench-target`, and the benchmark-local Cargo registry/source cache defaults to `/tmp/db-bench-cargo-home`. This keeps both generated objects and crate-source/registry metadata off the measured `/srv/scratch` filesystem and avoids the shared Sentinel Cargo cache. `scripts/cargo-local-1.99.sh` pins rustc 1.99.0, disables `RUSTC_WRAPPER`, and forces native CC/CXX to `/usr/bin/cc` and `/usr/bin/c++`, because `target-cpu=native` makes heterogeneous distributed compilation invalid. The benchmark data itself remains under `/srv/scratch/db-bench-2026-09-27/data`.

## Durability syscall sanity check

`scripts/check-sync-syscalls.sh` is an untimed validation probe. It traces a tiny single-transaction run and compares Linux sync-barrier syscall counts where syscall-count ordering is semantically meaningful. redb/Fjall/SurrealKV/heed/sled/TurboKV/RocksDB/MDBX/RoughDB use a hard relaxed-vs-sync count check; lkv/Manifold/jammdb/lsm-db are sync-only and must show a barrier. Persy is traced but informational because its relaxed mode deliberately performs the durability sync in the background after acknowledgement, so the same eventual syscall can occur before process exit. This is not proof of hardware-level persistence; crash/recovery tests validate the acknowledgement boundary.

## Result/data retention

Result NDJSON, summaries and provenance metadata are durable. Per-run database directories are deleted after their final on-disk size has been recorded, unless the individual benchmark is run with `--keep-db`. This prevents the full repeated matrix from consuming storage merely to retain equivalent scratch databases.

## Concurrency

Concurrency is a separate raw-KV result lane (lane = "kv-concurrency"); it is never merged with the historical single-client throughput matrix.

scripts/run-kv-concurrency-matrix.sh keeps total logical work fixed across client counts. Quick mode measures 1/2/4/8 simultaneous clients on the 8-logical-CPU laptop; full mode also includes 16 clients as a deliberate oversubscription/suboptimal case. Every case uses one shared logical database, one common prefill, one synchronized start barrier and disjoint append/delete ID ranges. Reads and update-in-place workloads intentionally contend on the same populated/hot sets.

The harness uses each engine's native concurrency surface rather than forcing a common external lock:

- redb, Manifold, TurboKV, ParityDB, RocksDB, MDBX, RoughDB and lsm-db use shared thread-safe handles via Arc;
- Fjall, SurrealKV, heed, Persy and jammdb use their native cloneable handles;
- sled 1.0.0-alpha.124 is Send + Clone but deliberately not Sync, so each client receives a native sled::Db clone pointing at the same database;
- lkv 0.2.1 is explicitly unsupported for this lane: write transactions require mutable Database access and there is no native clone/shared writer handle. Putting it behind a benchmark mutex would falsely measure harness serialization as engine scaling.

Async-capable engines share one multi-thread Tokio runtime. Client OS threads enter that runtime through a common Handle; this is important for engines such as TurboKV whose background durability tasks are tied to the runtime where the database was opened. The coordinator's synchronous wait is wrapped in Tokio block_in_place, so Tokio provisions replacement runtime capacity instead of silently stealing one worker from engine background tasks. An earlier per-client/current-thread runtime prototype was rejected because it could strand async durability work.

Client threads remain alive until the coordinator takes the process-after resource snapshot. This preserves their TIDs for schedstat/runqueue/context-switch accounting. Reported CPU runtime uses nanosecond per-TID schedstat deltas. The coarser process-wide /proc/self/stat utime+stime delta is retained only as a diagnostic for CPU consumed by threads that are created and destroyed entirely between snapshots; it is never maxed into the reported CPU value because USER_HZ quantization badly distorts millisecond-scale runs.

Each result includes aggregate HDR latency histograms plus per-client throughput/latency records and a max/min client-throughput ratio. The summarizer computes median speedup versus the otherwise-identical 1-client case, parallel efficiency (speedup / clients), CPU-core equivalents consumed and client fairness.

Optimistic transaction conflicts are treated as part of the engine's concurrency cost, not as benchmark corruption. SurrealKV TransactionWriteConflict and TransactionRetry are retried with the same logical write set until commit succeeds (bounded at 10,000 retries for fail-safe termination). The timed write-transaction latency includes every failed attempt, and aggregate/per-client retry counts are emitted. Other errors still fail the case.

Primary durability is strongest-supported: sync for engines with a durable-before-return API and relaxed/background for ParityDB. A smaller relaxed-mode scaling subset is run separately for engines that expose a meaningful relaxed path. Durability remains part of the exact grouping key.

The core concurrency matrix includes point reads, read-heavy, balanced, tiny one-record transactions, batched write bursts and churn. A smaller dedicated delete-burst slice measures concurrent tombstone/free-space behavior without multiplying every client count into every stress dimension.

Range-scan concurrency uses only engines/configurations with a native ordered seek/range API. paritydb-hash is omitted while paritydb-btree participates. Full mode includes 16 clients on the 8-CPU laptop intentionally; those rows measure oversubscription/queueing behavior rather than pretending 16 hardware threads exist.

### Record-product concurrency

The record-product concurrency lane (`lane = "record-concurrency"`) uses the same fixed-total-work and c1 scaling interpretation as raw KV, but intentionally keeps document/SQL/index overhead in scope. SurrealDB/SurrealKV and the isolated SurrealDB/RocksDB package each receive one native cloned Surreal client handle per worker. Turso receives one independent `Database::connect()` connection per worker; cloning one Turso `Connection` is deliberately avoided because its clones share a connection-operation gate. SQLite receives one independent WAL connection per worker.

Turso and SQLite use their native 60 s busy-timeout mechanisms, so writer lock wait is charged to measured transaction latency and client fairness rather than hidden behind benchmark-side retry loops. SurrealDB errors are retried only when the public typed API reports `QueryError::TransactionConflict`; no message/string matching is used. Retries are bounded at 10,000, counted per client and in aggregate, and the operation/transaction latency includes every failed attempt. All record client OS threads enter the same multi-thread Tokio runtime through a shared `Handle`; the coordinator waits inside `block_in_place`, preserving runtime capacity for engine background tasks. Native client fan-out time is measured separately as `client_setup_s` and excluded from synchronized foreground throughput.

Write-burst work is partitioned only at transaction boundaries: total `ops` is interpreted as whole transactions and no client receives a benchmark-manufactured partial final commit. Tiny/write-burst append IDs use disjoint global ranges; point/index/read-heavy clients intentionally share the populated record/index sets. In addition to the five core record workloads, targeted slices cover relaxed durability, large payloads, transaction sizes 1/1000, and a 64-record high-contention read-heavy working set. Full mode adds 16 clients on the 8-logical-CPU laptop as a deliberate oversubscription case.

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

run-crash-recovery.sh (raw KV) and run-record-crash-recovery.sh (SurrealDB/Turso/SQLite record lane) are process-crash tests, not simulated power failures.

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

`scripts/run-io-baseline.sh` creates a disposable benchmark file under the same filesystem as the database data and records fio JSON for:

- requested-direct sequential 1 MiB QD1 read/write;
- requested-direct random 4 KiB QD1 read/write and 70/30 mixed I/O;
- dedicated QD1 random-read and random-write **pressure calibrations on separate files**, using a block size eligible for Direct I/O in both directions;
- requested-direct random 4 KiB QD16 read/write;
- buffered random/sequential reads;
- 4 KiB buffered writes with `fdatasync` after each write.

`direct=1` describes the fio/O_DIRECT request, not by itself a proof that every filesystem performed uncached I/O. This distinction matters on OpenZFS: direct reads need page alignment, but direct writes also require the request offset and length to be `recordsize`-aligned. With the laptop dataset's 128 KiB recordsize, a 4 KiB write requested with O_DIRECT is redirected through ARC, and mixed 4 KiB read/write tests can therefore include cache-coherency effects. Those 4 KiB results remain useful filesystem-behavior diagnostics but are not used to calibrate external storage pressure.

The runner writes `support.json` with calibration protocol version, filesystem/source, ZFS recordsize/direct-mode information where applicable, and both authoritative directional pressure-calibration filenames plus their shared block size. Each fio case also brackets OpenZFS pool `direct_*`/`arc_*` kstats and Linux block-device counters. On ZFS, the read calibration is accepted only when `direct_read_bytes` covers the submitted read traffic and the write calibration only when `direct_write_bytes` covers the submitted writes; `direct=1` and alignment alone are not treated as proof. On ZFS with Direct I/O enabled, the pressure block size is the dataset recordsize and fio also receives an explicit matching `blockalign`; on other filesystems it defaults to 4 KiB. If ZFS Direct I/O is disabled, the raw pressure calibration is refused rather than silently measuring ARC throughput.

Protocol v3 used one aligned 70/30 `randrw` file. Laptop validation rejected it: aligned writes were 100% Direct I/O, but only 44.315% of submitted reads traversed ZFS's Direct-I/O path because overlapping mixed access triggered coherence behavior. Protocol v4 removes that ambiguity by using disjoint files and derives the 70/30 total capacity transparently as `min(read_iops / 0.7, write_iops / 0.3)`.

These are storage calibration numbers, not database scores. They let database CPU/op, process I/O bytes/op and commit latency be interpreted against the backing volume's current bandwidth/IOPS/fsync envelope, while keeping cached/filesystem-small-write behavior distinct from the aligned pressure reference.


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

## Sustained-write / compaction-cliff campaign

`scripts/run-kv-sustained-matrix.sh` is a separate raw-KV result class for behavior that a single aggregate throughput number hides: write stalls, compaction/checkpoint cliffs, write amplification spikes and recovery after a stall.

Each case prefills one fresh database, performs optional deterministic warmup, then executes a fixed number of logical mutations split into fixed-op windows. `kvsustained` emits one HDR transaction-latency histogram and process/system resource delta per window. Append, uniform in-place update, 95/5 hot-set update and exact 40/30/30 update/insert/delete churn patterns are supported. Churn uses the same 1,000-operation epoch construction as the ordinary raw-KV workload, then shuffles mutations and groups them into the configured transaction size.

`scripts/run-record-sustained-matrix.sh` applies the same window methodology to the record-product lane with `recordsustained` over SurrealDB/SurrealKV, Turso and SQLite. It remains a separate result class because SQL/document/index overhead is intentionally part of those measurements. Its churn batches preserve the same 40/30/30 logical mix and execute mixed upserts and deletes inside one database transaction rather than splitting a logical batch into two commits. For the process-write/logical-write proxy, record upserts count the 8-byte ID, 4-byte bucket and configured payload, while deletes count the 8-byte ID; this is an explicit logical-byte estimate, not physical media write amplification.

Window boundaries deliberately avoid recursive database-size scans. Such scans can create artificial idle gaps in which an LSM/background worker catches up, weakening the very stall behavior being measured. Database size is recorded before the foreground workload, after all foreground windows and after the optional settle period instead. Window evidence focuses on throughput, p99 transaction latency, CPU, process-attributed write bytes, scheduler delay and PSI.

Two foreground clocks are retained:

- `elapsed_s` is total wall time from the start of the first sustained window through completion of the last, including the small snapshot/histogram bookkeeping between windows;
- `active_elapsed_s` is the sum of only the mutation-loop intervals;
- `instrumentation_s` and `instrumentation_fraction_of_wall` expose their difference;
- `ops_per_s` uses total wall time, while `active_ops_per_s` is emitted only as a diagnostic.

The summarizer uses the median of the first `min(3, window_count)` windows as each trial's local opening baseline. It reports minimum throughput ratio, counts and longest runs below 75/50/25% of that baseline, first 50% cliff position, later recovery to 90%, worst p99 inflation, per-window CPU and process-write/logical-mutated-byte ratios, runqueue/CPU-PSI/I/O-PSI maxima, and post-workload settle debt. These thresholds are **descriptive diagnostics, not correctness gates**; raw window records remain authoritative.

The campaign remains core + targeted sweeps rather than a Cartesian product. Primary cases use the strongest supported durability identity (sync where durable-before-return exists; ParityDB's documented background path otherwise). Targeted slices add 4 KiB values for LSM-style engines, meaningful relaxed/background modes, and deliberately awkward transaction-size extremes (`1` and `1000`). Smoke/quick/full differ in work and trial count, not result semantics.

## Controlled CPU/scheduler-contention campaign

scripts/run-cpu-contention-matrix.sh measures sensitivity to external CPU scheduler pressure separately from the ordinary no-pressure leaderboard. It derives the actual CPU IDs from the process affinity mask rather than assuming CPUs are numbered 0..N-1.

For every nonzero pressure point the runner starts one taskset-pinned worker on every allowed logical CPU. Each worker uses a 20 ms duty-cycle period at the requested busy percentage. This is deliberate: fully occupying only a subset of CPUs lets Linux migrate the database workload onto idle CPUs, so a nominal 50% load can otherwise produce almost no scheduling contention. Quick mode uses 0/25/50/75/100%; full adds 12%; smoke uses 0/50/100%.

Pressure is sustained for the whole benchmark process invocation, including open, prefill, warmup and the measured phase. The measured workload still has its normal process-accounting boundaries. Each run writes support.json documenting the allowed CPU list and pressure method, plus per-case /proc/pressure/cpu snapshots. Results are labeled cpu-contention-Npct and must remain separate from no-pressure throughput results.

The summarizer reports CPU PSI some fraction in addition to the benchmark process's own runqueue-wait fraction. PSI describes host-wide runnable work delayed for CPU; runqueue_wait_fraction describes scheduler delay charged directly to benchmark threads.

## Controlled I/O-dependence campaign

`scripts/run-io-contention-matrix.sh` consumes only a completed protocol-v4 storage calibration. Quick mode applies 0/10/30/60% of the derived 70/30 composite capacity; full adds 90%. Smoke uses 0/30/60% for functional coverage. Quick contention requires a quick/full baseline; full contention requires a full baseline.

For each nonzero case the runner starts **two independent QD1 psync fio workers on two disjoint files**: one aligned `randread` worker and one aligned `randwrite` worker. Their requested caps are the calibrated 70/30 fractions of the requested total pressure. This preserves the exact block size/alignment/O_DIRECT semantics proven by the directional calibration without allowing read/write address overlap to trigger ZFS coherence fallback. The raw read and write fio JSON files are retained separately, and the per-case pressure sidecar embeds both job records plus requested total/read/write caps.

Both pressure workers span the entire database process invocation, including open, prefill, warmup and the measured phase. They are preconditioned for 2 seconds and each uses a 2-second `ramp_time`, so fio excludes its startup/precondition period from delivered-rate statistics. Two storage-evidence windows are deliberately kept separate: `storage/` brackets only the database invocation for contention/context metrics, while `pressure-storage/` brackets the pressure workers from before launch until after they are stopped and is used to corroborate that enough OpenZFS `direct_*` bytes traversed the Direct-I/O path. The verbatim fio output is retained as `.fio-output` because fio 3.42 prepends a termination notice when an indefinite job is stopped with SIGINT; a validated JSON payload extracted from that output is stored separately and used by the analyzer. After each nonzero-pressure case both workers are stopped and reaped, `sync` completes, and the runner waits briefly before the next randomized case so deferred writeback from one pressure level is not silently charged to another. Delivered read and write rates, their individual target ratios, and the total delivered/composite-baseline ratio remain explicit; saturation below a requested cap is retained as evidence rather than rewritten.

The storage baseline writes `calibration.json` only after every declared fio case and the storage summary have completed successfully. That manifest hashes `support.json`, both authoritative directional calibration results, and the parsed calibration summary; the contention runner refuses modified, partial, cross-host, or cross-filesystem calibration state.

A resumed case is considered complete only when both its database result and pressure evidence are present and parseable. Stale failure records are cleared after a successful rerun, setup/build failures cannot fall through to an old binary, and a failed trial-count summary makes the campaign fail rather than merely producing a warning file.

Quick/full performance campaigns additionally bracket every database invocation with an external-noise guard. Normal-priority compiler/build processes, wide filesystem scans, or otherwise unknown foreign processes using at least 50% CPU invalidate the current case and stop the campaign with resumable evidence; deliberately de-prioritized work at nice 15 or higher is ignored by policy; the dedicated `sccache-dist-worker.service` subtree is also ignored when its worker root is at nice 10 or higher, matching the host's explicit background-worker policy. A case rejected after execution is removed before it can be accepted, so resuming the same run ID reruns that case rather than mixing contaminated data into the aggregate. Smoke is exempt because it is a functional gate rather than performance data.

`scripts/summarize-io-pressure.py` joins each database result to its two-job pressure sidecar. It reports total and directional requested-versus-delivered pressure, delivered/composite-baseline IOPS, pressure bandwidth and directional p99 latency, database throughput and p99 ratios versus the same trial's 0%-pressure case, OpenZFS Direct-I/O evidence, and the database process/PSI context. Delivered pressure below a requested cap is retained as a measured outcome under device contention rather than rewritten as though the cap were achieved.

Results are labeled `io-pressure-Npct` and must not be mixed with no-pressure leaderboard cases. The runner refuses to start on an already busy host unless explicitly overridden; a calibrated pressure lane is meaningful only when there is not an uncontrolled second source of saturation already consuming the device.

## Memory/cache-budget campaign

scripts/run-memory-limit-matrix.sh is intentionally root-only. It uses transient systemd cgroups with explicit MemoryMax and MemorySwapMax=0 rather than simulating pressure by allocating anonymous junk memory in a sibling process.

Quick mode tests 25%, 50% and 75% of host RAM; full also includes 15%. Point-read, read-heavy and churn are run against the same logical dataset. cgroup result/status and MemoryPeak are stored per case, and OOM/failure is retained as a first-class failure result rather than silently retried with a larger budget.

The script refuses to build as root; the benchmark binary must be built first as the normal benchmark user. Root is used only for the cgroup control that actually requires it.

## Out-of-core / dataset-to-RAM dependence

scripts/run-kv-out-of-core.sh sizes a pseudo-random 4 KiB-value dataset by logical value footprint relative to host MemTotal. Quick mode targets 50% and 100% of RAM; full adds 200%.

This ratio is an input logical-footprint target, not a claim about exact database bytes. Every result still records the actual engine-specific on-disk size and RSS. Using pseudo-random values avoids accidentally turning a nominally out-of-core dataset into a tiny compressed database on engines with compression.

Before each campaign the runner requires substantial free disk headroom for WAL/SSTable/compaction amplification and refuses the run instead of filling the volume.

## Current execution host

Code and canonical history live at https://github.com/codeandsolder/all-db-bench-rs. Validation/execution moved from the I/O-contended cold-storage VPS to the laptop checkout under /srv/scratch/db-bench-2026-09-27. The laptop's ZFS-backed /srv/scratch has substantially more free space and 8 logical CPUs, making it the primary host for out-of-core, compaction, concurrency and controlled-pressure campaigns.

Shared-database concurrency is implemented as its own result class with native engine sharing/cloning and explicit support metadata; see the Concurrency section above.


## Simulated power-loss durability — dm-log-writes lane

`scripts/run-powerloss-matrix.sh` is a separate sync-durability lane for raw KV (`BENCH_KIND=kv`, the default) and the record products (`BENCH_KIND=record`). It requires real root-capable loop/device-mapper control and refuses to substitute a userspace cache-thrashing approximation.

Every case uses a fresh ext4 filesystem image derived from a clean, fully synced base image. The database is mounted at one stable path for base creation, live mutation and recovery; this is required for engines such as TurboKV that persist absolute SSTable paths in metadata.

The cut protocol is deliberately ordered:

1. run synchronous write batches and update an external progress file only after a database transaction returns;
2. after the configured delay, send `SIGSTOP` to the writer and wait until the process is actually stopped;
3. read the acknowledged-prefix marker while userspace is frozen;
4. call `dmsetup suspend --noflush` on the `dm-log-writes` target, which quiesces target I/O without manufacturing a filesystem flush;
5. insert a unique `dm-log-writes` mark and wait until `replay-log --find --end-mark` can see it;
6. snapshot the log, kill the stopped writer, resume/unmount/remove the original mapper, and discard that live image;
7. replay the captured durable log onto a pristine copy of the pre-write base image;
8. mount the reconstructed image, allowing ordinary ext4 journal recovery, reopen the database and verify the entire acknowledged prefix plus the bounded unreported tail;
9. unmount and run `e2fsck -f -n` on the reconstructed image.

The post-suspend mark is essential. `dm-log-writes` completes the original target bio first and queues its own log record to a separate kernel thread. A database `fsync`/FUA can therefore return correctly before the logging device's superblock has caught up. Reading the log immediately at the cut produced false one-transaction losses and occasional missing-log magic. The mark is queued after the device is quiescent and updates the log superblock in-order, so waiting for that mark drains already-durable FUA/flush records without splicing ordinary unflushed writes into the durable set.

Likewise, recovery is never run while the original mapper remains suspended. Sled 1.0.0-alpha.124 invokes the global `sync` command during recovery; leaving the intentionally frozen source filesystem mounted would make that unrelated global sync block forever. The runner snapshots the cut log first, then resumes and tears down the live source before replay/reopen.

This models a worst-case stable block image according to the kernel's flush/FUA ordering rules. It is stronger than SIGKILL-with-kernel-alive testing, but it is still a software fault-injection model rather than a claim about a particular SSD controller, capacitor, firmware bug or torn-sector behavior. Those require physical-device/power-interruption testing.

The primary Manifold 3.1.0 sync comparison uses `without_wal()` + `Durability::Immediate`. The upstream-default WAL configuration is exposed as `manifold-wal` only as an explicit diagnostic/negative control: it reproducibly loses the acknowledged post-base transaction under both true SIGKILL and the simulated power-loss lane, while no-WAL Immediate passes. It is intentionally excluded from default campaigns.
