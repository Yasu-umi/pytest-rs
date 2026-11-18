"""Regression test: a class fixture defined as @pytest.fixture over @classmethod.

pytest binds the classmethod to its class, so the fixture receives `cls` and
needs no instance (anyio's own suite uses one). pytest-rs used to treat it as
an instance method and failed collection with "Could not determine arguments".
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


class TestClassmethodFixture(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if not BINARY.exists():
            raise unittest.SkipTest("debug binary not built")

    def test_classmethod_fixture_is_bound_to_the_class(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            (tmp_path / "test_cm.py").write_text(
                "import pytest\n"
                "\n"
                "class TestX:\n"
                "    marker = 'cls-attr'\n"
                "\n"
                "    @pytest.fixture(scope='class', params=[1, 2])\n"
                "    @classmethod\n"
                "    def fam(cls, request):\n"
                "        return (cls.marker, request.param)\n"
                "\n"
                "    def test_it(self, fam):\n"
                "        assert fam[0] == 'cls-attr'\n"
                "        assert fam[1] in (1, 2)\n"
            )
            result = subprocess.run(
                [str(BINARY), "test_cm.py", "-q", "-p", "no:cacheprovider"],
                cwd=tmp_path,
                capture_output=True,
                text=True,
            )
            self.assertEqual(
                result.returncode,
                0,
                f"pytest-rs exited {result.returncode}\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("2 passed", result.stdout)


if __name__ == "__main__":
    unittest.main()
