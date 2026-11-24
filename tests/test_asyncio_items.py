"""pytest-asyncio items like upstream: their class and their tracebacks.

- An asyncio-run `async def` test is a `pytest_asyncio.plugin.Coroutine`
  item from collection on (`str(item)` is `<Coroutine test_x>`, which
  pytest-split's least_duration sorts duration ties by).
- A failing async test's traceback starts at the test's own frame, without
  the event loop frames that run it.
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

INI = """
[pytest]
asyncio_mode = {mode}
asyncio_default_fixture_loop_scope = function
"""


class TestAsyncioItems(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if not BINARY.exists():
            raise unittest.SkipTest("debug binary not built")

    def run_pytest(self, files: dict[str, str], *args: str) -> str:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp)
            for name, content in files.items():
                (path / name).write_text(textwrap.dedent(content).lstrip("\n"))
            result = subprocess.run(
                [str(BINARY), "-p", "no:cacheprovider", "-q", "-s", *args],
                cwd=path,
                capture_output=True,
                text=True,
            )
            return result.stdout + result.stderr

    def test_coroutine_items(self) -> None:
        files = {
            "conftest.py": """
                import pytest_asyncio.plugin as plugin

                def pytest_collection_modifyitems(items):
                    print("\\nITEMS", [(str(i), isinstance(i, plugin.Coroutine)) for i in items])
            """,
            "test_x.py": """
                import pytest

                def test_a():
                    pass

                async def test_b():
                    pass

                @pytest.mark.asyncio
                async def test_c():
                    pass
            """,
        }
        out = self.run_pytest({**files, "pytest.ini": INI.format(mode="auto")})
        self.assertIn(
            "ITEMS [('<Function test_a>', False), ('<Coroutine test_b>', True), "
            "('<Coroutine test_c>', True)]",
            out,
        )
        out = self.run_pytest({**files, "pytest.ini": INI.format(mode="strict")})
        self.assertIn(
            "ITEMS [('<Function test_a>', False), ('<Function test_b>', False), "
            "('<Coroutine test_c>', True)]",
            out,
        )

    def test_split_ties_sort_by_coroutine_repr(self) -> None:
        files = {
            "pytest.ini": INI.format(mode="auto"),
            "test_x.py": """
                def test_a():
                    pass

                async def test_b():
                    pass

                def test_c():
                    pass

                async def test_d():
                    pass
            """,
        }
        args = ("--splits", "2", "--splitting-algorithm", "least_duration", "--collect-only")
        # Upstream sorts the reprs: <Coroutine test_b>, <Coroutine test_d>,
        # <Function test_a>, <Function test_c>, then deals them round-robin.
        self.assertIn(
            "test_x.py::test_a\ntest_x.py::test_b\n", self.run_pytest(files, *args, "--group", "1")
        )
        self.assertIn(
            "test_x.py::test_c\ntest_x.py::test_d\n", self.run_pytest(files, *args, "--group", "2")
        )

    def test_traceback_starts_at_the_test(self) -> None:
        out = self.run_pytest(
            {
                "pytest.ini": INI.format(mode="auto"),
                "test_x.py": """
                    import asyncio

                    async def inner():
                        await asyncio.sleep(0)
                        assert 1 == 2

                    async def test_fail():
                        await inner()
                """,
            },
            "--tb=short",
        )
        self.assertIn(
            "test_x.py:8: in test_fail\n    await inner()\ntest_x.py:5: in inner\n    assert 1 == 2\n",
            out,
        )
        self.assertNotIn("helper.py", out)
        self.assertNotIn("base_events.py", out)


if __name__ == "__main__":
    unittest.main()
