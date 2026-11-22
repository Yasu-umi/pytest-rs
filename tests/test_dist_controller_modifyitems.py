"""Regression test: a -n controller does not fire pytest_collection_modifyitems, like pytest-xdist.

Upstream xdist's controller collects nothing (DSession takes over
pytest_collection), so pytest_collection_modifyitems fires only in the
workers, each with its own items. pytest-rs also fired it in the controller,
with zero items. Expected values are pytest 9.2.0 + pytest-xdist 3.8.0's.
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


class TestDistControllerModifyitems(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if not BINARY.exists():
            raise unittest.SkipTest("debug binary not built")

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.base = Path(self._tmp.name).resolve()
        self.log = self.base / "hooks.log"
        (self.base / "conftest.py").write_text(
            textwrap.dedent(
                """
                import os

                def pytest_collection_modifyitems(session, config, items):
                    worker = os.environ.get("PYTEST_XDIST_WORKER", "controller")
                    with open(os.environ["HOOK_LOG"], "a") as f:
                        f.write(f"{worker} {len(items)}\\n")
                """
            )
        )
        (self.base / "test_x.py").write_text("def test_a():\n    pass\n\ndef test_b():\n    pass\n")

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def calls(self, *args: str) -> list[str]:
        result = subprocess.run(
            [str(BINARY), "-p", "no:cacheprovider", "-q", *args, "test_x.py"],
            cwd=self.base,
            capture_output=True,
            text=True,
            env={**os.environ, "HOOK_LOG": str(self.log)},
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return sorted(self.log.read_text().splitlines())

    def test_workers_only(self) -> None:
        self.assertEqual(self.calls("-n", "2"), ["gw0 2", "gw1 2"])

    def test_serial_still_fires(self) -> None:
        self.assertEqual(self.calls(), ["controller 2"])

    def test_worker_hook_error_is_an_internal_error(self) -> None:
        # The error used to surface only because the controller fired the
        # hook itself; it must now travel from the workers (xdist's
        # test_internal_errors_propagate_to_controller).
        (self.base / "conftest.py").write_text(
            "def pytest_collection_modifyitems():\n    raise RuntimeError('Some runtime error')\n"
        )
        result = subprocess.run(
            [str(BINARY), "-p", "no:cacheprovider", "-n", "2", "test_x.py"],
            cwd=self.base,
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertEqual(
            result.stdout.count("INTERNALERROR> RuntimeError: Some runtime error"), 1, result.stdout
        )

    def test_collect_only_under_n_collects_locally(self) -> None:
        # xdist collects in the controller under --collect-only.
        self.assertEqual(self.calls("-n", "2", "--collect-only"), ["controller 2"])


if __name__ == "__main__":
    unittest.main()
