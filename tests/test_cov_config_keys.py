"""Coverage config keys the native --cov reads like coverage.py (#103).

- `[run] omit` / `[run] include`: files never measured.
- `[report] omit` / `[report] include`: files left out of the report.
- `[report] exclude_also`: added to the default (or custom) `exclude_lines`.
- `exclude_lines` / `exclude_also` from `setup.cfg` `[coverage:report]`.
"""

from __future__ import annotations

import os
import re
import subprocess
import tempfile
import textwrap
import unittest
from pathlib import Path

ROOT = Path(__file__).parent.parent
# PYTEST_RS_BIN selects a binary built for another Python (see conformance/runner.py).
BINARY = Path(os.environ.get("PYTEST_RS_BIN") or ROOT / "target" / "debug" / "pytest-rs-bin")

APP = """
def used():
    return 1


def unused():  # never called
    raise NotImplementedError
"""

HELPER = """
def helper():
    return 2
"""

TESTS = """
from app.core import used
from app.extra.helper import helper


def test_used():
    assert used() == 1
    assert helper() == 2
"""


class TestCovConfigKeys(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if not BINARY.exists():
            raise unittest.SkipTest("debug binary not built")

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.base = Path(self._tmp.name).resolve()
        self.write("app/__init__.py", "")
        self.write("app/core.py", APP)
        self.write("app/extra/__init__.py", "")
        self.write("app/extra/helper.py", HELPER)
        self.write("tests/test_app.py", TESTS)

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def write(self, rel: str, content: str) -> None:
        path = self.base / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(textwrap.dedent(content).lstrip("\n"))

    def rows(self, *args: str) -> dict[str, tuple[int, int]]:
        """Report rows: file -> (statements, missing)."""
        result = subprocess.run(
            [str(BINARY), "-p", "no:cacheprovider", "--cov=.", "tests", *args],
            cwd=self.base,
            capture_output=True,
            text=True,
            env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"},
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return {
            name: (int(stmts), int(miss))
            for name, stmts, miss in re.findall(r"^(\S+\.py)\s+(\d+)\s+(\d+)", result.stdout, re.M)
        }

    def pyproject(self, toml: str) -> None:
        self.write("pyproject.toml", '[tool.pytest.ini_options]\npythonpath = ["."]\n\n' + toml)

    def test_without_config_everything_is_measured(self) -> None:
        self.pyproject("")
        rows = self.rows()
        self.assertIn("app/extra/helper.py", rows)
        self.assertIn("tests/test_app.py", rows)
        self.assertEqual(rows["app/core.py"], (4, 1))

    def test_run_omit(self) -> None:
        self.pyproject('[tool.coverage.run]\nomit = ["*/tests/*", "app/extra/*"]\n')
        rows = self.rows()
        self.assertNotIn("tests/test_app.py", rows)
        self.assertNotIn("app/extra/helper.py", rows)
        self.assertIn("app/core.py", rows)

    def test_report_omit_and_include(self) -> None:
        self.pyproject('[tool.coverage.report]\ninclude = ["app/*"]\nomit = ["*/helper.py"]\n')
        rows = self.rows()
        self.assertNotIn("tests/test_app.py", rows)
        self.assertNotIn("app/extra/helper.py", rows)
        self.assertIn("app/core.py", rows)

    def test_exclude_also_keeps_the_default(self) -> None:
        self.write("app/core.py", APP.replace("return 1", "return 1  # pragma: no cover"))
        self.pyproject(
            "[tool.coverage.report]\nexclude_also = ['raise NotImplementedError', 'def unused']\n"
        )
        # `def unused` excludes the whole function; the pragma still excludes
        # `return 1`: only `def used():` is left.
        self.assertEqual(self.rows()["app/core.py"], (1, 0))

    def test_setup_cfg_report_section(self) -> None:
        self.pyproject("")
        self.write(
            "setup.cfg",
            """
            [coverage:run]
            omit =
                */tests/*
            [coverage:report]
            exclude_also =
                raise NotImplementedError
            """,
        )
        rows = self.rows()
        self.assertNotIn("tests/test_app.py", rows)
        self.assertEqual(rows["app/core.py"], (3, 0))


if __name__ == "__main__":
    unittest.main()
