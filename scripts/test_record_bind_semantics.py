from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]
SOURCE = (ROOT / "src/bin/recordbench.rs").read_text()
ROCKS_SOURCE = (ROOT / "engines/surrealdb-rocksdb/src/main.rs").read_text()


class RecordBindSemanticsTests(unittest.TestCase):
    def test_turso_integer_keys_are_bound_as_integers(self) -> None:
        self.assertNotIn("id.to_string()", SOURCE)
        self.assertNotIn("group.to_string()", SOURCE)
        self.assertNotIn("data.bucket.to_string()", SOURCE)
        self.assertIn("stmt.query((id as i64,)).await?", SOURCE)
        self.assertIn("stmt.query((group as i64,)).await?", SOURCE)
        self.assertIn("data.payload.as_str()", SOURCE)

    def test_surreal_point_reads_project_only_compared_fields(self) -> None:
        for source in (SOURCE, ROCKS_SOURCE):
            self.assertIn("SELECT bucket, payload FROM ONLY $id", source)
            self.assertNotIn('db.select(("item", id as i64))', source)

    def test_surreal_writes_do_not_materialize_returned_records(self) -> None:
        self.assertIn("UPSERT $id CONTENT $data RETURN NONE", SOURCE)
        self.assertIn("RETURN NONE;\\n", SOURCE)
        self.assertNotIn("let _: Option<RecordData> =", SOURCE)
        self.assertNotIn("payload: '{}'", SOURCE)


if __name__ == "__main__":
    unittest.main()
