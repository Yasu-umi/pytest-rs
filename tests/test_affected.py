"""End-to-end scenarios for --affected (docs/affected.md).

Each test builds a small project, records a run, edits it, runs again and
asserts which node ids ran. The rule each scenario covers is named in its
docstring.
"""

from __future__ import annotations

import os
import re
import subprocess
import tempfile
import textwrap
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).parent.parent
# PYTEST_RS_BIN selects a binary built for another Python (see conformance/runner.py).
BINARY = Path(os.environ.get("PYTEST_RS_BIN") or ROOT / "target" / "debug" / "pytest-rs-bin")


class Project:
    """A throwaway project directory and helpers to run pytest-rs in it."""

    OUTCOME = re.compile(r"^(\S+::\S+) (PASSED|FAILED|ERROR|SKIPPED|XFAIL|XPASS)", re.M)
    # -n -v: "[gw0] [ 50%] PASSED tests/test_x.py::test_a".
    DIST_OUTCOME = re.compile(
        r"^\[gw\d+\] \[[^\]]*\] (?:PASSED|FAILED|ERROR|SKIPPED|XFAIL|XPASS) (\S+::\S+)", re.M
    )

    def __init__(self, base: Path) -> None:
        self.base = base
        self.write(
            "pyproject.toml",
            """
            [tool.pytest.ini_options]
            testpaths = ["tests"]
            pythonpath = ["."]
            """,
        )
        self.write("app/__init__.py", "")
        self.write(
            "app/core.py",
            """
            LIMIT = 10

            def add(a, b):
                return a + b

            def mul(a, b):
                return a * b

            class Box:
                size = 1

                def grow(self):
                    return self.size + 1
            """,
        )
        self.write("tests/conftest.py", "")
        self.write(
            "tests/test_add.py",
            """
            from app.core import add

            def test_add():
                assert add(1, 2) == 3

            def test_add_more():
                assert add(2, 2) == 4
            """,
        )
        self.write(
            "tests/test_mul.py",
            """
            from app.core import mul, Box

            def test_mul():
                assert mul(2, 2) == 4

            def test_box():
                assert Box().grow() == 2
            """,
        )

    def write(self, rel: str, content: str) -> None:
        path = self.base / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(textwrap.dedent(content).lstrip("\n"))

    def edit(self, rel: str, old: str, new: str) -> None:
        path = self.base / rel
        text = path.read_text()
        assert old in text, f"{old!r} not in {rel}"
        path.write_text(text.replace(old, new, 1))

    def run(
        self, *args: str, env: dict[str, str] | None = None
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [str(BINARY), "-p", "no:cacheprovider", "-v", *args],
            cwd=self.base,
            capture_output=True,
            text=True,
            env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1", **(env or {})},
        )

    def ran(self, *args: str, env: dict[str, str] | None = None) -> set[str]:
        """Node ids that ran (any outcome)."""
        result = self.run(*args, env=env)
        assert result.returncode in (0, 1), result.stdout + result.stderr
        return self.nodeids(result.stdout)

    @classmethod
    def nodeids(cls, stdout: str) -> set[str]:
        return {nodeid for nodeid, _ in cls.OUTCOME.findall(stdout)} | set(
            cls.DIST_OUTCOME.findall(stdout)
        )


ALL = {
    "tests/test_add.py::test_add",
    "tests/test_add.py::test_add_more",
    "tests/test_mul.py::test_mul",
    "tests/test_mul.py::test_box",
}


