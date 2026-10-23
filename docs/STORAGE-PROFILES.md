# Storage profiles and compression/storage interaction

The benchmark treats storage as an explicit experimental dimension rather than assuming that a sufficiently idle host is reproducible.

Quiet-host admission is still useful: it rejects unrelated CPU and I/O activity. It is not a substitute for a storage contract. A publication result that depends on the backing device, filesystem, cache, compression, or host resource allocation must bind those parameters into its provenance.

## Result classes

Each relevant storage profile can produce two distinct result classes.

### Native stack

The native lane preserves the real device and filesystem behavior. It is the lane for effects such as device latency/queueing, filesystem allocation and journaling, copy-on-write, engine/filesystem compression, page cache or ARC, and deferred writeback/batching.

A native result is valid only for the recorded host + storage profile. It is not normalized into a fictional generic SSD/HDD.

### Bounded stack

The bounded lane uses the same storage stack but executes the complete campaign inside an immutable systemd/cgroup resource envelope. The envelope can bind allowed CPUs and CPU quota, memory maximum, swap maximum, and block-device read/write bandwidth + IOPS ceilings.

The complete profile manifest and its SHA-256 are exported to the campaign and must be retained with result provenance.

This lane is intended for repeatability when a VM or backing device changes underneath us. It should not replace the native lane: a ceiling can hide meaningful media behavior.

## Initial physical profiles

### Acer: ZFS + compressed SATA SSD

Current physical stack:

- Micron 1100 SATA SSD;
- ZFS `rpool/srv/scratch`;
- `compression=zstd`;
- `recordsize=128K`;
- `atime=off`;
- `sync=standard`;
- `logbias=latency`;
- `primarycache=all`;
- `secondarycache=all`;
- `dnodesize=auto`;
- `xattr=sa`.

Use dedicated child datasets for storage-profile publication runs. Do not change the active scratch dataset in place.

The compression campaign should pair otherwise-identical child datasets with `compression=off` and `compression=zstd` so engine compression and filesystem compression can be separated. Additional recordsize/cache-policy variants are separate profiles, not hidden tuning.

ARC is host-global and is not equivalent to the Linux page cache. Do not claim perfect cross-filesystem cache normalization. Use explicit protocols instead: dedicated/fresh datasets where a cold data cache is required; `primarycache=metadata` for a reproducible no-data-ARC profile; `primarycache=all` plus a declared prewarm pass for a warm profile; record ARC counters before/after every run.

### cold-storage: ext4 NVMe-backed root tier

Current host has four vCPUs (AMD EPYC 9655 under KVM). `/dev/sda3` is ext4 and had about 138 GiB free on 2026-10-23.

A reconnaissance direct-I/O probe measured approximately 1.35 GiB/s sequential write, 1.77 GiB/s sequential read, 117k 4 KiB QD16 random-read IOPS and 87k 4 KiB QD16 random-write IOPS. These are not publication calibration.

### cold-storage: ext4 HDD/scratch tier

`/dev/sdc` is a separate 2 TiB ext4 volume mounted at `/srv/scratch` with `noatime`; about 660 GiB was free on 2026-10-23.

A reconnaissance 64 MiB direct sequential write with fsync-on-close remained blocked in `jbd2_log_wait_commit` for about 48 seconds and drove I/O PSI close to saturation before the probe was terminated. Do not smooth this away by calling the device a generic HDD: pathological synchronous-commit behavior is itself relevant to databases.

Run the full fio calibration with a generous timeout before freezing any bounded HDD envelope.

## Cross-host interpretation

cold-storage NVMe versus cold-storage HDD is the clean media comparison because CPU and VM environment are shared.

Acer ZFS versus cold-storage ext4 is a deployment-stack comparison, not an apples-to-apples filesystem comparison: CPU microarchitecture, host kernel environment and physical media differ. Report per-profile engine rankings and interactions rather than calling the absolute difference “ZFS overhead”.

Within Acer, ZFS compression/cache/recordsize variants are directly comparable because the underlying host and SSD are shared.

## Profile manifest

`run-storage-envelope.sh` accepts a JSON profile. Version 1 has this shape:

```json
{
  "format_version": 1,
  "id": "example-bounded-v1",
  "mode": "bounded",
  "root": "/dedicated/storage/profile/root",
  "expected": {
    "hostname": "host.example",
    "filesystem": "ext4",
    "source": "/dev/nvme0n1p1",
    "mount_options": ["rw", "noatime"],
    "zfs": {
      "compression": "zstd",
      "recordsize": "128K"
    }
  },
  "limits": {
    "allowed_cpus": "0-3",
    "cpu_quota_percent": 400,
    "memory_max_bytes": 8589934592,
    "memory_swap_max_bytes": 0,
    "io": {
      "device": "/dev/nvme0n1",
      "read_bps": 1073741824,
      "write_bps": 1073741824,
      "read_iops": 80000,
      "write_iops": 60000
    }
  }
}
```

The profile root is the storage base for database payloads. When a campaign is wrapped by `run-storage-envelope.sh`, storage-bearing runners place case data under `root/data/runs/RUN_ID` while results/provenance stay under the canonical source checkout. This separation is deliberate: moving a benchmark checkout must not silently move the database to a different filesystem.

`capture-host-metadata.sh` follows the same storage root when recording filesystem, block-device, ZFS and ARC metadata. The source checkout remains the source of Git/toolchain provenance.

`expected.zfs` is optional and is validated only on a ZFS profile. Mount options are treated as required members, not an exact option string. A native profile uses `"mode": "native"` and may omit `limits`.

Do not choose bounded ceilings from vendor labels. First run the storage calibration, inspect latency/throughput stability, then freeze conservative exact numbers into a versioned manifest. A profile change creates a new profile ID.

## Compression effectiveness/cost

Compression is a separate campaign layered across storage profiles. At minimum record logical application bytes, apparent database bytes, allocated/physical database bytes, filesystem logical and physical usage where available, physical device read/write bytes, engine and filesystem compression configuration, CPU time/ns per logical operation, throughput/latency distributions, and post-workload settle/compaction size and CPU/I/O debt.

For ZFS additionally retain dataset `logicalused`, `used`, `compressratio` and ARC deltas.

The data-shape matrix should cover deterministic high-entropy bytes; zero/repeated data; moderately compressible structured binary; structured text/document records with repeated schema; TSDB smooth versus noisy values; low-cardinality/stable versus high-cardinality/churning labels; and regular versus jittered timestamps where relevant.

Prometheus Remote Write should record protobuf bytes before Snappy, compressed request bytes after Snappy, protobuf encoding CPU and Snappy CPU separately.

## Why the existing quiet-host gate remains

The bounded lane controls resources available to the benchmark. It does not prevent unrelated work on the same physical device or host from consuming latency budget underneath those ceilings. Existing pre/continuous/post noise admission therefore remains useful and should be retained.

The two mechanisms answer different questions:

- resource envelope: what was the benchmark allowed to use?
- noise admission: was something else materially stealing the resource?
