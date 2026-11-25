"""Hook exceptions outside the runtest reporting hooks, like upstream.

- A collection-phase hookimpl that raises (pytest_collection,
  pytest_collection_modifyitems, pytest_itemcollected, pytest_deselected,
  pytest_collection_finish) or pytest_runtestloop ends the session with
  INTERNALERROR (exit 3): the traceback once, then a blank line and the stats
  line from wrap_session's `finally`.
- pytest_exception_interact fires for every failed phase, also without --pdb.
- An import failure's short-summary line is the bare "ERROR path".
"""

from __future__ import annotations

import os
import subprocess
import tempfile
import textwrap
import unittest
from pathlib import Path

ROOT = Path(__file__).parent.parent
# PYTEST_RS_BIN selects a binary built for another Python (see conformance/runner.py).
BINARY = Path(os.environ.get("PYTEST_RS_BIN") or ROOT / "target" / "debug" / "pytest-rs-bin")

TESTS = """
def test_ok():
    pass


def test_fail():
    assert 0


def test_other():
    pass
"""


class TestCollectionHookInternalError(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if not BINARY.exists():
            raise unittest.SkipTest("debug binary not built")

    def run_pytest(
        self, conftest: str, *args: str, files: dict[str, str] | None = None
    ) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp)
            (path / "conftest.py").write_text(textwrap.dedent(conftest))
            for name, content in (files or {"test_x.py": TESTS}).items():
                (path / name).write_text(textwrap.dedent(content))
            return subprocess.run(
                [str(BINARY), "-p", "no:cacheprovider", "-q", "-k", "not other", *args],
                cwd=path,
                capture_output=True,
                text=True,
            )

    def assert_internal_error(self, hook: str, args: str, stats: str) -> None:
        result = self.run_pytest(f"def {hook}({args}):\n    raise AttributeError('boom')\n")
        output = f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        self.assertEqual(result.returncode, 3, output)
        self.assertEqual(result.stdout.count("INTERNALERROR> AttributeError: boom"), 1, output)
        self.assertRegex(
            result.stdout, rf"INTERNALERROR> AttributeError: boom\n\n{stats} in [\d.]+s\n$"
        )
        self.assertNotIn("INTERNAL ERROR", result.stderr, output)

    def test_collection(self) -> None:
        self.assert_internal_error("pytest_collection", "session", "no tests ran")

    def test_modifyitems(self) -> None:
        self.assert_internal_error("pytest_collection_modifyitems", "items", "no tests ran")

    def test_itemcollected(self) -> None:
        self.assert_internal_error("pytest_itemcollected", "item", "no tests ran")

    def test_deselected(self) -> None:
        self.assert_internal_error("pytest_deselected", "items", "1 deselected")

    def test_collection_finish(self) -> None:
        self.assert_internal_error("pytest_collection_finish", "session", "1 deselected")

    def test_runtestloop(self) -> None:
        self.assert_internal_error("pytest_runtestloop", "session", "1 deselected")

    def test_runtest_hook_banner_once_with_stats(self) -> None:
        self.assert_internal_error("pytest_runtest_makereport", "item, call", "1 deselected")

    def test_exception_interact_without_pdb(self) -> None:
        result = self.run_pytest(
            """
            def pytest_exception_interact(node, call, report):
                print("\\nINTERACT", node.nodeid, call.when, type(call.excinfo.value).__name__)
            """,
            "-s",
        )
        output = f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        self.assertEqual(result.returncode, 1, output)
        self.assertEqual(
            result.stdout.count("INTERACT test_x.py::test_fail call AssertionError"), 1, output
        )
        self.assertNotIn("INTERACT test_x.py::test_ok", result.stdout, output)

    def test_exception_interact_raising_keeps_the_failure(self) -> None:
        result = self.run_pytest(
            "def pytest_exception_interact(node, call, report):\n    raise AttributeError('boom')\n"
        )
        output = f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        self.assertEqual(result.returncode, 3, output)
        self.assertRegex(result.stdout, r"\n\n1 failed, 1 passed, 1 deselected in [\d.]+s\n$")

    def test_import_error_summary_line(self) -> None:
        files = {"test_err.py": "import nope\n", "test_ok.py": "def test_a():\n    pass\n"}
        result = self.run_pytest("", files=files)
        self.assertIn("\nERROR test_err.py\n", result.stdout, result.stdout)
        result = self.run_pytest("", "--collect-only", files=files)
        self.assertIn(
            "ERROR test_err.py\n!!!!!!!!!!!!!!!!!!!! Interrupted: 1 error during collection "
            "!!!!!!!!!!!!!!!!!!!!\n1 test collected, 1 error in ",
            result.stdout,
        )


if __name__ == "__main__":
    unittest.main()
