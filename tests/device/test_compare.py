"""耗时比较器的离线回归：失败和不完整的数据不能被当成性能提升。"""
import contextlib
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

import compare


def step(tool="iphone_status", seconds=0.2, ok=True):
    return {"tool": tool, "args": {}, "seconds": seconds, "unlock_seconds": 0, "ok": ok}


class CompareTests(unittest.TestCase):
    def compare_runs(self, old, new):
        with tempfile.TemporaryDirectory() as folder:
            a, b = Path(folder)/"old.json", Path(folder)/"new.json"
            a.write_text(json.dumps({"device": "fixture", "runs": old}))
            b.write_text(json.dumps({"device": "fixture", "runs": new}))
            with patch.object(sys, "argv", ["compare.py", str(a), str(b)]), contextlib.redirect_stdout(io.StringIO()):
                return compare.main()

    def test_valid_equal_runs_pass(self):
        self.assertEqual(self.compare_runs([[step()]], [[step()]]), 0)

    def test_slower_valid_run_fails(self):
        self.assertEqual(self.compare_runs([[step()]], [[step(seconds=0.8)]]), 1)

    def test_failed_new_run_is_not_accepted_as_faster(self):
        self.assertEqual(self.compare_runs([[step()]], [[step(seconds=0.001, ok=False)]]), 1)

    def test_failed_baseline_is_not_accepted(self):
        self.assertEqual(self.compare_runs([[step(ok=False)]], [[step()]]), 1)

    def test_incomplete_run_is_not_silently_dropped(self):
        whole = [step(), step("iphone_screenshot")]
        self.assertEqual(self.compare_runs([whole, [step()]], [whole]), 1)

    def test_empty_run_is_not_silently_dropped(self):
        self.assertEqual(self.compare_runs([[step()], []], [[step()]]), 1)


if __name__ == "__main__":
    unittest.main()
