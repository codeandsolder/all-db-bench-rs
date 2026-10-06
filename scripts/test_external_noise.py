from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("check-external-noise.py")
SPEC = importlib.util.spec_from_file_location("check_external_noise", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)
ProcessRow = MODULE.ProcessRow
classify_process = MODULE.classify_process
descendant_pids = MODULE.descendant_pids
low_priority_sccache_tree = MODULE.low_priority_sccache_tree
aggregate_foreign_cpu_percent = MODULE.aggregate_foreign_cpu_percent
cpu_percent_from_samples = MODULE.cpu_percent_from_samples


class ExternalNoiseTests(unittest.TestCase):
    def row(self, *, nice: int = 0, cpu: float = 0.0, comm: str = "thing", args: str = "thing"):
        return ProcessRow(123, 1, nice, cpu, comm, args)

    def test_ignores_low_priority_compiler(self) -> None:
        self.assertIsNone(classify_process(self.row(nice=19, cpu=95, comm="rustc", args="rustc crate.rs")))

    def test_flags_normal_priority_compiler(self) -> None:
        self.assertEqual(classify_process(self.row(cpu=5, comm="clippy-driver", args="clippy-driver rustc src/lib.rs")), "compiler-or-build")

    def test_flags_cargo_subcommand_frontend(self) -> None:
        self.assertEqual(classify_process(self.row(cpu=5, comm="cargo-clippy", args="/root/.rustup/toolchains/nightly/bin/cargo-clippy clippy --fix")), "compiler-or-build")

    def test_flags_wide_filesystem_scan(self) -> None:
        self.assertEqual(classify_process(self.row(cpu=2, comm="find", args="find /root /srv /opt /home /mnt -type f")), "wide-filesystem-scan")

    def test_flags_unknown_high_cpu_process(self) -> None:
        self.assertEqual(classify_process(self.row(cpu=75, comm="python3", args="python3 worker.py")), "foreign-high-cpu")

    def test_ignores_small_daemon_activity(self) -> None:
        self.assertIsNone(classify_process(self.row(cpu=3, comm="tailscaled", args="/usr/sbin/tailscaled")))

    def test_allows_benchmark_process(self) -> None:
        self.assertIsNone(classify_process(self.row(cpu=100, comm="kvbench", args="/tmp/kvbench --engine redb")))

    def test_excludes_helper_descendant_tree(self) -> None:
        rows = [
            ProcessRow(200, 100, 0, 100.0, "ps", "ps -eo pid=,ppid=,ni=,pcpu=,comm=,args="),
            ProcessRow(201, 200, 0, 100.0, "helper", "helper child"),
            ProcessRow(300, 1, 0, 100.0, "python3", "python3 foreign.py"),
        ]
        self.assertEqual(descendant_pids(rows, {100}), {100, 200, 201})

    def test_excludes_only_low_priority_sccache_worker_tree(self) -> None:
        rows = [
            ProcessRow(400, 1, 10, 20.0, "sccache-dist", "/usr/local/bin/sccache-dist server --config /etc/sccache-dist/server.conf"),
            ProcessRow(401, 400, 10, 100.0, "cc1plus", "/usr/lib/gcc/cc1plus file.cc"),
            ProcessRow(500, 1, 0, 5.0, "cargo", "cargo check --workspace"),
        ]
        self.assertEqual(low_priority_sccache_tree(rows), {400, 401})

    def test_aggregate_cpu_catches_many_medium_foreign_workers(self) -> None:
        rows = [
            ProcessRow(800, 1, 0, 20.0, "python", "python worker-a.py"),
            ProcessRow(801, 1, 0, 20.0, "python", "python worker-b.py"),
            ProcessRow(802, 1, 0, 20.0, "python", "python worker-c.py"),
            ProcessRow(803, 1, 19, 95.0, "python", "python low-priority.py"),
            ProcessRow(804, 1, 0, 100.0, "kvbench", "/tmp/kvbench --engine redb"),
        ]
        self.assertEqual(aggregate_foreign_cpu_percent(rows, excluded_pids=set()), 60.0)


    def test_cpu_percent_uses_interval_delta_not_lifetime_average(self) -> None:
        self.assertAlmostEqual(
            cpu_percent_from_samples((10_000, 123), (10_025, 123), elapsed_s=0.25, uptime_s=1000.0, ticks_per_second=100),
            100.0,
        )

    def test_cpu_percent_handles_process_born_during_sample(self) -> None:
        self.assertAlmostEqual(
            cpu_percent_from_samples(None, (25, 99_975), elapsed_s=0.25, uptime_s=1000.0, ticks_per_second=100),
            100.0,
        )

    def test_explicit_exclusion_can_cover_a_server_process_tree(self) -> None:
        rows = [
            ProcessRow(600, 100, 0, 75.0, "prometheus", "/tmp/prometheus --web.enable-remote-write-receiver"),
            ProcessRow(601, 600, 0, 80.0, "worker", "server helper"),
            ProcessRow(700, 1, 0, 75.0, "python3", "python3 foreign.py"),
        ]
        self.assertEqual(descendant_pids(rows, {600}), {600, 601})


if __name__ == "__main__":
    unittest.main()
