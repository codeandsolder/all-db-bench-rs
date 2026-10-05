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


if __name__ == "__main__":
    unittest.main()
