"""capsys: sys.stdout/stderr capture fixture (Python-level replacement)."""

import contextlib
import io
import sys
from typing import NamedTuple

from pytest._fixtures import fixture


class CaptureResult(NamedTuple):
    out: str
    err: str


class CaptureFixture:
    def __init__(self):
        self._out = io.StringIO()
        self._err = io.StringIO()
        self._old_out = None
        self._old_err = None

    def _start(self):
        self._old_out = sys.stdout
        self._old_err = sys.stderr
        sys.stdout = self._out
        sys.stderr = self._err

    def _stop(self):
        if self._old_out is not None:
            sys.stdout = self._old_out
            sys.stderr = self._old_err
            self._old_out = None
            self._old_err = None

    def readouterr(self):
        out = self._out.getvalue()
        err = self._err.getvalue()
        self._out.seek(0)
        self._out.truncate()
        self._err.seek(0)
        self._err.truncate()
        return CaptureResult(out, err)

    @contextlib.contextmanager
    def disabled(self):
        self._stop()
        try:
            yield
        finally:
            self._start()


@fixture
def capsys():
    capture = CaptureFixture()
    capture._start()
    yield capture
    capture._stop()
