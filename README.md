# rust-db-realistic-bench

Canonical repository: https://github.com/codeandsolder/all-db-bench-rs

The current primary validation/benchmark host is the 8-logical-CPU laptop checkout at /srv/scratch/db-bench-2026-09-27. Results remain host-scoped; do not combine laptop and older cold-storage measurements.

A replacement for the earlier redb/Turso/Fjall growth-loop benchmark, designed around fixed workloads, fresh databases, explicit durability and repeated randomized trials.

## Build

    CARGO_TARGET_DIR=/tmp/rust-db-realistic-bench-target ./scripts/cargo-local-1.99.sh build --release --locked --features kv-all --bin kvbench
    CARGO_TARGET_DIR=/tmp/rust-db-realistic-bench-target ./scripts/cargo-local-1.99.sh build --release --locked --features record --bin recordbench
    ./scripts/cargo-local-1.99.sh build --release --locked --manifest-path engines/surrealdb-rocksdb/Cargo.toml --target-dir /tmp/rust-db-surreal-rocks-target
    ./scripts/cargo-local-1.99.sh build --release --locked --manifest-path engines/tsdb-server/Cargo.toml --target-dir /tmp/rust-db-tsdb-target

## Smoke matrices

    ALLOW_BUSY=1 ./scripts/run-kv-matrix.sh smoke
    ALLOW_BUSY=1 ./scripts/run-record-matrix.sh smoke

ALLOW_BUSY=1 is appropriate only for functional smoke validation. Quick/full performance runs refuse a busy host by default.

## Durability adapter sanity check

    ./scripts/check-sync-syscalls.sh

This untimed probe traces Linux persistence-barrier syscalls. It uses hard relaxed-vs-sync count assertions only where that ordering is meaningful; Persy's background-sync relaxed mode is traced informationally, and sync-only engines are checked for at least one barrier.

## One run

    cargo run --release --bin kvbench -- \
      --engine redb \
      --durability sync \
      --workload read-heavy \
      --records 100000 \
      --ops 50000 \
      --txn-size 100 \
      --root data/manual

See docs/METHODOLOGY.md before interpreting results. In particular, SurrealDB/Turso full-database tests are a separate lane from the raw KV engines.

## Summarize a completed run

    uv run --script scripts/summarize.py results/runs/RUN_ID/results.ndjson \
      --json-out results/runs/RUN_ID/summary.json \
      --markdown-out results/runs/RUN_ID/summary.md

The summarizer aggregates independent trials using the median and IQR. It does not manufacture error bars from adjacent database sizes.

## Validation

    ./scripts/validate.sh

The static validation gate checks the pinned Rust 1.99.0 toolchain, formatting, shell syntax, all Python analysis scripts through `uv`, all in-tree benchmark binaries, and the isolated SurrealDB/RocksDB package. Run the smoke matrices after that to exercise every configured adapter against real database files.

## Wide/comprehensive campaigns

The normal matrix remains the compact baseline. The wider campaign adds controlled size/transaction/scan/scale sweeps without taking a useless full Cartesian product:

    ./scripts/run-kv-wide-matrix.sh quick
    ./scripts/run-record-wide-matrix.sh quick

Long wide runs are resumable at case granularity.

Reopen/cache-state lanes:

    ./scripts/run-reopen-matrix.sh quick warm
    # cold mode intentionally requires root and a quiet host:
    ./scripts/run-reopen-matrix.sh quick cold

Sudden-process-death recovery:

    ./scripts/run-crash-recovery.sh quick
    ./scripts/run-record-crash-recovery.sh quick

Simulated power-loss durability through `dm-log-writes` (requires passwordless sudo/root block-device control on the isolated disposable images):

    ./scripts/run-powerloss-matrix.sh quick
    BENCH_KIND=record ./scripts/run-powerloss-matrix.sh quick

The power-loss lane is separate from SIGKILL recovery. It freezes the writer first, records the acknowledged prefix, suspends the disposable device without flushing, drains only `dm-log-writes`' asynchronous logger through a post-suspend mark, reconstructs the worst-case stable image on a pristine base copy, mounts it for normal filesystem recovery, then runs the same prefix/hole/transaction-atomicity verifier plus `e2fsck -f -n`.

Summarize either SIGKILL or power-loss recovery campaigns without mixing them into throughput results:

    uv run scripts/summarize-recovery.py results/runs/RUN_ID \
      --json-out results/runs/RUN_ID/recovery-summary.json \
      --markdown-out results/runs/RUN_ID/recovery-summary.md

Storage/filesystem calibration (including an aligned pressure-calibration record):

    ./scripts/run-io-baseline.sh quick

On ZFS, `direct=1` is only a request: writes smaller than `recordsize` may be redirected through ARC, and same-file mixed reads/writes can trigger coherence fallback even when aligned. Protocol v4 therefore calibrates recordsize-aligned QD1 random reads and random writes **separately on disjoint files** and verifies each direction against OpenZFS `direct_*` byte counters. The 70/30 reference capacity is `min(read_iops / 0.7, write_iops / 0.3)`.

A completed calibration additionally writes `calibration.json`, binding hashes of both directional fio results, `support.json`, and the parsed calibration summary to the host/filesystem provenance. Calibrated I/O-pressure runs refuse incomplete or modified calibration state.

