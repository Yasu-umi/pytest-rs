"""Regression tests for report/collection details that differed from pytest 9.2.

- A skip-marked test reports its teardown phase too (setup skipped, teardown
  passed), so pytest_runtest_logreport sees both.
- Under -n, the controller's conftest pytest_runtest_logreport receives every
  report (xdist's DSession.worker_testreport), not only the workers'.
- Explicit pytest.param ids and bytes values are escaped like ascii_escaped
  ("a\\nb", "by\\nte").
- A value of pytest's own options (`--basetemp DIR`) is not a rootdir arg.
- `addopts = -q` plus `-v` is verbosity 0 (-v and -q share one counter).
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

LOGREPORT_CONFTEST = (
    "import os\n"
    "def pytest_runtest_logreport(report):\n"
    "    with open(os.path.join(os.path.dirname(__file__), 'lr.log'), 'a') as f:\n"
    "        who = 'worker' if os.environ.get('PYTEST_XDIST_WORKER') else 'controller'\n"
    "        f.write(f'{who} {report.nodeid} {report.when} {report.outcome}\\n')\n"
)


class TestReportParity(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if not BINARY.exists():
            raise unittest.SkipTest("debug binary not built")

    def run_rs(self, cwd: Path, *args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [str(BINARY), "-p", "no:cacheprovider", *args],
            cwd=cwd,
            capture_output=True,
            text=True,
        )

    def test_skip_mark_reports_teardown(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            (tmp_path / "conftest.py").write_text(LOGREPORT_CONFTEST)
            (tmp_path / "test_s.py").write_text(
                "import pytest\n@pytest.mark.skip(reason='r')\ndef test_s():\n    pass\n"
            )
            result = self.run_rs(tmp_path, "-q")
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertEqual(
                (tmp_path / "lr.log").read_text().splitlines(),
                [
                    "controller test_s.py::test_s setup skipped",
                    "controller test_s.py::test_s teardown passed",
                ],
            )

    def test_controller_receives_logreport_under_n(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            (tmp_path / "conftest.py").write_text(LOGREPORT_CONFTEST)
            (tmp_path / "test_a.py").write_text(
                "def test_a():\n    pass\ndef test_b():\n    pass\n"
            )
            result = self.run_rs(tmp_path, "-q", "-n", "2")
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            whos = [line.split()[0] for line in (tmp_path / "lr.log").read_text().splitlines()]
            self.assertEqual(whos.count("controller"), 6)
            self.assertEqual(whos.count("worker"), 6)

    def test_param_ids_are_escaped(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            (tmp_path / "test_p.py").write_text(
                "import pytest\n"
                "@pytest.mark.parametrize('v', [pytest.param(1, id='a\\nb')])\n"
                "def test_id(v):\n    pass\n"
                "@pytest.mark.parametrize('v', [b'by\\nte'])\n"
                "def test_bytes(v):\n    pass\n"
            )
            result = self.run_rs(tmp_path, "-q", "--co")
            self.assertIn("test_p.py::test_id[a\\nb]", result.stdout)
            self.assertIn("test_p.py::test_bytes[by\\nte]", result.stdout)

    def test_core_option_value_is_not_a_rootdir_arg(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp).resolve()
            proj = tmp_path / "proj"
            proj.mkdir()
            (tmp_path / "bt").mkdir()
            (proj / "pytest.ini").write_text("[pytest]\n")
            (proj / "test_a.py").write_text("def test_a():\n    pass\n")
            result = self.run_rs(proj, "--basetemp", "../bt")
            self.assertIn(f"rootdir: {proj}\n", result.stdout)

    def test_addopts_quiet_plus_verbose_is_level_zero(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            (tmp_path / "pytest.ini").write_text("[pytest]\naddopts = -q\n")
            (tmp_path / "test_a.py").write_text("def test_a():\n    pass\n")
            result = self.run_rs(tmp_path, "-v")
            self.assertIn("collected 1 item", result.stdout)
            self.assertIn("test_a.py .", result.stdout)


if __name__ == "__main__":
    unittest.main()
