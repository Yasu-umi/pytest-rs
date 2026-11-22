"""Regression test: under -n the summary has no "deselected" count, like pytest-xdist.

xdist's workers deselect (-k/-m/--deselect/pytest_collection_modifyitems)
on their own and never report the count to the controller, so the closing
line reads "no tests ran" / "1 passed" where a serial run says
"2 deselected" / "1 passed, 1 deselected". pytest-rs summed the workers'
count into the controller's line. Expected values are pytest 9.2.0 +
pytest-xdist 3.8.0's.
"""

from __future__ import annotations

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).parent.parent
# PYTEST_RS_BIN selects a binary built for another Python (see conformance/runner.py).
BINARY = Path(os.environ.get("PYTEST_RS_BIN") or ROOT / "target" / "debug" / "pytest-rs-bin")


class TestDistDeselectedSummary(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if not BINARY.exists():
            raise unittest.SkipTest("debug binary not built")

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.base = Path(self._tmp.name).resolve()
        (self.base / "test_x.py").write_text("def test_a():\n    pass\n\ndef test_b():\n    pass\n")

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def summary(self, *args: str) -> tuple[int, str]:
        result = subprocess.run(
            [str(BINARY), "-p", "no:cacheprovider", *args, "test_x.py"],
            cwd=self.base,
            capture_output=True,
            text=True,
        )
        lines = [line for line in result.stdout.splitlines() if line.strip()]
        self.assertTrue(lines, result.stdout + result.stderr)
        return result.returncode, lines[-1]

    def test_everything_deselected(self) -> None:
        for args in (["-k", "nomatch"], ["-m", "slow"]):
            code, line = self.summary("-n", "2", *args)
            self.assertEqual(code, 5, line)
            self.assertIn("no tests ran in", line)
            self.assertNotIn("deselected", line)

    def test_partly_deselected(self) -> None:
        for args in (["-k", "test_a"], ["--deselect", "test_x.py::test_b"]):
            code, line = self.summary("-n", "2", *args)
            self.assertEqual(code, 0, line)
            self.assertIn("1 passed in", line)
            self.assertNotIn("deselected", line)

    def test_serial_still_counts(self) -> None:
        code, line = self.summary("-k", "nomatch")
        self.assertEqual(code, 5, line)
        self.assertIn("2 deselected in", line)


if __name__ == "__main__":
    unittest.main()
