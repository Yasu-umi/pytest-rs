"""Regression test: --collect-only fires the conftest's pytest_sessionfinish, like pytest 9.2.

Upstream's wrap_session fires pytest_sessionfinish after a collect-only
session as well, serial and under -n (xdist collects locally then), with the
session's exit status, before the terminal's ERRORS section and closing line.
pytest-rs returned without firing it, for the bundled plugins too. Expected
values are pytest 9.2.0 + pytest-xdist 3.8.0 + pytest-split 0.11.0's.
"""

from __future__ import annotations

import json
import os
import subprocess
import tempfile
import textwrap
import unittest
from pathlib import Path

ROOT = Path(__file__).parent.parent
# PYTEST_RS_BIN selects a binary built for another Python (see conformance/runner.py).
BINARY = Path(os.environ.get("PYTEST_RS_BIN") or ROOT / "target" / "debug" / "pytest-rs-bin")


class TestCollectOnlySessionfinish(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if not BINARY.exists():
            raise unittest.SkipTest("debug binary not built")

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.base = Path(self._tmp.name).resolve()
        (self.base / "conftest.py").write_text(
            textwrap.dedent(
                """
                def pytest_sessionfinish(session, exitstatus):
                    print(f"\\nSESSIONFINISH {int(exitstatus)}")
                """
            )
        )
        (self.base / "test_x.py").write_text("def test_a():\n    pass\n\ndef test_b():\n    pass\n")

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def run_rs(self, *args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [str(BINARY), "-p", "no:cacheprovider", "--collect-only", "-q", *args],
            cwd=self.base,
            capture_output=True,
            text=True,
        )

    def assert_before(self, stdout: str, first: str, then: str) -> None:
        self.assertIn(first, stdout)
        self.assertIn(then, stdout)
        self.assertLess(stdout.index(first), stdout.index(then), stdout)

    def test_serial(self) -> None:
        result = self.run_rs("test_x.py")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_before(result.stdout, "SESSIONFINISH 0", "2 tests collected")

    def test_numprocesses(self) -> None:
        result = self.run_rs("-n", "2", "test_x.py")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_before(result.stdout, "SESSIONFINISH 0", "2 tests collected")
        self.assertEqual(result.stdout.count("SESSIONFINISH"), 1, result.stdout)

    def test_no_tests(self) -> None:
        (self.base / "test_empty.py").write_text("")
        result = self.run_rs("test_empty.py")
        self.assertEqual(result.returncode, 5, result.stdout + result.stderr)
        self.assert_before(result.stdout, "SESSIONFINISH 5", "no tests collected")

    def test_collection_error(self) -> None:
        (self.base / "test_err.py").write_text("import nonexistent_module_for_this_test\n")
        result = self.run_rs("test_err.py")
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assert_before(result.stdout, "SESSIONFINISH 2", "ImportError while importing")

    def durations_after(self, *args: str) -> dict[str, float]:
        durations = self.base / ".test_durations"
        durations.write_text(json.dumps({"test_x.py::test_a": 1.5, "gone.py::test_z": 2.0}))
        result = self.run_rs("--store-durations", *args, "test_x.py")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        loaded: dict[str, float] = json.loads(durations.read_text())
        return loaded

    def test_store_durations_keeps_existing_durations(self) -> None:
        # pytest-split's sessionfinish runs, with no reports to add.
        self.assertEqual(self.durations_after(), {"test_x.py::test_a": 1.5, "gone.py::test_z": 2.0})

    def test_clean_durations_empties_them_like_pytest_split(self) -> None:
        self.assertEqual(self.durations_after("--clean-durations"), {})


if __name__ == "__main__":
    unittest.main()
