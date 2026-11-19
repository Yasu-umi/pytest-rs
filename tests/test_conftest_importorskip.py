"""Regression test: a conftest.py that skips itself (module-level importorskip).

When collection reaches a directory whose conftest calls
``pytest.importorskip`` for a missing module, pytest skips that directory
(one skipped collect report, located at the conftest's importorskip line)
and collects the rest. pytest-rs used to stop the whole run with
"ImportError while loading conftest" and exit 4.
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


class TestConftestImportorskip(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if not BINARY.exists():
            raise unittest.SkipTest("debug binary not built")

    def run_in_tree(
        self, *args: str, env: dict[str, str] | None = None
    ) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            sub = tmp_path / "pkg" / "sub"
            sub.mkdir(parents=True)
            (tmp_path / "pkg" / "test_top.py").write_text("def test_top():\n    pass\n")
            (sub / "conftest.py").write_text(
                'import pytest\npytest.importorskip("no_such_module_xyz")\n'
            )
            (sub / "test_a.py").write_text("def test_a():\n    pass\n")
            return subprocess.run(
                [str(BINARY), "pkg", "-rs", "-p", "no:cacheprovider", *args],
                cwd=tmp_path,
                capture_output=True,
                text=True,
                env={**os.environ, **(env or {})},
            )

    def assert_dir_skipped(self, result: subprocess.CompletedProcess[str]) -> None:
        output = f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        self.assertEqual(result.returncode, 0, output)
        self.assertIn(
            "SKIPPED [1] pkg/sub/conftest.py:2: could not import 'no_such_module_xyz'",
            result.stdout,
        )
        self.assertIn("1 passed, 1 skipped", result.stdout, output)

    def test_directory_is_skipped(self) -> None:
        self.assert_dir_skipped(self.run_in_tree())

    def test_directory_is_skipped_with_forked_workers(self) -> None:
        self.assert_dir_skipped(self.run_in_tree("-n", "2"))

    def test_directory_is_skipped_with_spawned_workers(self) -> None:
        self.assert_dir_skipped(self.run_in_tree("-n", "2", env={"PYTEST_RS_DIST_SPAWN": "1"}))


if __name__ == "__main__":
    unittest.main()
