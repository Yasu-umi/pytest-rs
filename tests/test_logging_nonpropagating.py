"""Regression test: which loggers' records caplog captures, against pytest 9.2.

pytest 9.2 attaches its capture handlers, at each phase's start, to the root
logger and every logger that is non-propagating at that moment. pytest-rs
caches the Logger objects between phases (rebuilt when a PlaceHolder becomes
a Logger or one is added) but reads `propagate` every phase, so it must see:
a PlaceHolder turned into a non-propagating Logger during a test only from
the next phase on, and a logger made non-propagating later likewise.
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

TESTS = """\
import logging

logging.getLogger("a.b.c")  # "a" and "a.b" are PlaceHolders


def test_placeholder_becomes_nonpropagating_logger(caplog):
    logger = logging.getLogger("a.b")  # PlaceHolder -> Logger, same dict size
    logger.propagate = False
    logger.warning("from a.b")
    # Handlers were attached at this phase's start, before the change.
    assert [r.getMessage() for r in caplog.records] == []


def test_next_test_sees_it(caplog):
    logging.getLogger("a.b").warning("again")
    assert [r.getMessage() for r in caplog.records] == ["again"]


def test_existing_logger_turned_nonpropagating(caplog):
    logger = logging.getLogger("a.b.c")
    logger.propagate = False
    logger.warning("not yet")
    assert [r.getMessage() for r in caplog.records] == []


def test_and_seen_by_the_next_one(caplog):
    logging.getLogger("a.b.c").warning("now")
    assert [r.getMessage() for r in caplog.records] == ["now"]
"""


class TestLoggingNonPropagating(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if not BINARY.exists():
            raise unittest.SkipTest("debug binary not built")

    def test_matches_pytest(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            (tmp_path / "test_lg.py").write_text(TESTS)
            result = subprocess.run(
                [str(BINARY), "-q", "-p", "no:cacheprovider", "-p", "no:randomly"],
                cwd=tmp_path,
                capture_output=True,
                text=True,
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("4 passed", result.stdout)


if __name__ == "__main__":
    unittest.main()
