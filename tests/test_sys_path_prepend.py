"""Regression test: sys_path_prepend must move rootdir to sys.path[0].

Issue: when rootdir was already present in sys.path (e.g. via PYTHONPATH) but
at a non-zero index, pytest-rs skipped insertion entirely and left it in place.
A shadowing package earlier in PYTHONPATH would then win over the project root.
"""

from __future__ import annotations

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).parent.parent
BINARY = ROOT / "target" / "debug" / "pytest-rs-bin"


class TestSysPathPrepend(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if not BINARY.exists():
            raise unittest.SkipTest("debug binary not built")

    def test_moves_existing_to_front(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)

            # Package at project root — should always win.
            (tmp_path / "mypkg").mkdir()
            (tmp_path / "mypkg" / "__init__.py").write_text("VALUE = 'root'\n")

            # Shadowing copy placed *before* tmp_path in PYTHONPATH.
            shadow = tmp_path / "shadow"
            shadow.mkdir()
            (shadow / "mypkg").mkdir()
            (shadow / "mypkg" / "__init__.py").write_text("VALUE = 'shadow'\n")

            (tmp_path / "test_import.py").write_text(
                "import mypkg\n\ndef test_root_wins():\n    assert mypkg.VALUE == 'root', mypkg.VALUE\n"
            )

            env = os.environ.copy()
            # shadow comes before the project root — without the fix, shadow wins.
            env["PYTHONPATH"] = str(shadow) + os.pathsep + str(tmp_path)

            result = subprocess.run(
                [str(BINARY), "test_import.py", "--tb=short"],
                cwd=tmp_path,
                env=env,
                capture_output=True,
                text=True,
            )
            self.assertEqual(
                result.returncode,
                0,
                f"pytest-rs exited {result.returncode}\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )


if __name__ == "__main__":
    unittest.main()
