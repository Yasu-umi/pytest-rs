"""`--collect-only -qq` prints no closing "N tests collected" line.

Upstream's summary_stats returns early below -q, so `-qq` ends with the
per-file counts and a blank line; pytest-rs printed the count line too.
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


class TestCollectOnlyQQ(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if not BINARY.exists():
            raise unittest.SkipTest("debug binary not built")

    def collect(self, *args: str) -> str:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp)
            (path / "test_q.py").write_text(
                "def test_a():\n    pass\n\n\ndef test_b():\n    pass\n"
            )
            return subprocess.run(
                [str(BINARY), "-p", "no:cacheprovider", "--collect-only", *args],
                cwd=path,
                capture_output=True,
                text=True,
            ).stdout

    def test_qq(self) -> None:
        self.assertEqual(self.collect("-qq"), "test_q.py: 2\n\n")
        self.assertEqual(self.collect("-qqq"), "test_q.py: 2\n\n")

    def test_q_keeps_the_count(self) -> None:
        out = self.collect("-q")
        self.assertTrue(
            out.startswith("test_q.py::test_a\ntest_q.py::test_b\n\n2 tests collected in "), out
        )


if __name__ == "__main__":
    unittest.main()
