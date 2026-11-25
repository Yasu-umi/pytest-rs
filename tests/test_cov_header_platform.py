"""The native --cov section header names the platform like pytest-cov.

pytest-cov writes `coverage: platform {sys.platform}, python ...`
(`darwin`, `linux`, `win32`); pytest-rs wrote Rust's OS family name
(`macos`, `windows`), which only matched on Linux.
"""

from __future__ import annotations

import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).parent.parent
# PYTEST_RS_BIN selects a binary built for another Python (see conformance/runner.py).
BINARY = Path(os.environ.get("PYTEST_RS_BIN") or ROOT / "target" / "debug" / "pytest-rs-bin")


class TestCovHeaderPlatform(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if not BINARY.exists():
            raise unittest.SkipTest("debug binary not built")

    def test_platform_is_sys_platform(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp)
            (path / "mod.py").write_text("def f():\n    return 1\n")
            (path / "test_mod.py").write_text(
                "from mod import f\n\n\ndef test_f():\n    assert f() == 1\n"
            )
            result = subprocess.run(
                [str(BINARY), "-p", "no:cacheprovider", "-q", "--cov=mod"],
                cwd=path,
                capture_output=True,
                text=True,
            )
        output = f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        self.assertEqual(result.returncode, 0, output)
        self.assertIn(f" coverage: platform {sys.platform}, python ", result.stdout, output)


if __name__ == "__main__":
    unittest.main()
