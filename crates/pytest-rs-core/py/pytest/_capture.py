"""Output capture: the global per-phase sys-level capture plus the
capsys/capfd fixtures.

Mirrors pytest's capture plugin at the sys level: during each test phase
sys.stdout/sys.stderr are swapped for buffers; failing reports get
"Captured stdout {when}" sections. The runner drives phases via
start_phase()/finish_item() exactly like pytest._logging. Rust's own
progress output writes the real fd directly, so it never routes through
the captured sys.stdout.
"""

import contextlib
import io
import sys
from typing import NamedTuple

from pytest._fixtures import fixture


class CaptureResult(NamedTuple):
    out: str
    err: str


class CaptureState:
    """The global capture (pytest's CaptureManager equivalent).

    One buffer pair per item, installed at setup start; phase boundaries
    snapshot by offset instead of swapping streams, so a capsys/capfd
    fixture layered on top survives phase transitions untouched."""

    def __init__(self):
        self.enabled = False
        self.when = None
        self.sections = []  # finished (title, text) pairs for this item
        self._installed = False
        self._old = None
        self._out = None
        self._err = None
        self._out_offset = 0
        self._err_offset = 0

    def configure(self, mode):
        # "fd" approximates to sys-level capture (no fd duplication).
        self.enabled = mode in ("fd", "sys")

    def start_phase(self, when):
        if not self.enabled:
            return
        if when == "setup":
            self.finish_item()
            self.sections = []
            self._old = (sys.stdout, sys.stderr)
            self._out = io.StringIO()
            self._err = io.StringIO()
            sys.stdout = self._out
            sys.stderr = self._err
            self._installed = True
            self._out_offset = 0
            self._err_offset = 0
        else:
            self._snap_section()
        self.when = when

    def _snap_section(self):
        """Close the running phase: its buffer slice becomes a section."""
        if not self._installed or self.when is None:
            return
        out_all = self._out.getvalue()
        err_all = self._err.getvalue()
        out = out_all[self._out_offset :]
        err = err_all[self._err_offset :]
        self._out_offset = len(out_all)
        self._err_offset = len(err_all)
        if out:
            self.sections.append((f"Captured stdout {self.when}", out))
        if err:
            self.sections.append((f"Captured stderr {self.when}", err))

    def finish_item(self):
        if self._installed:
            self._snap_section()
            sys.stdout, sys.stderr = self._old
            self._installed = False
            self._old = None
        self.when = None

    def failure_sections(self):
        """(title, text) report sections for a failing report."""
        out = list(self.sections)
        if self._installed and self.when is not None:
            text = self._out.getvalue()[self._out_offset :]
            if text:
                out.append((f"Captured stdout {self.when}", text))
            text = self._err.getvalue()[self._err_offset :]
            if text:
                out.append((f"Captured stderr {self.when}", text))
        return out


state = CaptureState()


def configure(mode):
    state.configure(mode)


def start_phase(when):
    state.start_phase(when)


def finish_item():
    state.finish_item()


def failure_sections():
    return state.failure_sections()


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
        # Suspends every capture layer: output reaches the real terminal.
        old = (sys.stdout, sys.stderr)
        sys.stdout = sys.__stdout__
        sys.stderr = sys.__stderr__
        try:
            yield
        finally:
            sys.stdout, sys.stderr = old


@fixture
def capsys():
    capture = CaptureFixture()
    capture._start()
    yield capture
    capture._stop()


@fixture
def capfd():
    # fd-level capture approximated at the sys level (like the global
    # capture); covers print()/sys writes but not raw os.write().
    capture = CaptureFixture()
    capture._start()
    yield capture
    capture._stop()