Dimensional KV sweeps (locality, misses, key shape/size, value entropy, write placement, tombstones and deferred-work settling):

    ./scripts/run-kv-dimensional-matrix.sh quick

Windowed sustained-write / compaction-cliff campaigns:

    ./scripts/run-kv-sustained-matrix.sh quick
    ./scripts/run-record-sustained-matrix.sh quick

The raw-KV and four-product record sustained lanes (SurrealDB/SurrealKV, isolated SurrealDB/RocksDB, Turso, SQLite) remain separate result classes, but share the same fixed-op window analysis: p99 transaction latency, process/resource deltas, write-amplification proxies and post-foreground settle debt. Their 75/50/25% baseline-relative cliff thresholds are diagnostics, not pass/fail criteria. Record `smoke` uses a debug build for functional coverage; `quick` and `full` use release builds and are the only record-sustained profiles intended for performance interpretation.

Shared-database concurrency scaling on a multi-core host (fixed total work at 1/2/4/8 clients in quick mode; full mode also includes 16-client oversubscription):

    ./scripts/run-kv-concurrency-matrix.sh quick
    ./scripts/run-record-concurrency-matrix.sh quick

The raw-KV and record-product concurrency lanes remain separate result classes but use the same fixed-total-work scaling interpretation. Raw KV uses native engine sharing/cloning rather than one database per client or a benchmark-side global mutex; lkv 0.2.1 is explicitly unsupported because its current writer API cannot be shared/cloned without external serialization. Raw-KV v6 distinguishes historical `growth` state evolution from `bounded` steady-state scaling: bounded tiny/write bursts update in place, while balanced/churn toggle a fixed disjoint per-client key pool so longer timing windows do not silently become larger-database tests. Record concurrency schema v7 applies the same separation to its append workloads: growth tiny/write remains an append diagnostic, while bounded tiny/write maps one fixed global logical stream onto the prefilled record universe for performance scaling. Persy concurrency uses a bounded native transaction-lock timeout plus typed timeout retries because a real 8-client delete workload exposed a B-tree lock deadlock under Persy's one-day default timeout; retry cost remains measured. All concurrency cases also have a profile-bound outer wall-clock fail-safe. Record concurrency uses cloned SurrealDB client handles over embedded SurrealKV or isolated RocksDB and independent native Turso/SQLite connections to one shared database; writer lock waiting remains inside the product and is charged to transaction latency.

Controlled CPU/scheduler contention (uniform duty-cycle pressure on every allowed logical CPU):

    ./scripts/run-cpu-contention-matrix.sh quick

Calibrated I/O-dependence, using a completed fio baseline run:

    ./scripts/run-io-contention-matrix.sh quick results/runs/FIO_RUN_ID

Dataset-to-RAM / out-of-core dependence:

    ./scripts/run-kv-out-of-core.sh quick

Explicit cgroup memory-budget dependence is root-only by design; build the KV binary as the normal user first, then:

    sudo ./scripts/run-memory-limit-matrix.sh quick

Do not merge warm reopen, root cold-cache, SIGKILL recovery, simulated power-loss recovery, sustained-write/compaction-cliff, raw fio, controlled CPU pressure, calibrated I/O pressure, memory-limit, out-of-core, relaxed durability, sync durability, raw-KV and record-layer results into one leaderboard. They answer different questions.

## Time-series/server database lane

GreptimeDB, VictoriaMetrics, Prometheus and InfluxDB 3 Core are benchmarked in a separate external-server lane:

    ./scripts/run-tsdb-matrix.sh smoke
    ./scripts/run-tsdb-matrix.sh quick

The runner installs pinned official Linux/amd64 binaries under `.deps/tsdb`, starts one fresh local server per case, uses a logically identical step-aligned `bench_metric{host,region}` dataset, and records server-process CPU/runqueue/I/O/RSS plus on-disk bytes measured after graceful shutdown. GreptimeDB, VictoriaMetrics and Prometheus ingest through Prometheus Remote Write v1 and query with PromQL. InfluxDB 3 ingests the same samples through its native v3 line-protocol endpoint and queries equivalent semantics with SQL. Transport/query language are explicit result fields and are **not** normalized away. This lane must not be merged into the embedded-KV or record-product leaderboards.


## Latest engine set

All engine versions are pinned to the latest published usable release verified on 2026-10-04. Raw KV includes redb 4.3.0, Fjall 3.1.12, SurrealKV 0.21.4, heed/LMDB 0.22.1, sled 1.0.0-alpha.124, lkv 0.2.1, Manifold 3.1.0, TurboKV 0.6.0, ParityDB 0.5.6 in hash and ordered-B-tree configurations, RocksDB 0.25.0, libmdbx 0.9.0, Persy 1.8.1, RoughDB 0.10.1, jammdb 0.11.0 and lsm-db 1.0.0.

The record lane includes SurrealDB 3.3.0/SurrealKV, Turso 0.8.2-pre.2, SQLite 3.53.4 via rusqlite 0.40.2, and an isolated SurrealDB 3.3.0/RocksDB package. SQLite is built project-locally under .deps/sqlite-3.53.4 because rusqlite's bundled copy is older than current SQLite.

Final benchmark binaries are compiled locally with rustc 1.99.0 through scripts/cargo-local-1.99.sh; do not distribute target-cpu=native compilation across heterogeneous workers.
