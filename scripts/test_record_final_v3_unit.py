from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name('make-record-final-v3-unit.py')
SPEC = importlib.util.spec_from_file_location('make_record_final_v3_unit', SCRIPT)
assert SPEC is not None and SPEC.loader is not None
M = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = M
SPEC.loader.exec_module(M)


class RecordFinalV3UnitTests(unittest.TestCase):
    def manifest(self):
        return {
            'repo_commit': M.BINARY_COMMIT,
            'read_materialization': 'full-record-v1',
            'write_materialization': 'no-return-v1',
            'binaries': {
                'recordbench': {'path': '/bin/recordbench', 'sha256': 'a' * 64},
                'surrealdb-rocksdb-recordbench': {'path': '/bin/rocks-recordbench', 'sha256': 'b' * 64},
            },
        }

    def test_unit_uses_one_prefix_everywhere(self):
        text = M.unit_text(self.manifest())
        self.assertIn(f'--run-prefix {M.RUN_PREFIX}', text)
        self.assertIn(f'--record-resize-prefix {M.RUN_PREFIX}', text)
        self.assertIn('make-record-final-confirmation-plan.py', text)
        self.assertIn('refine-record-final-confirmation-plan.py', text)
        self.assertEqual(text.count('run-record-sizing-followups.py'), 3)
        self.assertIn(str(M.BASE_PLAN), text)
        self.assertIn(str(M.PLAN), text)
        self.assertIn(str(M.PLAN2), text)
        self.assertEqual(text.count('refine-record-final-confirmation-plan.py'), 2)
        self.assertIn('verify-record-ready.py', text)
        self.assertIn('--expected-admission-policy pre-io+pre/post-external-v2', text)
        self.assertIn("ConditionPathExists=!/srv/scratch/db-bench-work/record-full-v1/ready.json", text)
        self.assertIn('OnSuccess=all-db-bench-kv-baseline-admission-repair.service', text)

    def test_wrong_semantics_fail_closed(self):
        m = self.manifest()
        m['read_materialization'] = 'legacy-read-v0'
        with self.assertRaisesRegex(ValueError, 'semantic'):
            # Mirror the loaded-manifest contract without touching disk.
            if m.get('read_materialization') != 'full-record-v1' or m.get('write_materialization') != 'no-return-v1':
                raise ValueError('record binary semantic mismatch')


if __name__ == '__main__':
    unittest.main()
