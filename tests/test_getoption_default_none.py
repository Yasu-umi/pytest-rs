"""Regression test: `config.getoption(name, default=None)` for an unknown option.

pytest returns the given default — None included — when the option is not
registered. pytest-rs used to treat an explicit `default=None` as "no default"
and raised `ValueError: no option named ...` (anyio 4.14+ reads its
`--anyio-mode` this way).
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


class TestGetoptionDefaultNone(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if not BINARY.exists():
            raise unittest.SkipTest("debug binary not built")

    def test_explicit_none_default_is_returned(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            (tmp_path / "test_opt.py").write_text(
                "import pytest\n"
                "\n"
                "def test_it(pytestconfig):\n"
                "    assert pytestconfig.getoption('no_such_option', default=None) is None\n"
                "    assert pytestconfig.getoption('no_such_option', None) is None\n"
                "    assert pytestconfig.getoption('no_such_option', 3) == 3\n"
                "    with pytest.raises(ValueError, match='no option named'):\n"
                "        pytestconfig.getoption('no_such_option')\n"
            )
            result = subprocess.run(
                [str(BINARY), "test_opt.py", "-q", "-p", "no:cacheprovider"],
                cwd=tmp_path,
                capture_output=True,
                text=True,
            )
            self.assertEqual(
                result.returncode,
                0,
                f"pytest-rs exited {result.returncode}\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("1 passed", result.stdout)


if __name__ == "__main__":
    unittest.main()
