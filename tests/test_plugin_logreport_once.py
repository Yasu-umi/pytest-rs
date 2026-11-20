"""Regression test: a `-p` plugin module's pytest_runtest_logreport fires once per report.

The engine took the impl from both its own conftest-hook list (where `-p`
modules are also tagged) and the shim pluginmanager, so it ran twice per
report (6 calls for one passing test instead of pytest's 3).
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


class TestPluginLogreportOnce(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if not BINARY.exists():
            raise unittest.SkipTest("debug binary not built")

    def test_dash_p_plugin_logreport_fires_once_per_report(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            (tmp_path / "myplug.py").write_text(
                "CALLS = []\n"
                "def pytest_runtest_logreport(report):\n"
                "    CALLS.append((report.nodeid, report.when))\n"
                "def pytest_sessionfinish(session):\n"
                "    print('LOGREPORT_CALLS', len(CALLS))\n"
            )
            (tmp_path / "test_a.py").write_text("def test_a():\n    pass\n")
            result = subprocess.run(
                [str(BINARY), "-q", "-s", "-p", "myplug", "-p", "no:cacheprovider"],
                cwd=tmp_path,
                capture_output=True,
                text=True,
                env={**os.environ, "PYTHONPATH": str(tmp_path)},
            )
            output = f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
            self.assertEqual(result.returncode, 0, output)
            self.assertIn("LOGREPORT_CALLS 3", result.stdout, output)


if __name__ == "__main__":
    unittest.main()
