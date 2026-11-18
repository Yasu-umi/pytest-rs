"""Regression test: a class fixture defined as @pytest.fixture over @staticmethod.

pytest unwraps the staticmethod to its function, so a generator (or async)
staticmethod fixture is recognised as one. pytest-rs used to keep the
staticmethod object (`staticmethod.__get__(None, None)` raises), which is
still callable, so the test received the raw generator instead of its value.
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


class TestStaticmethodFixture(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if not BINARY.exists():
            raise unittest.SkipTest("debug binary not built")

    def test_generator_staticmethod_fixture_yields_its_value(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            (tmp_path / "test_sm.py").write_text(
                "import pytest\n"
                "\n"
                "torn_down = []\n"
                "\n"
                "class TestX:\n"
                "    @pytest.fixture\n"
                "    @staticmethod\n"
                "    def value():\n"
                "        yield 1\n"
                "        torn_down.append(True)\n"
                "\n"
                "    def test_it(self, value):\n"
                "        assert value == 1\n"
                "\n"
                "def test_teardown_ran():\n"
                "    assert torn_down == [True]\n"
            )
            result = subprocess.run(
                [str(BINARY), "test_sm.py", "-q", "-p", "no:cacheprovider"],
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
