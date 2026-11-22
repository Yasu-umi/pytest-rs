"""Regression test: session-level hooks on -n workers, against pytest 9.2 + xdist.

Two gaps under -n:
- a conftest pytest_collection_finish fired once, on the controller, with no
  items; upstream xdist's controller never collects (DSession returns True
  from pytest_collection) and each worker's perform_collect fires it with
  that worker's items.
- the pytest_sessionfinish of a plugin instance registered from
  pytest_configure (`config.pluginmanager.register(P())`) never ran on a
  worker, although its pytest_runtest_logreport did.

The expected per-process hook logs below are pytest 9.2.0 + pytest-xdist
3.8.0's, for serial runs and for -n 2.
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

CONFTEST = """\
import os

def _log(msg):
    wid = os.environ.get("PYTEST_XDIST_WORKER", "main")
    with open(os.path.join(os.path.dirname(__file__), "log-" + wid + ".txt"), "a") as f:
        f.write(msg + "\\n")

def pytest_collection_finish(session):
    _log("collection_finish items=%d" % len(session.items))

class Plugin:
    def pytest_runtest_logreport(self, report):
        if report.when == "call":
            _log("plugin logreport")

    def pytest_sessionfinish(self, session, exitstatus):
        _log("plugin sessionfinish")

def pytest_configure(config):
    config.pluginmanager.register(Plugin(), "myplugin")

def pytest_sessionfinish(session, exitstatus):
    _log("module sessionfinish")
"""

TESTS = "def test_a():\n    pass\ndef test_b():\n    pass\ndef test_c():\n    pass\n"

# pytest 9.2.0 + pytest-xdist 3.8.0. Per worker, the logreport lines depend
# on scheduling, so only the other lines are compared there.
SERIAL = {
    "main": [
        "collection_finish items=3",
        "module sessionfinish",
        "plugin logreport",
        "plugin logreport",
        "plugin logreport",
        "plugin sessionfinish",
    ],
}
DIST = {
    "main": [
        "module sessionfinish",
        "plugin logreport",
        "plugin logreport",
        "plugin logreport",
        "plugin sessionfinish",
    ],
    "gw0": ["collection_finish items=3", "module sessionfinish", "plugin sessionfinish"],
    "gw1": ["collection_finish items=3", "module sessionfinish", "plugin sessionfinish"],
}


class TestWorkerSessionHooks(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if not BINARY.exists():
            raise unittest.SkipTest("debug binary not built")

    def _run(self, *args: str, spawn: bool = False) -> dict[str, list[str]]:
        env = {k: v for k, v in os.environ.items() if k != "PYTEST_RS_DIST_SPAWN"}
        if spawn:
            env["PYTEST_RS_DIST_SPAWN"] = "1"
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            (tmp_path / "conftest.py").write_text(CONFTEST)
            (tmp_path / "test_x.py").write_text(TESTS)
            result = subprocess.run(
                [str(BINARY), "-q", "-p", "no:cacheprovider", *args],
                cwd=tmp_path,
                capture_output=True,
                text=True,
                env=env,
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            logs = {}
            for log in tmp_path.glob("log-*.txt"):
                lines = sorted(log.read_text().splitlines())
                if log.stem != "log-main":
                    lines = [line for line in lines if line != "plugin logreport"]
                logs[log.stem.removeprefix("log-")] = lines
            return logs

    def test_serial(self) -> None:
        self.assertEqual(self._run(), SERIAL)

    def test_dist(self) -> None:
        for spawn in (False, True):
            with self.subTest(spawn=spawn):
                self.assertEqual(self._run("-n", "2", spawn=spawn), DIST)


if __name__ == "__main__":
    unittest.main()
