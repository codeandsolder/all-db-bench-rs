from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]
RUNNER = (ROOT / "scripts/run-record-matrix.sh").read_text()
WIDE_RUNNER = (ROOT / "scripts/run-record-wide-matrix.sh").read_text()


class RecordMatrixSemanticTests(unittest.TestCase):
    def test_support_and_case_acceptance_require_current_semantics(self) -> None:
        self.assertIn("READ_MATERIALIZATION=full-record-v1", RUNNER)
        self.assertIn("WRITE_MATERIALIZATION=no-return-v1", RUNNER)
        self.assertIn('"read_materialization":"$READ_MATERIALIZATION"', RUNNER)
        self.assertIn('"write_materialization":"$WRITE_MATERIALIZATION"', RUNNER)
        self.assertIn('.read_materialization == $read_expected', RUNNER)
        self.assertIn('.write_materialization == $write_expected', RUNNER)
        self.assertIn("record result semantic identity mismatch", RUNNER)

    def test_wide_matrix_rejects_wrong_semantics(self) -> None:
        self.assertIn("READ_MATERIALIZATION=full-record-v1", WIDE_RUNNER)
        self.assertIn("WRITE_MATERIALIZATION=no-return-v1", WIDE_RUNNER)
        self.assertIn('.read_materialization == $read_expected', WIDE_RUNNER)
        self.assertIn('.write_materialization == $write_expected', WIDE_RUNNER)
        self.assertIn("record result semantic identity mismatch", WIDE_RUNNER)


if __name__ == "__main__":
    unittest.main()
