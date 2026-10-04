# Result schema

Each isolated benchmark invocation appends exactly one JSON object to NDJSON.

## Identity and configuration

- `format_version`: schema version.
- `engine`, `engine_version`: exact adapter target.
- `durability`, `durability_mapping`: comparison lane plus the concrete engine API/configuration used.
- `workload`, `records`, `value_bytes` or `payload_bytes`, `txn_size`, `scan_len`, `trial`, `seed`: complete workload definition needed to group repetitions.

## Timing

- `prefill_s`: database population time; never included in measured workload throughput.
- `warmup_s`: deterministic warmup time; never included in measured workload throughput.
- `elapsed_s`: measured workload wall time only.
- `ops_completed`: logical workload operations completed.
- `ops_per_s`: `ops_completed / elapsed_s`.

For `range-scan`, one logical operation is one range query. `reads` records rows visited, while `ops_completed` records range queries, so row throughput can be derived as `reads / elapsed_s`.

## Latency

`read_latency` / `operation_latency` and `write_txn_latency` / `transaction_latency` contain actual HDR-histogram sample counts and p50/p95/p99/p99.9/max in microseconds.

Write latency is per committed transaction, not divided by keys in the batch. Keep `txn_size` beside it when comparing results.

## Resource/storage

- `db_bytes`: recursive on-disk bytes after the engine has been closed and before optional cleanup. For schema-v3 KV runs with a settle window, `post_workload_settle.db_bytes_before/after` captures size while the engine remains open.
- `peak_rss_kib`: Linux `VmHWM` for the process, including prefill and measured phase. It is a high-water mark, not steady-state RSS.
- `db_kept`: whether the scratch database was retained with `--keep-db`.
- `path`: logical scratch path used for the run; it may no longer exist when `db_kept=false`.

## Aggregation

`scripts/summarize.py` groups exact-compatible configurations and reports median throughput, IQR, median p99 latency, database size, peak RSS and prefill time across independent trials. It does not average unlike workload shapes or create uncertainty bands from adjacent database sizes.

## Historical schema v2 additions — 2026-10-04

At that stage, new results used format_version 2.

Identity now includes scenario, allowing the core matrix, size sweeps, reopen lanes and recovery lanes to coexist without accidental aggregation. The summarizer also keys on ops_requested; unlike operation counts are never silently grouped.

Open/reopen fields:

- open_s: database open/recovery time before prefill.
- reused_db: this invocation opened a pre-existing benchmark database.
- prefill_skipped: no population phase ran in this invocation.
- warmup_reads: requested deterministic benchmark warmup count.

Measured-process resource object measured_process is a delta around the measured workload only:

- cpu_runtime_ns, runqueue_wait_ns, timeslices
- minflt, majflt
- voluntary/involuntary context switches
- rchar, wchar, syscr, syscw
- kernel-accounted read_bytes, write_bytes, cancelled_write_bytes
- RSS and thread count before/after the measured interval
- CPU-runtime and runqueue-wait fractions of measured wall time

system_before, system_after and measured_system_delta record load/memory snapshots plus PSI, page-fault, swap and reclaim counters. These are shared-host interference/context metrics, not process attribution.

Crash-recovery verification runs may include verification:

- expected/checked acknowledged prefix size and missing-prefix count;
- number of tail records inspected;
- contiguous unreported tail length, total tail records found and records found after the first gap;
- whether the recovered tail preserves transaction-size atomicity;
- aggregate verification_ok;
- verification wall time.

Wide KV campaigns also write one sidecar per case under device/ with raw whole-device block-stat counters before and after that case. Whole-device deltas must not be reported as process I/O on a shared host.


## Historical schema v3 KV additions — 2026-10-04

Raw-KV results now use `format_version: 3`. The record/database lane remains schema v2 because these new byte-KV shape controls do not apply there.

Additional identity/configuration fields are part of the aggregation key:

- `key_bytes`: exact generated key length. Minimum 8 bytes.
- `key_shape`:
  - `sequential`: ordered 64-bit ID prefix plus deterministic suffix;
  - `shared-prefix`: constant prefix with the ordered 64-bit ID in the final 8 bytes;
  - `hashed`: deterministic one-to-one 64-bit permutation plus deterministic suffix. This deliberately destroys ID order and is not valid for the ID-ordered range-scan workload.
- `value_pattern`:
  - `pseudo-random`: high-entropy deterministic bytes;
  - `zeros`: maximally compressible zero-filled values;
  - `repeated`: one deterministic 8-byte block repeated across the value.
- `access_pattern`:
  - `auto`: preserves the historical core behavior (uniform point-read/churn, 80/20 hot-set behavior for read-heavy/balanced);
  - `uniform`;
  - `hot80`: 80% of accesses target a 20% hot set;
  - `hot95`: 95% of accesses target a 5% hot set.
- `miss_percent`: requested percentage of reads aimed at deterministic keys outside the populated/written ID space.
- `write_pattern` for tiny-txn/write-burst:
  - `append`: new monotonically allocated IDs;
  - `update-uniform`: updates uniformly sampled existing IDs;
  - `update-hot`: updates an existing hot set using the configured locality, defaulting to 80/20 when access_pattern is auto/uniform.

`delete-burst` is a new workload that deletes a unique sequential subset of the populated keys in transaction-sized batches. It intentionally rejects `ops > records` so a tombstone/reclamation run cannot silently become a repeated-delete benchmark.

### Post-workload settle/deferred-work object

When `settle_ms > 0`, `post_workload_settle` measures background/deferred work **after foreground throughput timing stops but before the engine is closed**:

