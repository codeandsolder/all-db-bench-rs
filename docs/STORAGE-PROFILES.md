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

### Asus: controlled ZFS + Samsung NVMe

The ZFS publication/compression lane runs on Asus (`cachyasus`), not Acer. Asus has 16 logical CPUs, about 30.8 GiB RAM, and a Samsung MZVLQ1T0HALB 1 TB NVMe. The `zpcachyos` pool had about 745 GiB free when the profiles were created. This host is preferred because it has substantially less active development load than Acer and a faster local drive.

The benchmark uses dedicated sibling datasets under `zpcachyos/dbbench`; it never changes the active root/home dataset properties in place. The initial controlled matrix is:

- `off-all`: `compression=off`, `primarycache=all`;
- `lz4-all`: `compression=lz4`, `primarycache=all` (closest to the host's normal root policy);
- `zstd-all`: `compression=zstd`, `primarycache=all`;
- `zstd-metadata`: `compression=zstd`, `primarycache=metadata` to remove data ARC from the measured stack while retaining metadata caching.

All four datasets explicitly use `recordsize=128K`, `atime=off`, `sync=standard`, `logbias=latency`, `secondarycache=all`, `xattr=sa`, `dnodesize=legacy`, and a 120 GiB `refquota`. Compression/cache/recordsize variants are separate profiles rather than hidden tuning.

ARC is host-global and is not equivalent to the Linux page cache. Do not claim perfect cross-filesystem cache normalization. `primarycache=metadata` is the reproducible no-data-ARC condition; `primarycache=all` is the native warm-ARC condition and should use a declared prewarm/cold-start protocol as appropriate. Record ARC and ZFS pool counters before/after every case.

A direct validation showed that Linux blk-cgroup bandwidth limits do apply through this OpenZFS stack on the Samsung NVMe: an uncapped 512 MiB 1 MiB-write probe reached about 448.5 MB/s, while the same operation inside a 32 MiB/s `IOWriteBandwidthMax` cgroup reached about 32.85 MB/s. Bounded ZFS profiles are therefore meaningful on this host; nevertheless, native profiles remain required because a cap can hide queueing, CoW, ARC, TXG, and device behavior.

The initial native calibration on `off-all` measured about 814 MB/s 128 KiB QD1 direct reads and 115 MB/s writes. Small-I/O scaling is intentionally not idealized away: 4 KiB QD1 was about 3.41k read / 960 write IOPS, while QD16 was only about 3.17k / 1.26k IOPS and developed multi-millisecond latency with long tails. A 4 KiB `fdatasync` workload sustained about 238 writes/s (sync p50 ~2.67 ms, p99 ~20.1 ms, max ~301 ms). This is a useful ZFS/recordsize/CoW/TXG characteristic, not a defect in the benchmark.

The bounded v1 ZFS contract uses eight CPUs, 16 GiB RAM, no swap, 512 MiB/s read, 96 MiB/s write, 3k read IOPS and 900 write IOPS on the underlying NVMe partition. These are physical-I/O ceilings, not logical database-operation targets; ZFS aggregation and writeback remain part of the stack and delivered rates are still recorded.

### cold-storage: ext4 NVMe-backed root tier

Current host has four vCPUs (AMD EPYC 9655 under KVM). `/dev/sda3` is ext4 and had about 138 GiB free on 2026-10-23.

The 2026-10-23 calibration measured about 3.03 GB/s sequential read, 0.97 GB/s sequential write, 9.0k/28.6k 4 KiB QD1 read/write IOPS, and 97.2k/83.6k QD16 read/write IOPS. The 4 KiB `fdatasync` path sustained about 3.38k writes/s, with sync p99 about 0.50 ms, p99.9 about 14.1 ms, and a maximum observed sync latency around 660 ms. QEMU still advertises the device as rotational, write-back, and without FUA; the profile therefore records it as the observed fast root tier rather than asserting physical-media identity.

The bounded v1 contract caps the tier at 1 GiB/s read, 768 MiB/s write, 80k read IOPS and 60k write IOPS, with four CPUs, 8 GiB RAM and no swap.

### cold-storage: ext4 HDD/scratch tier

`/dev/sdc` is a separate 2 TiB ext4 volume mounted at `/srv/scratch` with `noatime`; about 660 GiB was free on 2026-10-23.

The 2026-10-23 calibration exposed a split personality. Direct I/O looked implausibly fast for bare rotational media (about 6.44 GB/s sequential read, 4.63 GB/s sequential write and 104k/74.8k QD16 read/write IOPS), while the first 4 KiB `fdatasync` took about **50.17 seconds**. The guest sees a rotational, write-back, no-FUA QEMU device. Treat this as a virtual rotational/cache stack: host-side caching can make ordinary direct I/O look spectacular while FLUSH finally pays the durability debt.

The native profile preserves that behavior. A separate bounded HDD-class profile caps bandwidth at 180 MiB/s and random IOPS at 200 read / 150 write, but deliberately does **not** claim to simulate seek distributions and does not hide the underlying FLUSH pathology.

## Cross-host interpretation

cold-storage NVMe versus cold-storage HDD is the clean media comparison because CPU and VM environment are shared.

Asus ZFS versus cold-storage ext4 is a deployment-stack comparison, not an apples-to-apples filesystem comparison: CPU microarchitecture, host kernel environment and physical media differ. Report per-profile engine rankings and interactions rather than calling the absolute difference “ZFS overhead”.

Within Asus, ZFS compression/cache/recordsize variants are directly comparable because the underlying host and SSD are shared.

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

ZFS dataset accounting is sampled only after an explicit pool sync: each case syncs before its baseline snapshot, again after engine close before physical/dataset counters are captured, and again after deleting the case before the next baseline. The foreground throughput/latency interval excludes these syncs. This prevents TXG-lagged `used/logicalused` values and deferred frees from leaking across case boundaries. `stat.st_blocks` is retained only as diagnostic filesystem accounting and is not interpreted as physical allocation on ZFS.

The data-shape matrix should cover deterministic high-entropy bytes; zero/repeated data; moderately compressible structured binary; structured text/document records with repeated schema; TSDB smooth versus noisy values; low-cardinality/stable versus high-cardinality/churning labels; and regular versus jittered timestamps where relevant.

Prometheus Remote Write should record protobuf bytes before Snappy, compressed request bytes after Snappy, protobuf encoding CPU and Snappy CPU separately.

## Admission under native and bounded profiles

The old global `/proc/pressure/io full avg10 <= 5%` pre-admission rule remains appropriate for **native/unbounded** publication runs. It is no longer a precondition for a **bounded** profile.

That distinction is deliberate. Idle-priority background I/O can raise global PSI before the benchmark exists; refusing to start until PSI falls can then deadlock forever because the foreground workload never appears to make idle I/O yield. Under a bounded profile, free-space checks and pre/continuous/post foreign-process noise evidence remain active, the resource envelope is fixed before the workload starts, and global PSI is retained as measurement evidence rather than an admission veto.

A bounded profile therefore does not mean “ignore contention”. Normal-priority foreign work can still invalidate a case through the continuous noise guard. It means that global storage pressure by itself is no longer confused with an uncontrolled benchmark environment.

The mechanisms answer different questions:

- resource envelope: what was the benchmark allowed to use?
- foreign-work guard: was unrelated normal-priority work materially stealing the resource?
- PSI and storage counters: what pressure did the complete stack actually experience?
