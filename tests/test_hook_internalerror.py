"""An exception from a reporting hookimpl ends the session with INTERNALERROR.

Upstream calls `pytest_runtest_makereport`, `pytest_runtest_logreport`,
`pytest_report_teststatus`, `pytest_runtest_logfinish` and
`pytest_warning_recorded` with no try/except: the exception reaches
wrap_session, which prints it as INTERNALERROR and exits with 3. pytest-rs
printed it to stderr and carried on (exit code 0).

Also `Function.funcargs`: the fixture values by name, filled by setup.
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

TESTS = """
import warnings


def test_ok():
    warnings.warn(UserWarning("w"))
"""


class TestHookInternalError(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if not BINARY.exists():
            raise unittest.SkipTest("debug binary not built")

    def run_pytest(self, conftest: str, tests: str = TESTS) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp)
            (path / "conftest.py").write_text(textwrap.dedent(conftest))
            (path / "test_x.py").write_text(textwrap.dedent(tests))
            return subprocess.run(
                [str(BINARY), "-q", "-s", "-p", "no:cacheprovider"],
                cwd=path,
                capture_output=True,
                text=True,
            )

    def assert_internal_error(self, hook: str, args: str) -> None:
        result = self.run_pytest(f"def {hook}({args}):\n    raise AttributeError('boom in hook')\n")
        output = f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        self.assertEqual(result.returncode, 3, output)
        self.assertIn("INTERNALERROR> AttributeError: boom in hook", result.stdout, output)
        self.assertNotIn("passed", result.stdout, output)

    def test_makereport(self) -> None:
        self.assert_internal_error("pytest_runtest_makereport", "item, call")

    def test_logreport(self) -> None:
        self.assert_internal_error("pytest_runtest_logreport", "report")

    def test_report_teststatus(self) -> None:
        self.assert_internal_error("pytest_report_teststatus", "report")

    def test_logfinish(self) -> None:
        self.assert_internal_error("pytest_runtest_logfinish", "nodeid")

    def test_warning_recorded(self) -> None:
        self.assert_internal_error("pytest_warning_recorded", "warning_message")

    def test_exit_is_not_an_internal_error(self) -> None:
        result = self.run_pytest(
            "import pytest\n\ndef pytest_runtest_logreport(report):\n    pytest.exit('bye', 4)\n"
        )
        self.assertNotIn("INTERNALERROR", result.stdout + result.stderr)

    def test_funcargs(self) -> None:
        result = self.run_pytest(
            """
            import pytest

            @pytest.fixture(autouse=True)
            def auto():
                return "A"

            @pytest.fixture
            def base():
                return 1

            @pytest.fixture
            def dep(base):
                return base + 1

            def pytest_runtest_makereport(item, call):
                print(f"\\n{call.when} {sorted(item.funcargs.items())}")

            def pytest_runtest_logfinish(nodeid):
                print("\\nGONE", ITEMS[0].funcargs)

            ITEMS = []

            def pytest_collection_modifyitems(items):
                ITEMS.extend(items)
                print("\\nBEFORE", items[0].funcargs)
            """,
            """
            import pytest

            @pytest.mark.parametrize("x", [7])
            def test_p(x, dep, request):
                assert request.node.funcargs["dep"] == 2
            """,
        )
        output = f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        self.assertEqual(result.returncode, 0, output)
        self.assertIn("BEFORE {}", result.stdout, output)
        for when in ("setup", "call", "teardown"):
            self.assertRegex(
                result.stdout,
                rf"{when} \[\('auto', 'A'\), \('base', 1\), \('dep', 2\), "
                r"\('request', <FixtureRequest for <Function test_p\[7\]>>\), \('x', 7\)\]",
            )
        self.assertIn("GONE None", result.stdout, output)


if __name__ == "__main__":
    unittest.main()