- `requested_ms`, `sample_ms`, `elapsed_s`;
- `db_bytes_before`, `db_bytes_after`;
- aggregate process delta for the whole settle window;
- aggregate shared-host system delta for the whole settle window;
- `samples[]`, each with elapsed/interval milliseconds, current on-disk size, process delta and system delta.

This lane is intended to expose deferred compaction/checkpoint/writeback cost without charging it to foreground throughput. A fast foreground result followed by substantial settle CPU/write traffic is therefore visible rather than hidden.

The summarizer groups on all schema-v3 identity fields and reports median settle CPU time, process-attributed settle write bytes and settle-window DB-size change where present.


## Crash-result interpretation

verification.verification_ok is a measured property, not universally an assertion that every durability mode promises the same acknowledgement boundary.

- In the **sync** lane, verification_ok=false is a hard campaign failure.
- In **relaxed/background** lanes, missing externally acknowledged records can be an expected consequence of the documented durability contract. The crash runner retains the JSON result and additionally writes such cases to relaxed-ack-losses.ndjson.
- Reopen/process errors are still hard failures in all lanes.
- Transaction holes or partial recovered batches remain consistency failures; durability mode does not excuse structural corruption.


## Historical schema v4 process-accounting correction — 2026-10-04

All newly generated raw-KV and record/database results use format_version 4. The lane-specific configuration fields introduced by v2/v3 remain unchanged; v4 exists because process CPU accounting changed materially.

Process snapshots now sum /proc/self/task/*/{schedstat,stat,status} across every live thread at each measurement boundary instead of reading only the thread-group leader. This captures persistent database background workers and multi-thread runtime work in CPU runtime, runqueue wait, page faults, context switches and thread counts.

A thread created and destroyed entirely between the two snapshots cannot be reconstructed from procfs, so these counters remain a lower bound for extremely short-lived worker threads. Process /proc/self/io remains process-wide.

The summarizer includes format_version in its exact-match aggregation key and prints it in Markdown output. Results from old schemas therefore cannot be silently combined with v4 even when every workload field matches.


## KV concurrency lane — schema v5

kvconcurrency writes the same schema-v5 process/system resource objects as the raw-KV lane, with lane: "kv-concurrency" and additional concurrency identity/diagnostic fields:

- clients: simultaneous client count; this is part of the exact aggregation key.
- ops_requested: total logical work across all clients, held fixed when calculating scaling.
- concurrency_handle: human-readable native sharing strategy (Arc/shared handle, native clones, or sled per-client clones).
- client_measurements[]: per-client requested/completed operations, elapsed time, throughput, read/write/delete counts and HDR read/write-transaction latency quantiles.
- client_elapsed_s_min/median/max.
- client_ops_per_s_min/median/max.
- client_throughput_max_min_ratio: a simple fairness/straggler indicator; 1.0 is perfectly even, larger is less even.
- write_conflict_retries: total transparent optimistic-write retries across all clients. SurrealKV TransactionWriteConflict/TransactionRetry are retried with the same logical write set; successful-operation and latency accounting includes those attempts.
- client_measurements[].write_conflict_retries: the corresponding per-client retry count.

The coordinator records foreground wall time from the synchronized start barrier until all clients report completion. Client threads are kept alive until the process-after snapshot is captured so per-thread scheduler accounting does not lose completed-client TIDs.

The summarizer keeps client count separate and, when a matching 1-client row exists, adds:

- speedup_vs_c1;
- parallel_efficiency_vs_c1 = speedup / clients;
- p99_read_multiplier_vs_c1;
- p99_write_txn_multiplier_vs_c1;
- cpu_cores_median from process CPU runtime / wall time;
- client_throughput_max_min_ratio_median.
- write_conflict_retries_median and write_conflict_retries_per_k_write_ops_median.

A 1-client concurrency result is the baseline for these ratios. It is not silently substituted with a row from the ordinary kv lane because the concurrency binary has different thread/barrier/runtime mechanics.


## Schema v5 CPU-accounting precision correction — 2026-10-04

All newly generated raw-KV, record/database and KV-concurrency results use format_version 5.

Schema v4 introduced per-TID scheduler accounting but reported cpu_runtime_ns as max(per-TID schedstat delta, process-wide /proc/self/stat utime+stime converted from USER_HZ ticks). The latter is monotonic but coarse (typically 10 ms per tick); on millisecond-scale smoke cases it could therefore imply physically impossible CPU-core counts.

In v5:

- measured_process.cpu_runtime_ns is the sum of nanosecond-resolution schedstat CPU deltas matched by TID across the measurement boundaries;
- process_cpu_tick_runtime_ns exposes the coarse process-wide utime+stime delta only as a diagnostic;
- process_cpu_tick_minus_task_ns exposes its positive difference from the schedstat total, useful for spotting possible CPU from threads that were created and destroyed entirely between snapshots, but it must not be treated as exact unattributed CPU because tick quantization is coarse;
- cpu_runtime_fraction_of_wall and all CPU-per-operation summaries use the schedstat value only.
- measured_process.accounting_wall_ns is the monotonic time between completed process snapshots and is the denominator for CPU/runqueue fractions; measured_system_delta.accounting_wall_ns analogously brackets the system snapshots and is used for PSI fractions. Foreground elapsed_s remains the barrier-to-completion throughput timer.

Concurrency client threads are intentionally kept alive through the after-snapshot, so their TIDs are present at both boundaries. Persistent engine/runtime workers are covered the same way. Extremely short-lived transient workers can still be undercounted; the process-tick diagnostic exists to make that risk visible rather than silently biasing the primary metric upward.
