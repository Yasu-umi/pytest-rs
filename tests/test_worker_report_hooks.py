"""Regression test: outcomes under -n with conftest report hooks, against pytest 9.2.

A -n worker never registered the core pytest_runtest_makereport impl, so a
conftest makereport hook made the `_pytest.runner` fallback rebuild each
report from `call`: a skip-marked test's setup (no exception) became a pass
and the skip vanished from the summary and the junit report. The expected
summary line and per-test junit outcomes below are pytest 9.2 + xdist's,
for every combination of hook and -n/serial. The upstream pytest and xdist
suites have no test combining -n with a conftest report hook, which is
why conformance did not catch it.
"""

from __future__ import annotations

import os
import subprocess
import tempfile
import unittest
import xml.etree.ElementTree as ET
from pathlib import Path

ROOT = Path(__file__).parent.parent
# PYTEST_RS_BIN selects a binary built for another Python (see conformance/runner.py).
BINARY = Path(os.environ.get("PYTEST_RS_BIN") or ROOT / "target" / "debug" / "pytest-rs-bin")

TESTS = """\
import pytest
@pytest.mark.skip(reason="x")
def test_s():
    pass
def test_skip_call():
    pytest.skip("in call")
@pytest.mark.xfail(reason="xf")
def test_xfail():
    assert 0
@pytest.mark.xfail(reason="xp")
def test_xpass():
    pass
@pytest.mark.xfail(reason="strict", strict=True)
def test_xpass_strict():
    pass
@pytest.fixture
def broken():
    raise RuntimeError("setup boom")
def test_error(broken):
    pass
def test_fail():
    assert 0
def test_p():
    pass
"""

HOOKS = {
    "makereport": "def pytest_runtest_makereport(item, call):\n    pass\n",
    "makereport-wrapper": (
        "import pytest\n"
        "@pytest.hookimpl(wrapper=True)\n"
        "def pytest_runtest_makereport(item, call):\n"
        "    rep = yield\n"
        "    return rep\n"
    ),
    "logreport": "def pytest_runtest_logreport(report):\n    pass\n",
    "none": "",
}

# pytest 9.2.0 + pytest-xdist, identical for every hook and with or without -n.
SUMMARY = "2 failed, 1 passed, 2 skipped, 1 xfailed, 1 xpassed, 1 error"
OUTCOMES = {
    "test_error": "error",
    "test_fail": "failure",
    "test_p": "passed",
    "test_s": "skipped",
    "test_skip_call": "skipped",
    "test_xfail": "skipped",
    "test_xpass": "passed",
    "test_xpass_strict": "failure",
}


class TestWorkerReportHooks(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if not BINARY.exists():
            raise unittest.SkipTest("debug binary not built")

    def test_outcomes_match_pytest(self) -> None:
        for hook, conftest in HOOKS.items():
            for dist in ((), ("-n", "2")):
                with self.subTest(hook=hook, dist=dist), tempfile.TemporaryDirectory() as tmp:
                    tmp_path = Path(tmp)
                    (tmp_path / "conftest.py").write_text(conftest)
                    (tmp_path / "test_mix.py").write_text(TESTS)
                    result = subprocess.run(
                        [str(BINARY), "-q", "-p", "no:cacheprovider", *dist, "--junitxml=j.xml"],
                        cwd=tmp_path,
                        capture_output=True,
                        text=True,
                    )
                    last = result.stdout.strip().splitlines()[-1]
                    self.assertTrue(last.startswith(SUMMARY + " in "), result.stdout)
                    outcomes = {}
                    for case in ET.parse(tmp_path / "j.xml").iter("testcase"):
                        outcome = "passed"
                        for tag in ("failure", "error", "skipped"):
                            if case.find(tag) is not None:
                                outcome = tag
                        outcomes[case.get("name")] = outcome
                    self.assertEqual(outcomes, OUTCOMES)


if __name__ == "__main__":
    unittest.main()
