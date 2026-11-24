"""The native --cov resolves against the current directory, like coverage.py.

pytest run from a sub-directory without its own pytest config gets the
parent as rootdir; coverage.py (and so pytest-cov) still resolves `--cov`
values, report names, its config files and its output files against the
current directory.
"""

from __future__ import annotations

import importlib.util
import os
import re
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

ROOT = Path(__file__).parent.parent
# PYTEST_RS_BIN selects a binary built for another Python (see conformance/runner.py).
BINARY = Path(os.environ.get("PYTEST_RS_BIN") or ROOT / "target" / "debug" / "pytest-rs-bin")

MYMOD = """
def add(a, b):
    return a + b


def unused():
    return 0
"""

TESTS = """
from mymod import add
import mypkg


def test_add():
    assert add(1, 2) == 3
"""


class TestCovInvocationDir(unittest.TestCase):
    VENV = {"VIRTUAL_ENV": sys.prefix} if sys.prefix != sys.base_prefix else {}

    @classmethod
    def setUpClass(cls) -> None:
        if not BINARY.exists():
            raise unittest.SkipTest("debug binary not built")

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.proj = Path(self._tmp.name).resolve()
        self.sub = self.proj / "sub"
        # The pytest config (=> rootdir) is in the parent; pytest runs in sub/.
        self.write("pyproject.toml", "[tool.pytest.ini_options]\n")
        self.write("outside.py", "X = 1\n")
        self.write("sub/mymod.py", MYMOD)
        self.write("sub/mypkg/__init__.py", "def f():\n    return 1\n")
        self.write("sub/tests/__init__.py", "")
        self.write("sub/tests/test_mymod.py", TESTS)

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def write(self, rel: str, content: str) -> None:
        path = self.proj / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(textwrap.dedent(content).lstrip("\n"))

    def output(self, *args: str) -> str:
        result = subprocess.run(
            [str(BINARY), "-p", "no:cacheprovider", "tests", *args],
            cwd=self.sub,
            capture_output=True,
            text=True,
            env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1", **self.VENV},
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotIn("No data", result.stdout + result.stderr)
        return result.stdout

    def rows(self, *args: str) -> dict[str, tuple[int, int]]:
        """Report rows: file -> (statements, missing)."""
        return {
            name: (int(stmts), int(miss))
            for name, stmts, miss in re.findall(
                r"^(\S+\.py)\s+(\d+)\s+(\d+)", self.output(*args), re.M
            )
        }

    def test_module_name_not_importable_at_startup(self) -> None:
        """`sub/` goes on sys.path only during collection."""
        self.assertEqual(self.rows("--cov=mymod"), {"mymod.py": (4, 1)})

    def test_module_name_under_xdist(self) -> None:
        self.assertEqual(self.rows("--cov=mymod", "-n", "2"), {"mymod.py": (4, 1)})

    def test_package_name(self) -> None:
        self.assertEqual(self.rows("--cov=mypkg"), {"mypkg/__init__.py": (2, 1)})

    def test_dot_is_the_current_directory(self) -> None:
        self.assertEqual(
            sorted(self.rows("--cov=.")),
            ["mymod.py", "mypkg/__init__.py", "tests/__init__.py", "tests/test_mymod.py"],
        )

    def test_coverage_config_is_read_from_the_current_directory(self) -> None:
        self.write(
            "pyproject.toml",
            "[tool.pytest.ini_options]\n\n[tool.coverage.report]\nomit = ['*/mypkg/*']\n",
        )
        self.assertIn("mypkg/__init__.py", self.rows("--cov=."))
        self.write("sub/.coveragerc", "[report]\nomit = */mypkg/*\n")
        self.assertNotIn("mypkg/__init__.py", self.rows("--cov=."))

    def test_reports_are_written_to_the_current_directory(self) -> None:
        out = self.output("--cov=.", "--cov-report=xml")
        self.assertIn("Coverage XML written to file coverage.xml", out)
        self.assertFalse((self.proj / "coverage.xml").exists())
        xml = (self.sub / "coverage.xml").read_text()
        self.assertIn(f"<source>{self.sub}</source>", xml)
        self.assertIn('filename="mymod.py"', xml)

    @unittest.skipUnless(importlib.util.find_spec("coverage"), "writes need coverage")
    def test_data_file_is_written_to_the_current_directory(self) -> None:
        self.output("--cov=mymod")
        self.assertTrue((self.sub / ".coverage").exists())
        self.assertFalse((self.proj / ".coverage").exists())


if __name__ == "__main__":
    unittest.main()
