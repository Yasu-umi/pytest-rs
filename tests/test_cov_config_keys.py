"""Coverage config keys the native --cov reads like coverage.py (#103).

- `[run] omit` / `[run] include`: files never measured.
- `[report] omit` / `[report] include`: files left out of the report.
- `[report] exclude_also`: added to the default (or custom) `exclude_lines`.
- `exclude_lines` / `exclude_also` from `setup.cfg` `[coverage:report]`.
- `[run] source` / `source_pkgs` for a bare `--cov`, `[run] data_file`.
- `[report] partial_branches`, `skip_empty`, `sort`, `ignore_errors`.
- `[xml] output`.
- File names with `/./` (from `pythonpath = ["."]`) match their source.
"""

from __future__ import annotations

import importlib.util
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

    def output(self, *args: str, cov: str = "--cov=.") -> str:
        result = subprocess.run(
            # The path first: a bare `--cov` would take it as its value.
            [str(BINARY), "-p", "no:cacheprovider", "tests", cov, *args],
            cwd=self.base,
            capture_output=True,
            text=True,
            env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"},
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result.stdout

    def rows(self, *args: str, cov: str = "--cov=.") -> dict[str, tuple[int, int]]:
        """Report rows: file -> (statements, missing)."""
        return {
            name: (int(stmts), int(miss))
            for name, stmts, miss in re.findall(
                r"^(\S+\.py)\s+(\d+)\s+(\d+)", self.output(*args, cov=cov), re.M
            )
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

    def test_source_dir_with_dot_on_sys_path(self) -> None:
        """`pythonpath = ["."]` puts `rootdir/.` on sys.path, so file names
        contain `/./`; they still fall under `--cov=app`."""
        self.pyproject("")
        rows = self.rows(cov="--cov=app")
        self.assertEqual(rows["app/core.py"], (4, 1))
        self.assertNotIn("tests/test_app.py", rows)

    def test_bare_cov_uses_run_source(self) -> None:
        self.pyproject('[tool.coverage.run]\nsource = ["app"]\n')
        rows = self.rows(cov="--cov")
        self.assertIn("app/core.py", rows)
        self.assertNotIn("tests/test_app.py", rows)

    def test_bare_cov_uses_source_pkgs(self) -> None:
        self.pyproject('[tool.coverage.run]\nsource_pkgs = ["app.extra"]\n')
        rows = self.rows(cov="--cov")
        self.assertIn("app/extra/helper.py", rows)
        self.assertNotIn("app/core.py", rows)

    @unittest.skipUnless(importlib.util.find_spec("coverage"), "writes need coverage")
    def test_data_file(self) -> None:
        self.pyproject('[tool.coverage.run]\ndata_file = "out/covdata"\n')
        (self.base / "out").mkdir()
        self.rows()
        self.assertTrue((self.base / "out/covdata").exists())
        self.assertFalse((self.base / ".coverage").exists())

    def test_xml_output(self) -> None:
        self.pyproject('[tool.coverage.xml]\noutput = "reports/cov.xml"\n')
        (self.base / "reports").mkdir()
        out = self.output("--cov-report=xml")
        self.assertIn("Coverage XML written to file reports/cov.xml", out)
        self.assertTrue((self.base / "reports/cov.xml").exists())

    def test_skip_empty(self) -> None:
        self.pyproject("[tool.coverage.report]\nskip_empty = true\n")
        out = self.output()
        self.assertNotIn("app/__init__.py", out)
        self.assertIn("2 empty files skipped.", out)

    def test_sort(self) -> None:
        self.pyproject('[tool.coverage.report]\nsort = "-miss"\n')
        names = re.findall(r"^(\S+\.py)\s+\d+\s+\d+", self.output(), re.M)
        self.assertEqual(names[0], "app/core.py")

    def test_invalid_sort_fails_the_report(self) -> None:
        self.pyproject('[tool.coverage.report]\nsort = "size"\n')
        self.assertIn("Failed to generate report: Invalid sorting option: 'size'", self.output())

    def test_partial_branches(self) -> None:
        self.write(
            "app/core.py",
            """
            def used(flag=True):
                if flag:
                    return 1
                return 0


            def other(flag=True):
                if flag:  # pragma: no branch
                    return 1
                return 0
            """,
        )
        self.write(
            "tests/test_app.py",
            """
            from app.core import used, other


            def test_used():
                assert used() == 1
                assert other() == 1
            """,
        )
        self.pyproject("[tool.coverage.run]\nbranch = true\n")
        # Name, Stmts, Miss, Branch, BrPart: only `used`'s `if` is partial.
        row = re.search(r"^app/core\.py\s+(\d+)\s+(\d+)\s+(\d+)\s+(\d+)", self.output(), re.M)
        assert row is not None
        self.assertEqual((int(row.group(3)), int(row.group(4))), (4, 1))

    def test_missing_source_fails_the_report_unless_ignore_errors(self) -> None:
        self.write("app/gone.py", "def gone():\n    return 3\n")
        self.write(
            "tests/test_app.py",
            """
            import os
            from app import gone


            def test_gone():
                assert gone.gone() == 3
                os.remove(gone.__file__)
            """,
        )
        self.pyproject("")
        self.assertIn("Failed to generate report: No source for code:", self.output())
        self.write("app/gone.py", "def gone():\n    return 3\n")
        self.pyproject("[tool.coverage.report]\nignore_errors = true\n")
        out = self.output()
        self.assertNotIn("Failed to generate report", out)
        self.assertIn("app/core.py", out)


if __name__ == "__main__":
    unittest.main()
