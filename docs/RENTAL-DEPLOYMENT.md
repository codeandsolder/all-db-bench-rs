# Vast / SaladCloud container deployment

The container runtime executes the **existing** raw-KV, full-record and
external TSDB baseline matrices. It is not a new benchmark or an account-level
Vast/Salad provisioner. All result families remain separate; a container
result must not be compared to bare-metal storage results as if the
resources and filesystem were identical.

## Supported surface

| Lane | Rental entrypoint | Preconditions |
| --- | --- | --- |
| KV (smoke, quick, full) | Yes | Sealed KV and benchctl binaries |
| Record (smoke, quick, full) | Yes | Sealed record + isolated Rocks binaries |
| External TSDB (smoke, quick, full) | Yes, optional bundle | All four pinned official server distributions |
| Wide, dimensional, concurrency, sustained, compression | Existing independent runners, not yet part of rental entrypoint | Declare storage/CPU allocation and build additional bins |
| fio, warm reopen, crash recovery, CPU/I/O contention | Existing independent runners, not yet part of rental entrypoint | Per-lane calibration and isolation |
| Cold-cache reopen, cgroup memory limitation, dm-log-writes power loss, bounded storage envelopes | Host-only | Requires privileged host access unavailable in ordinary provider containers |

## Build a reproducible image

Build from a clean, committed checkout with Rust 1.99.1, healthy managed
sccache, native sccache wrappers, and the required system toolchains.
No build path silently bypasses sccache.

    ./deploy/rental/build.sh
    ./deploy/rental/package.sh --output "$PWD/target/rental-bundle"
    docker build -f deploy/rental/Dockerfile -t all-db-bench:COMMIT target/rental-bundle

Use `--with-tsdb` for both build and packaging when TSDB is included.
The build uses a common `x86-64-v2+aes` target (AES-NI is required by gxhash) for Rust,
C and C++; it does not ship Acer's `target-cpu=native` binaries to
unrelated machines. It strips debug symbols but does not change benchmark
logic. Never merge results built for different ISA policies into the same
comparison without explicitly recording the build variant.

The record binary is linked against verified SQLite 3.54.0 at build time; the rental manifest records its static linkage and the record runner does not attempt to rebuild SQLite inside the image. This is only enabled after validating the image's pinned manifest.

The package contains a pinned, clean Git checkout, release executables
and a SHA-256 manifest. The image starts no Cargo compilation, runs
as an unprivileged user and stores DB data in `/work/data/runs/`,
result data in `/work/results/`. The manifest records builder glibc;
preflight detects missing dynamic libraries. The default image base uses
an up-to-date Arch Linux userland to accommodate binaries built on a
recent glibc. Before publishing, actually test the image with the
packaged binaries; toolchain ABI and ISA support are independent.

## Invocation and persistence

For a local smoke probe with a host-mounted work directory:

    docker run --rm \
      -e RENTAL_PROVIDER=local \
      -e RENTAL_INSTANCE_ID=demo-001 \
      -e RENTAL_RUN_ID=demo-001 \
      -e RENTAL_RESULTS_PERSISTED=1 \
      -e RENTAL_MAX_SECONDS=1800 \
      -v all-db-bench-rental-work:/work \
      all-db-bench:COMMIT --profile smoke --lanes kv,record

To inspect a prepared image without executing a database:

    docker run --rm all-db-bench:COMMIT --preflight-only --profile quick --lanes kv,record

A rental that has no persistent, externally collected work volume MUST
supply `RENTAL_RESULTS_PUT_URL` (a presigned HTTPS PUT URL) instead of
`RENTAL_RESULTS_PERSISTED=1`. A compressed results-only archive is
uploaded at each completed lane and on exit; it contains status, case
results, logs, metadata and summaries, *not* transient databases.
The URL must be valid through the deadline, permit overwrites to the
same object and be distinct per run. The URL is never logged by the
entrypoint. Retain it only in the provider's temporary environment.

The runner writes an atomic status object under
`/work/results/rental/RUN_ID/status.json`. Case results preserve their
existing per-case resumability, but resume is safe only on the same
persisted filesystem, provider instance, binaries, source SHA, and
storage profile. A different rental gets a **new** run ID.

Supply `RENTAL_DEADLINE_UNIX` for a real rental, or
`RENTAL_MAX_SECONDS` (default eight hours) for a local timebox.
Each lane has a timeout and a TERM/KILL grace interval.
This is *not* an instance billing/expiry controller:
the provisioner MUST separately destroy instances after the deadline
and prevent orphan bills when image startup never completes.

## Provider-specific behavior

**Vast.ai:** the default entrypoint works with Vast's Entrypoint / args
launch mode. Vast SSH/Jupyter launch modes replace image ENTRYPOINT,
so explicitly invoke `deploy/rental/run.sh` from your SSH/onstart
procedure in those modes. Specify `RENTAL_PROVIDER=vast` and a unique
`RENTAL_INSTANCE_ID`, for example the allocation's container label.
Vast apportions CPU and RAM approximately to rented GPU share; idle
bursting does not imply a guaranteed exclusive CPU. Disk size is
fixed at instance creation. Choose storage/CPU/RAM from the offer, not
GPU name alone.

**SaladCloud:** use the same image with its container-group entrypoint,
set `RENTAL_PROVIDER=salad` and a **per-instance** identifier
(not the shared group name). Provide a presigned result destination
and deadline. Treat reallocation as loss of local scratch and give a
new instance a new run ID. Each replica needs an independent results
key, and the host's effective CPU/storage may change at replacement.

## Measurement and correctness contract

The runtime checks executable SHA-256 against its manifest, verifies
the actual Git checkout source SHA and clean tracked tree, checks
writable storage, the cgroup memory limit and free-disk floors.
Minimum free disk: 2/10/30 GiB (smoke/quick/full). Minimum cgroup
memory: 1/4/8 GiB. These are admission floors, not guarantees that
every large workload fits.

Rental identity is hashed from provider+instance rather than relying on
image-global `/etc/machine-id`. Captured metadata includes cgroup
memory/CPU limits, storage mount, kernel, pressure and tool versions.
All regular pre/continuous/post noise checks are retained. Do not
set `ALLOW_BUSY=1` or `ALLOW_EXTERNAL_NOISE=1` to make benchmark
numbers appear on a noisy rented machine. A container's PID namespace
may hide host work, so an accepted case is not proof of single tenancy.

## Remaining rollout gates

1. Successfully build the generic full release set through sccache.
2. Build and run the actual OCI image against packaged binaries;
   a shell test alone does not prove image compatibility.
3. Run KV + record + TSDB smoke on a real rental, inspect summaries
   and verify the durable upload.
4. Exercise TERM, upload failure, provider expiry/cleanup and
   same-instance per-case resume.
5. Define storage and privilege contracts before including the other
   matrices in the rental entrypoint; full exhaustive campaigns could
   take weeks. Integrate the image with the existing Vast vtest broker
   or a SaladCloud group controller for actual provisioning.

Upstream references:
- Vast execution: https://docs.vast.ai/guides/instances/docker-environment
- Vast lifecycle: https://docs.vast.ai/guides/reference/faq/instances
- SaladCloud API: https://docs.salad.com/reference/saladcloud-api/container-groups/reallocate-container-group-instance
