"""The header's `plugins:` line lists the built-in plugins a run uses.

Upstream lists installed plugins (`cov-7.1.0`); pytest-rs reimplements
pytest-asyncio, pytest-cov, pytest-xdist, pytest-split, pytest-benchmark,
anyio and pytest-mock natively, so they never appeared. Now each is listed as
`rs-<name>-<pytest-rs version>` when the run uses it (as far as options and
ini tell before collection), and pytest-asyncio's `asyncio: mode=...` line
follows, like upstream.
"""

from __future__ import annotations

import os
import re
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).parent.parent
# PYTEST_RS_BIN selects a binary built for another Python (see conformance/runner.py).
BINARY = Path(os.environ.get("PYTEST_RS_BIN") or ROOT / "target" / "debug" / "pytest-rs-bin")


class TestHeaderBuiltinPlugins(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if not BINARY.exists():
            raise unittest.SkipTest("debug binary not built")

    def header(self, *args: str, ini: str = "") -> list[str]:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp)
            (path / "test_a.py").write_text("def test_a():\n    pass\n")
            if ini:
                (path / "pytest.ini").write_text(ini)
            out = subprocess.run(
                [str(BINARY), "-p", "no:cacheprovider", *args],
                cwd=path,
                capture_output=True,
                text=True,
            ).stdout
        lines = out.split("\n")
        end = next(i for i, line in enumerate(lines) if line.startswith(("collected", "2 workers")))
        return [re.sub(r"-\d+\.\d+\.\d+[^,\s]*", "-V", line) for line in lines[:end]]

    def test_nothing_in_use(self) -> None:
        lines = self.header()
        self.assertFalse(
            any(line.startswith(("plugins:", "asyncio:", "benchmark:")) for line in lines), lines
        )

    def test_in_use(self) -> None:
        lines = self.header(
            "--cov=.",
            "--splits",
            "2",
            "--group",
            "1",
            "--benchmark-disable",
            ini="[pytest]\nasyncio_mode = auto\n",
        )
        plugins = next(line for line in lines if line.startswith("plugins:"))
        self.assertEqual(
            sorted(plugins.removeprefix("plugins: ").split(", ")),
            ["rs-asyncio-V", "rs-benchmark-V", "rs-cov-V", "rs-split-V"],
            lines,
        )
        # pytest-asyncio's tryfirst header line follows the plugins line.
        self.assertEqual(
            lines[lines.index(plugins) + 1],
            "asyncio: mode=Mode.AUTO, debug=False, asyncio_default_fixture_loop_scope=None, "
            "asyncio_default_test_loop_scope=function",
        )

    def test_xdist(self) -> None:
        plugins = [line for line in self.header("-n", "2") if line.startswith("plugins:")]
        self.assertEqual(plugins, ["plugins: rs-xdist-V"])


if __name__ == "__main__":
    unittest.main()