class TestAffected(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if not BINARY.exists():
            raise unittest.SkipTest("debug binary not built")

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.project = Project(Path(self._tmp.name).resolve())

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def record(self) -> None:
        self.assertEqual(self.project.ran("--affected"), ALL)

    # --- zero impact and options --------------------------------------------

    def test_without_option_nothing_is_recorded(self) -> None:
        """without --affected* no data file is written."""
        self.assertEqual(self.project.ran(), ALL)
        self.assertFalse((self.project.base / ".pytest-rs-affected").exists())

    def test_invalid_mode_is_a_usage_error(self) -> None:
        result = self.project.run("--affected=sometimes")
        self.assertEqual(result.returncode, 4, result.stdout + result.stderr)

    # --- selection basics ---------------------------------------------------

    def test_nothing_changed_deselects_everything_and_exits_ok(self) -> None:
        """exit status 5 becomes 0 when everything was deselected."""
        self.record()
        result = self.project.run("--affected")
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertEqual(self.project.nodeids(result.stdout), set())
        self.assertIn("2 of 2 test files unchanged, not imported", result.stdout)

    def test_select_without_data_runs_everything(self) -> None:
        """select with no usable data runs everything and says so."""
        result = self.project.run("--affected=select")
        self.assertEqual(self.project.nodeids(result.stdout), ALL)
        self.assertIn("affected: select, .pytest-rs-affected (no data)", result.stdout)

    def test_body_edit_selects_its_callers(self) -> None:
        """a function body edit reruns exactly the tests that called it."""
        self.record()
        self.project.edit("app/core.py", "return a * b", "return b * a")
        self.assertEqual(self.project.ran("--affected"), {"tests/test_mul.py::test_mul"})
        self.assertEqual(self.project.ran("--affected"), set())

    def test_method_edit_selects_its_callers(self) -> None:
        self.record()
        self.project.edit("app/core.py", "return self.size + 1", "return 1 + self.size")
        self.assertEqual(self.project.ran("--affected"), {"tests/test_mul.py::test_box"})

    def test_formatting_and_comment_edits_select_nothing(self) -> None:
        """checksums ignore formatting and comments."""
        self.record()
        self.project.edit("app/core.py", "return a + b", "return (a  +  b)  # sum")
        self.assertEqual(self.project.ran("--affected"), set())

    def test_new_function_selects_nothing(self) -> None:
        """adding a function changes no existing unit."""
        self.record()
        self.project.edit("app/core.py", "def mul", "def sub(a, b):\n    return a - b\n\ndef mul")
        self.assertEqual(self.project.ran("--affected"), set())

    def test_module_statement_edit_selects_every_test_using_the_module(self) -> None:
        """a dependency on a function implies its module unit."""
        self.record()
        self.project.edit("app/core.py", "LIMIT = 10", "LIMIT = 11")
        self.assertEqual(self.project.ran("--affected"), ALL)

    def write_lazy_user(self) -> None:
        self.project.write("app/extra.py", "def value():\n    return 1\n")
        self.project.write(
            "tests/test_extra.py",
            """
            def test_extra():
                from app import extra
                assert extra.value() == 1
            """,
        )
        self.project.ran("--affected")

    def test_deleted_module_selects_its_dependents(self) -> None:
        """a deleted .py file removes its units, so their dependents run."""
        self.write_lazy_user()
        (self.project.base / "app/extra.py").unlink()
        self.assertEqual(self.project.ran("--affected"), {"tests/test_extra.py::test_extra"})

    def test_unparseable_file_selects_its_dependents(self) -> None:
        """a file that fails to parse has all of its units changed."""
        self.write_lazy_user()
        self.project.edit("app/extra.py", "return 1", "return (")
        self.assertEqual(self.project.ran("--affected"), {"tests/test_extra.py::test_extra"})

    # --- contexts -----------------------------------------------------------

    def test_session_fixture_dependencies_belong_to_its_users(self) -> None:
        """a higher-scope fixture's code is a dependency of every test using it."""
        self.project.write(
            "tests/conftest.py",
            """
            import pytest
            from app import core

            @pytest.fixture(scope="session")
            def base():
                return core.mul(2, 3)
            """,
        )
        self.project.write(
            "tests/test_fixture.py",
            """
            def test_first(base):
                assert base == 6

            def test_second(base):
                assert base == 6

            def test_unrelated():
                pass
            """,
        )
        self.project.ran("--affected")
        self.project.edit("app/core.py", "return a * b", "return b * a")
        self.assertEqual(
            self.project.ran("--affected"),
            {
                "tests/test_fixture.py::test_first",
                "tests/test_fixture.py::test_second",
                "tests/test_mul.py::test_mul",
            },
        )
        self.project.edit("tests/conftest.py", "core.mul(2, 3)", "core.mul(3, 2)")
        self.assertEqual(
            self.project.ran("--affected"),
            {"tests/test_fixture.py::test_first", "tests/test_fixture.py::test_second"},
        )

    def test_fixture_finalizer_belongs_to_the_fixture(self) -> None:
        """finalizer code runs at scope end, attributed to the fixture."""
        self.project.write(
            "app/resource.py",
            """
            def open_it():
                return "open"

            def close_it():
                return "closed"
            """,
        )
        self.project.write(
            "tests/test_resource.py",
            """
            import pytest
            from app import resource

            @pytest.fixture(scope="module")
            def handle():
                yield resource.open_it()
                resource.close_it()

            def test_a(handle):
                assert handle == "open"

            def test_b(handle):
                assert handle == "open"

            def test_c():
                pass
            """,
        )
        self.project.ran("--affected")
        self.project.edit("app/resource.py", 'return "closed"', 'return "shut"')
        self.assertEqual(
            self.project.ran("--affected"),
            {"tests/test_resource.py::test_a", "tests/test_resource.py::test_b"},
        )

    def test_xunit_module_setup_belongs_to_every_test_of_the_module(self) -> None:
        """xunit setup_module/teardown_module are treated like module fixtures."""
        self.project.write(
            "app/helpers.py",
            """
            def prepare():
                return "ready"

            def cleanup():
                return "done"
            """,
        )
        self.project.write(
            "tests/test_xunit.py",
            """
            from app import helpers

            STATE = {}

            def setup_module(module):
                STATE["s"] = helpers.prepare()

            def teardown_module(module):
                helpers.cleanup()

            def test_one():
                assert STATE["s"] == "ready"

            def test_two():
                assert STATE["s"] == "ready"
            """,
        )
        self.project.ran("--affected")
        both = {"tests/test_xunit.py::test_one", "tests/test_xunit.py::test_two"}
        self.project.edit("app/helpers.py", 'return "ready"', 'return "rea" + "dy"')
        self.assertEqual(self.project.ran("--affected"), both)
        self.project.edit("app/helpers.py", 'return "done"', 'return "finished"')
        self.assertEqual(self.project.ran("--affected"), both)

    def test_import_time_code_belongs_to_the_importing_test_file(self) -> None:
        """code run while importing test file X is a dependency of X's tests."""
        self.project.write("app/settings.py", "VALUE = 1\n")
        self.project.write(
            "tests/test_settings.py",
            """
            from app import settings

            def test_value():
                assert settings.VALUE == 1
            """,
        )
        self.project.ran("--affected")
        self.project.edit("app/settings.py", "VALUE = 1", "VALUE = 1 + 0")
        self.assertEqual(self.project.ran("--affected"), {"tests/test_settings.py::test_value"})

    def test_conftest_import_time_code_belongs_to_every_test(self) -> None:
        """code run in the global collection context is a dependency of every test."""
        self.project.write("app/config.py", "MODE = 'a'\n")
        self.project.write("tests/conftest.py", "from app import config\n")
        self.project.ran("--affected")
        self.project.edit("app/config.py", "MODE = 'a'", "MODE = 'b'")
        self.assertEqual(self.project.ran("--affected"), ALL)

    def test_conftest_import_moved_into_a_fixture(self) -> None:
        """docs/affected.md: importing the application inside a fixture scopes its import-time code."""
        self.project.write("app/config.py", "MODE = 'a'\n")
        self.project.write(
            "tests/conftest.py",
            """
            import pytest

            @pytest.fixture
            def mode():
                from app import config
                return config.MODE
            """,
        )
        self.project.write("tests/test_mode.py", "def test_mode(mode):\n    assert mode\n")
        self.project.ran("--affected")
        self.project.edit("app/config.py", "MODE = 'a'", "MODE = 'b'")
        self.assertEqual(self.project.ran("--affected"), {"tests/test_mode.py::test_mode"})

    def test_lazy_import_belongs_to_the_test(self) -> None:
        """a module first imported inside a test is that test's dependency."""
        self.project.write("app/lazy.py", "FLAG = True\n")
        self.project.write(
            "tests/test_lazy.py",
            """
            def test_lazy():
                from app import lazy
                assert lazy.FLAG

            def test_other():
                pass
            """,
        )
        self.project.ran("--affected")
        self.project.edit("app/lazy.py", "FLAG = True", "FLAG = 1")
        self.assertEqual(self.project.ran("--affected"), {"tests/test_lazy.py::test_lazy"})

    # --- what always runs ---------------------------------------------------

    def test_failed_tests_rerun_until_they_pass(self) -> None:
        self.project.write("tests/test_flaky.py", "def test_flaky():\n    assert False\n")
        self.project.ran("--affected")
        self.assertEqual(self.project.ran("--affected"), {"tests/test_flaky.py::test_flaky"})
        self.project.write("tests/test_flaky.py", "def test_flaky():\n    assert True\n")
        self.assertEqual(self.project.ran("--affected"), {"tests/test_flaky.py::test_flaky"})
        self.assertEqual(self.project.ran("--affected"), set())

    def test_new_tests_run(self) -> None:
        self.record()
        self.project.edit(
            "tests/test_add.py",
            "def test_add_more",
            "def test_new():\n    pass\n\ndef test_add_more",
        )
        self.assertEqual(self.project.ran("--affected"), {"tests/test_add.py::test_new"})

    def test_skipped_tests_are_recorded(self) -> None:
        self.project.write(
            "tests/test_skip.py",
            "import pytest\n\n@pytest.mark.skip(reason='x')\ndef test_skip():\n    pass\n",
        )
        self.project.ran("--affected")
        self.assertEqual(self.project.ran("--affected"), set())

    def test_explicit_node_ids_always_run(self) -> None:
        self.record()
        self.assertEqual(
            self.project.ran("--affected", "tests/test_add.py::test_add"),
            {"tests/test_add.py::test_add"},
        )

    def test_k_composes_with_selection(self) -> None:
        """-k is an extra filter on top of selection."""
        self.record()
        self.project.edit("app/core.py", "return a + b", "return b + a")
        self.assertEqual(
            self.project.ran("--affected", "-k", "more"), {"tests/test_add.py::test_add_more"}
        )
        # test_add was not run (filtered by -k): still affected next time.
        self.assertEqual(self.project.ran("--affected"), {"tests/test_add.py::test_add"})

    def test_always_run(self) -> None:
        self.project.edit(
            "pyproject.toml",
            'pythonpath = ["."]',
            'pythonpath = ["."]\n\n[tool.pytest-rs.affected]\nalways_run = ["tests/test_mul.py"]',
        )
        self.record()
        self.assertEqual(
            self.project.ran("--affected"),
            {"tests/test_mul.py::test_mul", "tests/test_mul.py::test_box"},
        )

    def test_always_changed(self) -> None:
        self.project.edit(
            "pyproject.toml",
            'pythonpath = ["."]',
            'pythonpath = ["."]\n\n[tool.pytest-rs.affected]\nalways_changed = ["app/core.py"]',
        )
        self.record()
        self.assertEqual(self.project.ran("--affected"), ALL)

    # --- non-Python files -------------------------------------

    def configure(self, toml: str) -> None:
        self.project.edit(
            "pyproject.toml",
            'pythonpath = ["."]',
            'pythonpath = ["."]\n\n[tool.pytest-rs.affected]\n' + textwrap.dedent(toml),
        )

    def test_unrecorded_non_python_change_is_ignored(self) -> None:
        """a file no test read is ignored by default (unknown_files = "ignore")."""
        self.record()
        self.project.write("data/values.json", "{}")
        self.assertEqual(self.project.ran("--affected"), set())

    def test_unknown_files_all_runs_everything(self) -> None:
        self.configure('unknown_files = "all"')
        self.record()
        self.project.write("data/values.json", "{}")
        result = self.project.run("--affected")
        self.assertEqual(self.project.nodeids(result.stdout), ALL)
        self.assertIn("changed: data/values.json", result.stdout)
        self.assertEqual(self.project.ran("--affected"), set())

    def test_ignored_files_change_nothing(self) -> None:
        self.configure('unknown_files = "all"\nignore = ["docs/**"]')
        self.record()
        self.project.write("docs/index.md", "# docs")
        self.assertEqual(self.project.ran("--affected"), set())

    def test_builtin_full_run_list(self) -> None:
        """configuration and environment files always mean a full run."""
        self.record()
        self.project.write("requirements.txt", "attrs\n")
        self.assertEqual(self.project.ran("--affected"), ALL)

    def test_full_run_on_covers_python_files_too(self) -> None:
        self.project.write("scripts/gen.py", "X = 1\n")
        self.configure('full_run_on = ["scripts/**"]')
        self.record()
        self.project.edit("scripts/gen.py", "X = 1", "X = 2")
        self.assertEqual(self.project.ran("--affected"), ALL)

    def test_full_run_with_k_drops_the_tests_it_did_not_run(self) -> None:
        """A full-run trigger with -k: the tests left out must run next time."""
        self.configure('unknown_files = "all"')
        self.record()
        self.project.write("data/values.json", "{}")
        self.assertEqual(
            self.project.ran("--affected", "-k", "test_box"), {"tests/test_mul.py::test_box"}
        )
        self.assertEqual(self.project.ran("--affected"), ALL - {"tests/test_mul.py::test_box"})

    def test_file_read_by_a_test_is_its_dependency(self) -> None:
        """a file opened for reading under the rootdir is a dependency."""
        self.project.write("data/values.json", '{"a": 1}')
        self.project.write(
            "tests/test_data.py",
            """
            import json

            def test_reads():
                with open("data/values.json") as f:
                    assert "a" in json.load(f)

            def test_other():
                pass
            """,
        )
        self.project.ran("--affected")
        self.project.write("data/values.json", '{"a": 2}')
        self.assertEqual(self.project.ran("--affected"), {"tests/test_data.py::test_reads"})

    def test_file_read_at_collection_belongs_to_the_test_file(self) -> None:
        """parametrize data read while importing a test file."""
        self.project.write("data/cases.json", "[1, 2]")
        self.project.write(
            "tests/test_cases.py",
            """
            import json
            import pytest

            with open("data/cases.json") as f:
                CASES = json.load(f)

            @pytest.mark.parametrize("n", CASES)
            def test_case(n):
                assert n > 0
            """,
        )
        self.project.ran("--affected")
        self.project.write("data/cases.json", "[1, 2, 3]")
        self.assertEqual(
            self.project.ran("--affected"),
            {
                "tests/test_cases.py::test_case[1]",
                "tests/test_cases.py::test_case[2]",
                "tests/test_cases.py::test_case[3]",
            },
        )

    def test_directory_listing_is_a_dependency(self) -> None:
        """a directory listed with os.listdir/os.scandir is a dependency."""
        self.project.write("plugins/a.txt", "a")
        self.project.write(
            "tests/test_plugins.py",
            """
            import os

            def test_discovers():
                assert "a.txt" in os.listdir("plugins")

            def test_other():
                pass
            """,
        )
        self.project.ran("--affected")
        self.project.write("plugins/b.txt", "b")
        self.assertEqual(self.project.ran("--affected"), {"tests/test_plugins.py::test_discovers"})

    def test_subprocess_marks_the_test_incomplete(self) -> None:
        """a test that starts a subprocess always runs (incomplete = "run")."""
        self.project.write(
            "tests/test_proc.py",
            """
            import subprocess
            import sys

            def test_spawns():
                subprocess.run([sys.executable, "-c", "pass"], check=True)

            def test_other():
                pass
            """,
        )
        self.project.ran("--affected")
        self.assertEqual(self.project.ran("--affected"), {"tests/test_proc.py::test_spawns"})

    def test_subprocess_outside_tests_says_why_everything_runs(self) -> None:
        """A conftest starting a subprocess at import makes every test incomplete."""
        self.project.write(
            "tests/conftest.py",
            "import subprocess, sys\n\nsubprocess.run([sys.executable, '-c', 'pass'], check=True)\n",
        )
        self.record()
        result = self.project.run("--affected")
        self.assertEqual(self.project.nodeids(result.stdout), ALL)
        self.assertIn("a subprocess was started outside any test", result.stdout)

    def test_incomplete_trust(self) -> None:
        self.configure('incomplete = "trust"')
        self.project.write(
            "tests/test_proc.py",
            """
            import subprocess
            import sys

            def test_spawns():
                subprocess.run([sys.executable, "-c", "pass"], check=True)
            """,
        )
        self.project.ran("--affected")
        self.assertEqual(self.project.ran("--affected"), set())

    def test_file_triggers_tests(self) -> None:
        """a change under a file_triggers key runs the listed test globs."""
        self.project.write("migrations/0001.sql", "create table a;")
        self.configure(
            """
            [tool.pytest-rs.affected.file_triggers]
            "migrations/**" = ["tests/test_mul.py"]
            """
        )
        self.record()
        self.project.write("migrations/0002.sql", "create table b;")
        self.assertEqual(
            self.project.ran("--affected"),
            {"tests/test_mul.py::test_mul", "tests/test_mul.py::test_box"},
        )
        self.assertEqual(self.project.ran("--affected"), set())

    def test_file_triggers_tests_left_out_by_k_run_next_time(self) -> None:
        self.configure(
            """
            [tool.pytest-rs.affected.file_triggers]
            "migrations/**" = ["tests/test_mul.py"]
            """
        )
        self.record()
        self.project.write("migrations/0001.sql", "create table a;")
        self.assertEqual(
            self.project.ran("--affected", "-k", "test_box"), {"tests/test_mul.py::test_box"}
        )
        self.assertEqual(self.project.ran("--affected"), {"tests/test_mul.py::test_mul"})

    def test_file_triggers_all_and_recorded(self) -> None:
        self.configure(
            """
            unknown_files = "all"

            [tool.pytest-rs.affected.file_triggers]
            "assets/**" = "all"
            "notes/**" = "recorded"
            """
        )
        self.record()
        self.project.write("notes/todo.txt", "x")
        self.assertEqual(self.project.ran("--affected"), set())
        self.project.write("assets/logo.svg", "<svg/>")
        self.assertEqual(self.project.ran("--affected"), ALL)

    # --- pre-import selection and cache-hit imports -----------------------------

    def test_unchanged_test_files_are_not_imported(self) -> None:
        marker = self.project.base.parent / f"{self.project.base.name}-imports.log"
        self.project.write(
            "tests/test_marker.py",
            f"""
            with open({str(marker)!r}, "a") as f:
                f.write("imported\\n")

            def test_marker():
                pass
            """,
        )
        self.project.ran("--affected")
        self.project.ran("--affected")
        self.project.ran("--affected")
        self.assertEqual(marker.read_text().splitlines(), ["imported"])
        marker.unlink()

    def test_test_file_edit_reimports_only_that_file(self) -> None:
        self.record()
        self.project.edit("tests/test_add.py", "assert add(2, 2) == 4", "assert add(2, 2) == 2 + 2")
        result = self.project.run("--affected")
        self.assertEqual(self.project.nodeids(result.stdout), {"tests/test_add.py::test_add_more"})
        self.assertIn("1 of 2 test files unchanged, not imported", result.stdout)

    def test_new_test_file_runs(self) -> None:
        self.record()
        self.project.write("tests/test_new.py", "def test_new():\n    pass\n")
        self.assertEqual(self.project.ran("--affected"), {"tests/test_new.py::test_new"})

    def test_static_import_of_an_already_imported_module(self) -> None:
        """a module another test file imported first is a cache hit."""
        self.project.write("app/consts.py", "SIZE = 3\n")
        self.project.write(
            "tests/test_a_first.py",
            "from app import consts\n\ndef test_first():\n    assert consts.SIZE\n",
        )
        self.project.write(
            "tests/test_b_second.py",
            "from app.consts import SIZE\n\ndef test_second():\n    assert SIZE == 3\n",
        )
        self.project.ran("--affected")
        self.project.edit("app/consts.py", "SIZE = 3", "SIZE = 4")
        self.assertEqual(
            self.project.ran("--affected"),
            {"tests/test_a_first.py::test_first", "tests/test_b_second.py::test_second"},
        )

    # --- -n and pytest-split -----------------------------------

    def test_n_records_and_selects(self) -> None:
        """Workers select on their own and their recordings merge in the controller."""
        for env in ({}, {"PYTEST_RS_DIST_SPAWN": "1"}):
            with self.subTest(spawn=bool(env)):
                data = self.project.base / ".pytest-rs-affected"
                data.unlink(missing_ok=True)
                self.assertEqual(self.project.ran("--affected", "-n", "2", env=env), ALL)
                result = self.project.run("--affected", "-n", "2", env=env)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(self.project.nodeids(result.stdout), set())
                self.project.edit("app/core.py", "return a * b", "return b * a")
                self.assertEqual(
                    self.project.ran("--affected", "-n", "2", env=env),
                    {"tests/test_mul.py::test_mul"},
                )
                self.project.edit("app/core.py", "return b * a", "return a * b")
                leftovers = [p.name for p in self.project.base.iterdir() if ".worker-" in p.name]
                self.assertEqual(leftovers, [])

    def test_n_recording_serves_a_serial_run(self) -> None:
        self.assertEqual(self.project.ran("--affected", "-n", "2"), ALL)
        self.project.edit("app/core.py", "return a + b", "return b + a")
        self.assertEqual(
            self.project.ran("--affected=select"),
            {"tests/test_add.py::test_add", "tests/test_add.py::test_add_more"},
        )

    def test_split_divides_the_selected_tests(self) -> None:
        """Selection runs before pytest-split: the groups split the selected set."""
        self.record()
        self.project.edit("app/core.py", "return a + b", "return b + a")
        self.project.edit("app/core.py", "return a * b", "return b * a")
        selected = {
            "tests/test_add.py::test_add",
            "tests/test_add.py::test_add_more",
            "tests/test_mul.py::test_mul",
        }
        for granularity in ("test", "file"):
            with self.subTest(granularity=granularity):
                groups = [
                    self.project.ran(
                        "--affected=select",
                        "--splits",
                        "2",
                        "--group",
                        str(group),
                        f"--splitting-granularity={granularity}",
                    )
                    for group in (1, 2)
                ]
                self.assertEqual(groups[0] | groups[1], selected)
                self.assertEqual(groups[0] & groups[1], set())
                self.assertTrue(groups[0] and groups[1], groups)

    def test_split_starts_no_subprocess_after_recording_starts(self) -> None:
        """pytest-split, configured after --affected starts recording, must not make tests incomplete."""
        self.assertEqual(
            self.project.ran("--affected", "--splits", "1", "--group", "1", "--store-durations"),
            ALL,
        )
        self.assertEqual(
            self.project.ran("--affected", "--splits", "1", "--group", "1", "--store-durations"),
            set(),
        )

    # --- git fast path ------------------------------------------------------

    def git(self, *args: str) -> None:
        subprocess.run(
            ["git", *args],
            cwd=self.project.base,
            check=True,
            capture_output=True,
            env={
                **os.environ,
                "GIT_AUTHOR_NAME": "t",
                "GIT_AUTHOR_EMAIL": "t@example.invalid",
                "GIT_COMMITTER_NAME": "t",
                "GIT_COMMITTER_EMAIL": "t@example.invalid",
            },
        )

    def commit_all(self) -> None:
        # Files modified in the same second the index is written are
        # "racily clean": git re-reads them to compare. Age them first, so
        # git trusts its stat cache like in a real checkout.
        past = time.time() - 60
        for path in self.project.base.rglob("*"):
            if path.is_file() and ".git" not in path.parts:
                os.utime(path, (past, past))
        self.git("add", "-A")
        self.git("commit", "-qm", "c")

    def setup_repo(self) -> None:
        (self.project.base / ".gitignore").write_text(".pytest-rs-affected*\ndata/\n")
        self.git("init", "-q")
        # chmod (below) changes only the ctime: git must not re-read the
        # file for it, or it reads it as changed itself.
        self.git("config", "core.trustctime", "false")
        self.commit_all()

    def test_git_vouched_files_are_not_read(self) -> None:
        """A tracked file unchanged since its recorded commit is not read at all."""
        self.setup_repo()
        self.record()
        core = self.project.base / "app" / "core.py"
        core.chmod(0)
        try:
            self.assertEqual(self.project.ran("--affected=select"), set())
        finally:
            core.chmod(0o644)

    def test_git_does_not_hide_changes(self) -> None:
        self.setup_repo()
        self.record()
        # Unstaged, then committed: the same change is seen either way.
        self.project.edit("app/core.py", "return a * b", "return b * a")
        self.assertEqual(self.project.ran("--affected=select"), {"tests/test_mul.py::test_mul"})
        self.commit_all()
        self.assertEqual(self.project.ran("--affected=select"), {"tests/test_mul.py::test_mul"})

    def test_git_ignored_inputs_are_hashed(self) -> None:
        self.setup_repo()
        self.project.write("data/values.json", '{"a": 1}')
        self.project.write(
            "tests/test_data.py",
            "def test_reads():\n    assert open('data/values.json').read()\n",
        )
        self.commit_all()
        self.project.ran("--affected")
        self.project.write("data/values.json", '{"a": 2}')
        self.assertEqual(self.project.ran("--affected=select"), {"tests/test_data.py::test_reads"})

    def test_git_dirty_files_are_hashed(self) -> None:
        """A file recorded with uncommitted edits names no commit; reverting it is a change."""
        self.setup_repo()
        self.project.edit("app/core.py", "return a * b", "return b * a")
        self.record()
        self.git("checkout", "--", "app/core.py")
        self.assertEqual(self.project.ran("--affected=select"), {"tests/test_mul.py::test_mul"})

    # --- sharded CI and --affected-merge -----------------------------

    def shard(self, group: int, data: str) -> set[str]:
        return self.project.ran(
            "--affected", "--affected-data", data, "--splits", "2", "--group", str(group)
        )

    def merge(self, *args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [str(BINARY), "--affected-merge", *args],
            cwd=self.project.base,
            capture_output=True,
            text=True,
        )

    def test_merge_sharded_recordings(self) -> None:
        """Each shard records its group; the merged data serves every later run."""
        ran = self.shard(1, "s1.data") | self.shard(2, "s2.data")
        self.assertEqual(ran, ALL)
        result = self.merge("s1.data", "s2.data", "--affected-data", ".pytest-rs-affected")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("merged 2 files (4 tests)", result.stdout)
        self.assertEqual(self.project.ran("--affected=select"), set())
        self.project.edit("app/core.py", "return a * b", "return b * a")
        self.assertEqual(self.project.ran("--affected=select"), {"tests/test_mul.py::test_mul"})

    def test_merge_keeps_each_shards_new_entries(self) -> None:
        """Shards start from the same restored data: the newer entry of a test wins.

        Merging in either order must keep what each shard just recorded, not the
        stale copy the other shard carried over from the restored data.
        """
        self.record()
        base = (self.project.base / ".pytest-rs-affected").read_bytes()
        # Both shards fail their test after this edit: the newer entries say
        # "failed", the restored copies say "passed".
        self.project.edit("app/core.py", "return a + b", "return a + b + 1")
        self.project.edit("app/core.py", "return a * b", "return a * b + 1")
        for group, data in ((1, "s1.data"), (2, "s2.data")):
            (self.project.base / data).write_bytes(base)
            self.shard(group, data)
        self.project.edit("app/core.py", "return a + b + 1", "return a + b")
        self.project.edit("app/core.py", "return a * b + 1", "return a * b")
        for order in (("s1.data", "s2.data"), ("s2.data", "s1.data")):
            with self.subTest(order=order):
                result = self.merge(*order, "--affected-data", "merged.data")
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                # Every test that failed in its shard reruns.
                self.assertEqual(
                    self.project.ran("--affected=select", "--affected-data", "merged.data"),
                    {
                        "tests/test_add.py::test_add",
                        "tests/test_add.py::test_add_more",
                        "tests/test_mul.py::test_mul",
                    },
                )

    def test_merge_usage_errors(self) -> None:
        self.record()
        missing_output = self.merge(".pytest-rs-affected")
        self.assertEqual(missing_output.returncode, 4, missing_output.stderr)
        self.assertIn("--affected-data", missing_output.stderr)
        (self.project.base / "junk.data").write_text("junk")
        junk = self.merge(".pytest-rs-affected", "junk.data", "--affected-data", "out.data")
        self.assertEqual(junk.returncode, 4, junk.stderr)
        self.assertIn("junk.data", junk.stderr)
        self.assertFalse((self.project.base / "out.data").exists())

    # --- modes --------------------------------------------------------------

    def test_select_mode_does_not_record(self) -> None:
        self.record()
        self.project.edit("app/core.py", "return a * b", "return b * a")
        self.assertEqual(self.project.ran("--affected=select"), {"tests/test_mul.py::test_mul"})
        self.assertEqual(self.project.ran("--affected=select"), {"tests/test_mul.py::test_mul"})

    def test_record_mode_runs_everything_and_records(self) -> None:
        self.record()
        self.assertEqual(self.project.ran("--affected=record"), ALL)
        self.project.edit("app/core.py", "return a * b", "return b * a")
        self.assertEqual(self.project.ran("--affected=select"), {"tests/test_mul.py::test_mul"})

    def test_data_path_option(self) -> None:
        self.assertEqual(self.project.ran("--affected", "--affected-data", "out/data.bin"), ALL)
        self.assertTrue((self.project.base / "out/data.bin").exists())
        self.assertFalse((self.project.base / ".pytest-rs-affected").exists())
        self.assertEqual(self.project.ran("--affected", "--affected-data", "out/data.bin"), set())

    def test_unreadable_data_runs_everything(self) -> None:
        (self.project.base / ".pytest-rs-affected").write_bytes(b"garbage")
        result = self.project.run("--affected")
        self.assertEqual(self.project.nodeids(result.stdout), ALL)
        self.assertIn("unreadable data", result.stdout)


if __name__ == "__main__":
    unittest.main()
