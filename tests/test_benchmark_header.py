"""The built-in benchmark plugin's header line only when it is in use.

pytest-benchmark prints `benchmark: <version> (defaults: ...)` because it is
installed. pytest-rs ships the plugin built in, so it printed the line in
every run; now it prints it only when a `--benchmark-*` option is given (on
the command line or in addopts) — the header is written before collection,
so a test requesting the `benchmark` fixture cannot count.
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


class TestBenchmarkHeader(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if not BINARY.exists():
            raise unittest.SkipTest("debug binary not built")

    def header(self, *args: str, ini: str = "") -> str:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp)
            (path / "test_a.py").write_text("def test_b(benchmark):\n    benchmark(lambda: 1)\n")
            if ini:
                (path / "pytest.ini").write_text(ini)
            result = subprocess.run(
                [str(BINARY), "-p", "no:cacheprovider", *args],
                cwd=path,
                capture_output=True,
                text=True,
            )
        return result.stdout.split("collected", 1)[0]

    def test_not_in_use(self) -> None:
        self.assertNotIn("benchmark:", self.header())

    def test_option_on_the_command_line(self) -> None:
        self.assertIn("\nbenchmark: ", self.header("--benchmark-disable"))

    def test_option_in_addopts(self) -> None:
        self.assertIn("\nbenchmark: ", self.header(ini="[pytest]\naddopts = --benchmark-skip\n"))


if __name__ == "__main__":
    unittest.main()
