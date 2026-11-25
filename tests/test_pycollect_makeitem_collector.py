"""pytest_pycollect_makeitem gets a real Module / Class collector (#134).

Upstream passes the PyCollector that collects the member: plugins call its
name filters (anyio's `collector.istestfunction`, aiohttp's
`funcnamefilter` / `_genfunctions`), read `collector.config` and its
markers. It is called for class members too, and an exception from a
hookimpl is a collection error of the module.
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
import pytest

pytestmark = pytest.mark.slow


def test_a():
    pass


def helper():
    pass


@pytest.mark.parametrize("x", [1, 2])
def test_gen(x):
    pass


async def test_async():
    pass


class TestC:
    def test_m(self):
        pass

    @staticmethod
    def test_s():
        pass
"""


class TestPycollectMakeitemCollector(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if not BINARY.exists():
            raise unittest.SkipTest("debug binary not built")

    def run_pytest(self, conftest: str, *args: str) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp)
            (path / "conftest.py").write_text(textwrap.dedent(conftest))
            (path / "test_m.py").write_text(textwrap.dedent(TESTS))
            return subprocess.run(
                [str(BINARY), "-p", "no:cacheprovider", "-p", "no:asyncio", "-q", "-s", *args],
                cwd=path,
                capture_output=True,
                text=True,
            )

    def test_collector_api(self) -> None:
        result = self.run_pytest(
            """
            import inspect
            import pytest

            @pytest.hookimpl(tryfirst=True)
            def pytest_pycollect_makeitem(collector, name, obj):
                if collector.istestfunction(obj, name):
                    print(
                        f"\\nMI {type(collector).__name__} {collector.nodeid} {name}"
                        f" module={collector.module.__name__} cls={collector.cls is not None}"
                        f" slow={collector.get_closest_marker('slow') is not None}"
                        f" Module={isinstance(collector, pytest.Module)}"
                        f" Class={isinstance(collector, pytest.Class)}"
                    )
                    if inspect.iscoroutinefunction(obj):
                        # anyio's plugin does this to its async tests.
                        pytest.mark.usefixtures("marker_fixture")(obj)
                if collector.funcnamefilter(name) and name == "test_gen":
                    # aiohttp's plugin returns the default items.
                    return list(collector._genfunctions(name, obj))

            @pytest.fixture
            def marker_fixture():
                print("\\nFIXTURE")

            def pytest_pyfunc_call(pyfuncitem):
                if inspect.iscoroutinefunction(pyfuncitem.obj):
                    return True
            """
        )
        output = f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        self.assertEqual(result.returncode, 0, output)
        lines = sorted(line for line in result.stdout.splitlines() if line.startswith("MI "))
        self.assertEqual(
            lines,
            [
                "MI Class test_m.py::TestC test_m module=test_m cls=True slow=True Module=False Class=True",
                "MI Class test_m.py::TestC test_s module=test_m cls=True slow=True Module=False Class=True",
                "MI Module test_m.py test_a module=test_m cls=False slow=True Module=True Class=False",
                "MI Module test_m.py test_async module=test_m cls=False slow=True Module=True Class=False",
                "MI Module test_m.py test_gen module=test_m cls=False slow=True Module=True Class=False",
            ],
            output,
        )
        self.assertIn("FIXTURE", result.stdout, output)
        self.assertIn("6 passed", result.stdout, output)

    def test_exception_is_a_collection_error(self) -> None:
        result = self.run_pytest(
            "def pytest_pycollect_makeitem(collector, name, obj):\n    raise AttributeError('boom')\n"
        )
        output = f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        self.assertEqual(result.returncode, 2, output)
        self.assertIn("ERROR collecting test_m.py", result.stdout, output)
        self.assertIn("ERROR test_m.py - AttributeError: boom", result.stdout, output)


if __name__ == "__main__":
    unittest.main()
