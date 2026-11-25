import json
import os
import tempfile
import unittest
from pathlib import Path

from phase0_codex_active_test import read_latest_log_rate_limit_snapshot


class SnapshotTests(unittest.TestCase):
    def test_source_timestamp_across_all_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for i in range(8):
                event = {
                    "timestamp": f"2026-11-{i + 1:02}T00:00:00Z",
                    "type": "event_msg",
                    "payload": {"type": "token_count", "rate_limits": {"limit_id": "codex"}},
                }
                path = root / f"{i}.jsonl"
                path.write_text(json.dumps(event) + "\n", encoding="utf-8")
                os.utime(path, (1000 - i, 1000 - i))
            snapshot = read_latest_log_rate_limit_snapshot(root)
            self.assertEqual(snapshot["source_timestamp"], "2026-11-08T00:00:00Z")


if __name__ == "__main__":
    unittest.main()
