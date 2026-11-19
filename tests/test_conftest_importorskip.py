"""Regression test: a conftest.py that skips itself (module-level importorskip).

When collection reaches a directory whose conftest calls
``pytest.importorskip`` for a missing module, pytest skips that directory
(one skipped collect report, located at the conftest's importorskip line)
and collects the rest. pytest-rs used to stop the whole run with
"ImportError while loading conftest" and exit 4.

An initial conftest (on an argument's own path) is different: pytest 9.2
wraps only ``Exception`` in ConftestImportFailure, so the Skipped escapes and
the interpreter reports it as an uncaught exception (traceback on stderr,
nothing on stdout, exit status 1).
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
        self, *args: str, target: str = "pkg", env: dict[str, str] | None = None
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
                [str(BINARY), target, "-rs", "-p", "no:cacheprovider", *args],
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

    def test_initial_conftest_skip_is_uncaught(self) -> None:
        for target in ("pkg/sub", "pkg/sub/test_a.py"):
            with self.subTest(target=target):
                result = self.run_in_tree(target=target)
                output = f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
                self.assertEqual(result.returncode, 1, output)
                self.assertEqual(result.stdout, "", output)
                self.assertTrue(
                    result.stderr.startswith("Traceback (most recent call last):\n"), output
                )
                self.assertIn('pytest.importorskip("no_such_module_xyz")', result.stderr)
                self.assertTrue(
                    result.stderr.endswith(
                        "Skipped: could not import 'no_such_module_xyz': No module named 'no_such_module_xyz'\n"
                    ),
                    output,
                )


if __name__ == "__main__":
    unittest.main()
