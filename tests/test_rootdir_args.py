"""Regression test: rootdir and configfile with plugin option values, against pytest 9.2.

pytest determines the rootdir before plugins register their options, so the
value of a plugin option given as a separate arg (pytest-split's
`--durations-path X`) counts as an arg. When the args' common ancestor has
no config file above it, determine_setup then tries a setup.py in that chain,
then each arg's own chain, so `--durations-path /elsewhere/d.json test_a.py`
still finds the project's pytest.ini. pytest-rs lacked those two steps and
rooted at the common ancestor without a configfile (asyncio_mode etc. lost).
Expected values are pytest 9.2.0 + pytest-split 0.11.0's. The upstream pytest
suite has no test giving a plugin option value outside the project, which is
why conformance did not catch it.
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


class TestRootdirArgs(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if not BINARY.exists():
            raise unittest.SkipTest("debug binary not built")

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.base = Path(self._tmp.name).resolve()
        (self.base / "proj").mkdir()
        (self.base / "proj" / "pytest.ini").write_text("[pytest]\n")
        (self.base / "proj" / "test_a.py").write_text("def test_x():\n    pass\n")
        (self.base / "proj" / "d2.json").write_text("{}")
        (self.base / "out").mkdir()
        (self.base / "out" / "d.json").write_text("{}")

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def header(self, cwd: Path, *args: str) -> tuple[str, str | None]:
        result = subprocess.run(
            [str(BINARY), "-p", "no:cacheprovider", *args],
            cwd=cwd,
            capture_output=True,
            text=True,
        )
        rootdir = re.search(r"^rootdir: (.*)$", result.stdout, re.M)
        configfile = re.search(r"^configfile: (.*)$", result.stdout, re.M)
        self.assertIsNotNone(rootdir, result.stdout + result.stderr)
        assert rootdir is not None
        return rootdir.group(1), configfile.group(1) if configfile else None

    def test_plugin_option_value_outside_the_project(self) -> None:
        proj = self.base / "proj"
        value = str(self.base / "out" / "d.json")
        cases = [
            ("--durations-path", value, "test_a.py"),
            (f"--durations-path={value}", "test_a.py"),
            ("--durations-path", "d2.json", "test_a.py"),
        ]
        for args in cases:
            with self.subTest(args=args):
                self.assertEqual(self.header(proj, *args), (str(proj), "pytest.ini"))

    def test_plugin_option_value_only(self) -> None:
        # The value is the only arg: no config above it, so pytest roots at
        # the common ancestor of the invocation dir and the value's dir.
        proj = self.base / "proj"
        value = str(self.base / "out" / "d.json")
        self.assertEqual(self.header(proj, "--durations-path", value), (str(self.base), None))

    def test_setup_py_marks_the_rootdir(self) -> None:
        project = self.base / "setuppy"
        (project / "pkg").mkdir(parents=True)
        (project / "setup.py").touch()
        (project / "pkg" / "test_s.py").write_text("def test_x():\n    pass\n")
        self.assertEqual(self.header(project / "pkg"), (str(project), None))


if __name__ == "__main__":
    unittest.main()
