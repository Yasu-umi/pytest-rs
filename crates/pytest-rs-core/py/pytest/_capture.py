"""Output capture: the global per-phase capture plus the capsys/capfd
fixture family.

The capture classes (FDCapture, SysCapture, MultiCapture, ...) are ports
of pytest's _pytest/capture.py, faithful down to the state machine and
reprs because upstream's own suite unit-tests them; _pytest.capture
re-exports them. In the default "fd" mode each captured fd is dup2'd onto
an unlinked temp file, so print(), os.write(1, ...) and C-level output
all land in the same buffer in order; "sys" mode swaps sys.stdout/stderr
only. The runner drives the global capture via start_phase()/
finish_item() exactly like pytest._logging; failing reports get
"Captured stdout {when}" sections. Rust's own progress output writes
between items, when the real fds are restored, so it is never captured.
"""

import contextlib
import io
import os
import sys
import tempfile
from typing import NamedTuple

from pytest._fixtures import fixture


class CaptureResult(NamedTuple):
    out: str
    err: str


class EncodedFile(io.TextIOWrapper):
    __slots__ = ()

    @property
    def name(self):
        # Ensure that file.name is a string (TemporaryFile names are ints).
        return repr(self.buffer)

    @property
    def mode(self):
        # TextIOWrapper doesn't expose a mode, but at least some code
        # checks it.
        return self.buffer.mode.replace("b", "")


class CaptureIO(io.TextIOWrapper):
    """In-memory text stream with a real .buffer, so
    sys.stdout.buffer.write(b"...") works under sys-level capture."""

    def __init__(self):
        super().__init__(io.BytesIO(), encoding="UTF-8", newline="", write_through=True)

    def getvalue(self):
        return self.buffer.getvalue().decode("UTF-8")


class TeeCaptureIO(CaptureIO):
    """--capture=tee-sys: capture and pass writes through to the real
    stream."""

    def __init__(self, other):
        self._other = other
        super().__init__()

    def write(self, s):
        super().write(s)
        return self._other.write(s)


class DontReadFromInput:
    """sys.stdin while capture is active: reading is a hard error instead
    of a silent hang."""

    @property
    def encoding(self):
        return getattr(sys.__stdin__, "encoding", "UTF-8")

    def read(self, size=-1):
        raise OSError("pytest: reading from stdin while output is captured!  Consider using `-s`.")

    readline = read

    def __next__(self):
        return self.readline()

    def readlines(self, hint=-1):
        raise OSError("pytest: reading from stdin while output is captured!  Consider using `-s`.")

    def __iter__(self):
        return self

    def fileno(self):
        raise io.UnsupportedOperation("redirected stdin is pseudofile, has no fileno()")

    def flush(self):
        raise io.UnsupportedOperation("redirected stdin is pseudofile, has no flush()")

    def isatty(self):
        return False

    def close(self):
        pass

    def readable(self):
        return False

    def seek(self, offset, whence=0):
        raise io.UnsupportedOperation("redirected stdin is pseudofile, has no seek(int)")

    def seekable(self):
        return False

    def tell(self):
        raise io.UnsupportedOperation("redirected stdin is pseudofile, has no tell()")

    def truncate(self, size=None):
        raise io.UnsupportedOperation("cannot truncate stdin")

    def write(self, data):
        raise io.UnsupportedOperation("cannot write to stdin")

    def writelines(self, lines):
        raise io.UnsupportedOperation("Cannot write to stdin")

    def writable(self):
        return False

    def __enter__(self):
        return self

    def __exit__(self, type, value, traceback):
        pass

    @property
    def buffer(self):
        return self


# Capture classes.

patchsysdict = {0: "stdin", 1: "stdout", 2: "stderr"}


class CaptureBase:
    EMPTY_BUFFER: str | bytes | None = None

    def discard_after_fork(self):
        """Release this capture's resources in a forked -n worker, without
        ever touching targetfd (0/1/2) itself: by the time a forked worker
        runs this, fd 1 has already been repurposed as its IPC pipe to the
        controller, so anything that dup2s/closes targetfd itself (as a
        plain .done() does, to restore the pre-capture fd) would sever that
        pipe. This base implementation has no raw fd tricks to worry about
        (see NoCapture/SysCaptureBase), so plain .done() already qualifies;
        FDCaptureBase overrides this with a version that skips its unsafe
        parts."""
        self.done()


class NoCapture(CaptureBase):
    EMPTY_BUFFER = ""

    def __init__(self, fd):
        pass

    def start(self):
        pass

    def done(self):
        pass

    def suspend(self):
        pass

    def resume(self):
        pass

    def snap(self):
        return ""

    def writeorg(self, data):
        pass


class SysCaptureBase(CaptureBase):
    def __init__(self, fd, tmpfile=None, *, tee=False):
        name = patchsysdict[fd]
        self._old = getattr(sys, name)
        self.name = name
        if tmpfile is None:
            if name == "stdin":
                tmpfile = DontReadFromInput()
            else:
                tmpfile = CaptureIO() if not tee else TeeCaptureIO(self._old)
        self.tmpfile = tmpfile
        self._state = "initialized"

    def repr(self, class_name):
        return "<{} {} _old={} _state={!r} tmpfile={!r}>".format(
            class_name,
            self.name,
            (hasattr(self, "_old") and repr(self._old)) or "<UNSET>",
            self._state,
            self.tmpfile,
        )

    def __repr__(self):
        return "<{} {} _old={} _state={!r} tmpfile={!r}>".format(
            self.__class__.__name__,
            self.name,
            (hasattr(self, "_old") and repr(self._old)) or "<UNSET>",
            self._state,
            self.tmpfile,
        )

    def _assert_state(self, op, states):
        assert self._state in states, "cannot {} in state {!r}: expected one of {}".format(
            op, self._state, ", ".join(states)
        )

    def start(self):
        self._assert_state("start", ("initialized",))
        setattr(sys, self.name, self.tmpfile)
        self._state = "started"

    def done(self):
        self._assert_state("done", ("initialized", "started", "suspended", "done"))
        if self._state == "done":
            return
        setattr(sys, self.name, self._old)
        del self._old
        self.tmpfile.close()
        self._state = "done"

    def suspend(self):
        self._assert_state("suspend", ("started", "suspended"))
        setattr(sys, self.name, self._old)
        self._state = "suspended"

    def resume(self):
        self._assert_state("resume", ("started", "suspended"))
        if self._state == "started":
            return
        setattr(sys, self.name, self.tmpfile)
        self._state = "started"


class SysCaptureBinary(SysCaptureBase):
    EMPTY_BUFFER = b""

    def snap(self):
        self._assert_state("snap", ("started", "suspended"))
        self.tmpfile.seek(0)
        res = self.tmpfile.buffer.read()
        self.tmpfile.seek(0)
        self.tmpfile.truncate()
        return res

    def writeorg(self, data):
        self._assert_state("writeorg", ("started", "suspended"))
        self._old.flush()
        self._old.buffer.write(data)
        self._old.buffer.flush()


class SysCapture(SysCaptureBase):
    EMPTY_BUFFER = ""

    def snap(self):
        self._assert_state("snap", ("started", "suspended"))
        res = self.tmpfile.getvalue()
        self.tmpfile.seek(0)
        self.tmpfile.truncate()
        return res

    def writeorg(self, data):
        self._assert_state("writeorg", ("started", "suspended"))
        self._old.write(data)
        self._old.flush()


class FDCaptureBase(CaptureBase):
    def __init__(self, targetfd):
        self.targetfd = targetfd

        try:
            os.fstat(targetfd)
        except OSError:
            # The target fd is invalid; capture against /dev/null so
            # suspend/resume and fd reuse stay robust (pytest's approach).
            self.targetfd_invalid = os.open(os.devnull, os.O_RDWR)
            os.dup2(self.targetfd_invalid, targetfd)
        else:
            self.targetfd_invalid = None
        self.targetfd_save = os.dup(targetfd)

        if targetfd == 0:
            self.tmpfile = open(os.devnull, encoding="utf-8")
            self.syscapture = SysCapture(targetfd)
        else:
            self.tmpfile = EncodedFile(
                tempfile.TemporaryFile(buffering=0),
                encoding="utf-8",
                errors="replace",
                newline="",
                write_through=True,
            )
            if targetfd in patchsysdict:
                self.syscapture = SysCapture(targetfd, self.tmpfile)
            else:
                self.syscapture = NoCapture(targetfd)

        self._state = "initialized"

    def __repr__(self):
        return (
            f"<{self.__class__.__name__} {self.targetfd} oldfd={self.targetfd_save} "
            f"_state={self._state!r} tmpfile={self.tmpfile!r}>"
        )

    def _assert_state(self, op, states):
        assert self._state in states, "cannot {} in state {!r}: expected one of {}".format(
            op, self._state, ", ".join(states)
        )

    def start(self):
        """Start capturing on targetfd using memorized tmpfile."""
        self._assert_state("start", ("initialized",))
        os.dup2(self.tmpfile.fileno(), self.targetfd)
        self.syscapture.start()
        self._state = "started"

    def done(self):
        """Stop capturing, restore streams, return original capture file,
        seeked to position zero."""
        self._assert_state("done", ("initialized", "started", "suspended", "done"))
        if self._state == "done":
            return
        os.dup2(self.targetfd_save, self.targetfd)
        os.close(self.targetfd_save)
        if self.targetfd_invalid is not None:
            if self.targetfd_invalid != self.targetfd:
                os.close(self.targetfd)
            os.close(self.targetfd_invalid)
        self.syscapture.done()
        self.tmpfile.close()
        self._state = "done"

    def discard_after_fork(self):
        """Like .done(), but never touches targetfd (0/1/2) itself — a
        forked worker has already repurposed it as its IPC pipe to the
        controller, so .done()'s dup2/close of targetfd would sever that
        pipe. Only targetfd_save/targetfd_invalid (separate, unrelated fd
        numbers this object privately owns) and the tmpfile need releasing;
        left alone, they leak until Python's GC eventually finalizes them
        at some unpredictable later point during this worker's own test
        run, which pytest's own unraisableexception machinery then
        faithfully (but spuriously) attributes to whatever test happens to
        be running at that moment."""
        self._assert_state("discard_after_fork", ("initialized", "started", "suspended", "done"))
        if self._state == "done":
            return
        os.close(self.targetfd_save)
        if self.targetfd_invalid is not None:
            os.close(self.targetfd_invalid)
        self.syscapture.done()
        self.tmpfile.close()
        self._state = "done"

    def suspend(self):
        self._assert_state("suspend", ("started", "suspended"))
        if self._state == "suspended":
            return
        self.syscapture.suspend()
        os.dup2(self.targetfd_save, self.targetfd)
        self._state = "suspended"

    def resume(self):
        self._assert_state("resume", ("started", "suspended"))
        if self._state == "started":
            return
        self.syscapture.resume()
        os.dup2(self.tmpfile.fileno(), self.targetfd)
        self._state = "started"


class FDCaptureBinary(FDCaptureBase):
    """Capture IO to/from a given OS-level file descriptor; snap()
    produces bytes."""

    EMPTY_BUFFER = b""

    def snap(self):
        self._assert_state("snap", ("started", "suspended"))
        self.tmpfile.seek(0)
        res = self.tmpfile.buffer.read()
        self.tmpfile.seek(0)
        self.tmpfile.truncate()
        return res

    def writeorg(self, data):
        """Write to original file descriptor."""
        self._assert_state("writeorg", ("started", "suspended"))
        os.write(self.targetfd_save, data)


class FDCapture(FDCaptureBase):
    """Capture IO to/from a given OS-level file descriptor; snap()
    produces text."""

    EMPTY_BUFFER = ""

    def snap(self):
        self._assert_state("snap", ("started", "suspended"))
        self.tmpfile.seek(0)
        res = self.tmpfile.read()
        self.tmpfile.seek(0)
        self.tmpfile.truncate()
        return res

    def writeorg(self, data):
        """Write to original file descriptor."""
        self._assert_state("writeorg", ("started", "suspended"))
        os.write(self.targetfd_save, data.encode("utf-8"))


class MultiCapture:
    _state = None
    _in_suspended = False

    def __init__(self, in_, out, err):
        self.in_ = in_
        self.out = out
        self.err = err

    def __repr__(self):
        return (
            f"<MultiCapture out={self.out!r} err={self.err!r} in_={self.in_!r} "
            f"_state={self._state!r} _in_suspended={self._in_suspended!r}>"
        )

    def start_capturing(self):
        self._state = "started"
        if self.in_:
            self.in_.start()
        if self.out:
            self.out.start()
        if self.err:
            self.err.start()

    def pop_outerr_to_orig(self):
        """Pop current snapshot out/err capture and flush to orig streams."""
        out, err = self.readouterr()
        if out:
            self.out.writeorg(out)
        if err:
            self.err.writeorg(err)
        return out, err

    def suspend_capturing(self, in_=False):
        self._state = "suspended"
        if self.out:
            self.out.suspend()
        if self.err:
            self.err.suspend()
        if in_ and self.in_:
            self.in_.suspend()
            self._in_suspended = True

    def resume_capturing(self):
        self._state = "started"
        if self.out:
            self.out.resume()
        if self.err:
            self.err.resume()
        if self._in_suspended:
            self.in_.resume()
            self._in_suspended = False

    def stop_capturing(self):
        """Stop capturing and reset capturing streams."""
        if self._state == "stopped":
            raise ValueError("was already stopped")
        self._state = "stopped"
        if self.out:
            self.out.done()
        if self.err:
            self.err.done()
        if self.in_:
            self.in_.done()

    def discard_after_fork(self):
        """Like stop_capturing, but for a capture inherited into a forked -n
        worker via fork() — see CaptureBase.discard_after_fork for why
        targetfd itself must never be touched here."""
        if self._state == "stopped":
            return
        self._state = "stopped"
        if self.out:
            self.out.discard_after_fork()
        if self.err:
            self.err.discard_after_fork()
        if self.in_:
            self.in_.discard_after_fork()

    def is_started(self):
        """Whether actively capturing -- not suspended or stopped."""
        return self._state == "started"

    def readouterr(self):
        out = self.out.snap() if self.out else ""
        err = self.err.snap() if self.err else ""
        return CaptureResult(out, err)


def _get_multicapture(method):
    if method == "fd":
        return MultiCapture(in_=FDCapture(0), out=FDCapture(1), err=FDCapture(2))
    elif method == "sys":
        return MultiCapture(in_=SysCapture(0), out=SysCapture(1), err=SysCapture(2))
    elif method == "no":
        return MultiCapture(in_=None, out=None, err=None)
    elif method == "tee-sys":
        return MultiCapture(in_=None, out=SysCapture(1, tee=True), err=SysCapture(2, tee=True))
    raise ValueError(f"unknown capturing method: {method!r}")


class CaptureManager:
    """pytest's CaptureManager API surface (upstream unit-tests it); the
    runner itself drives CaptureState below, which wraps one of these."""

    def __init__(self, method):
        self._method = method
        self._global_capturing = None
        self._capture_fixture = None

    def __repr__(self):
        return (
            f"<CaptureManager _method={self._method!r} _global_capturing={self._global_capturing!r} "
            f"_capture_fixture={self._capture_fixture!r}>"
        )

    def is_capturing(self):
        if self.is_globally_capturing():
            return "global"
        if self._capture_fixture:
            return f"fixture {self._capture_fixture._name}"
        return False

    def is_globally_capturing(self):
        return self._method != "no"

    def start_global_capturing(self):
        assert self._global_capturing is None
        self._global_capturing = _get_multicapture(self._method)
        self._global_capturing.start_capturing()

    def stop_global_capturing(self):
        if self._global_capturing is not None:
            self._global_capturing.pop_outerr_to_orig()
            self._global_capturing.stop_capturing()
            self._global_capturing = None

    def resume_global_capture(self):
        # During teardown of the python process, and on rare occasions, capture
        # attributes can be `None` while trying to resume global capture.
        if self._global_capturing is not None:
            self._global_capturing.resume_capturing()

    def suspend_global_capture(self, in_=False):
        if self._global_capturing is not None:
            self._global_capturing.suspend_capturing(in_=in_)

    def suspend(self, in_=False):
        self.suspend_fixture()
        self.suspend_global_capture(in_)

    def resume(self):
        self.resume_global_capture()
        self.resume_fixture()

    def read_global_capture(self):
        assert self._global_capturing is not None
        return self._global_capturing.readouterr()

    def set_fixture(self, capture_fixture):
        self._capture_fixture = capture_fixture

    def unset_fixture(self):
        self._capture_fixture = None

    def suspend_fixture(self):
        if self._capture_fixture:
            self._capture_fixture._suspend()

    def resume_fixture(self):
        if self._capture_fixture:
            self._capture_fixture._resume()


class CaptureState:
    """The global capture driven by the Rust runner.

    One MultiCapture per session, started lazily at the first item's setup
    and suspended between items (so Rust's progress output reaches the
    real terminal); phase boundaries drain the buffers into sections, so
    a capsys/capfd fixture layered on top survives phase transitions
    untouched."""

    def __init__(self):
        self.enabled = False
        self.mode = "no"
        self.when = None
        self.sections = []  # finished (title, text) pairs for this item
        self.fixture = None  # the active capsys/capfd CaptureFixture, if any
        self._capture = None
        self._installed = False
        self._subtest_parent_out = []
        self._subtest_parent_err = []

    @property
    def fixture_name(self):
        return self.fixture._name if self.fixture is not None else None

    def configure(self, mode):
        self.mode = mode
        self.enabled = mode in ("fd", "sys", "tee-sys")
        # Like pytest's pytest_load_initial_conftests wrapper: the global
        # capture exists (suspended) for the whole session, so collection
        # units resume it and session end stops it even with zero items.
        if self.enabled and self._capture is None:
            self._capture = _get_multicapture(mode)
            self._capture.start_capturing()
            self._capture.suspend_capturing(in_=True)

    def start_phase(self, when):
        if not self.enabled:
            return
        if when == "setup":
            self.finish_item()
            self.sections = []
            self._capture.resume_capturing()
            self._installed = True
        else:
            self._snap_section()
        self.when = when

    def collect_begin(self):
        """Capture around one file's collection (pytest's
        pytest_make_collect_report wrapper)."""
        if not self.enabled:
            return
        self._capture.resume_capturing()
        self._installed = True
        self.when = "collect"

    def collect_end(self):
        """[(title, text)] report sections captured while collecting."""
        if not self.enabled or not self._installed:
            return []
        try:
            out, err = self._capture.readouterr()
        except Exception:
            self._emergency_suspend()
            raise
        self._capture.suspend_capturing(in_=True)
        self._installed = False
        self.when = None
        sections = []
        if out:
            sections.append(("Captured stdout", out))
        if err:
            sections.append(("Captured stderr", err))
        return sections

    def session_end(self):
        """Stop the global capture (pytest's stop_global_capturing): any
        leftover output pops to the real streams; a broken snap raises."""
        if self._capture is None:
            return
        capture, self._capture = self._capture, None
        self._installed = False
        self.when = None
        try:
            capture.pop_outerr_to_orig()
        finally:
            capture.stop_capturing()

    def reinit_post_fork(self):
        """Forked workers inherit capture objects whose saved fds point at
        the controller's terminal, not this worker's IPC pipe: discard and
        re-create them against the current fds (never restoring the stale
        saves). Explicitly releases the inherited capture's own resources
        first (discard_after_fork, not stop_capturing/done — those try to
        restore the saved fd onto targetfd, which by now is this worker's
        IPC pipe): left merely dereferenced, its tempfile/saved-fd would
        leak until Python's GC eventually finalizes them at some
        unpredictable later point during this worker's own test run, which
        pytest's unraisableexception machinery then attributes (spuriously)
        to whatever test happens to be running at that moment."""
        if self._capture is not None:
            self._capture.discard_after_fork()
        self._capture = None
        self._installed = False
        self.when = None
        self.configure(self.mode)

    def _snap_section(self):
        """Close the running phase: its buffer contents become a section."""
        if not self._installed or self.when is None:
            return
        try:
            # Like pytest's per-phase fixture deactivation: unread capsys/
            # capfd output flushes to the global capture, so it shows in
            # the report.
            if self.fixture is not None:
                self.fixture._pop_to_orig()
            out, err = self._capture.readouterr()
        except Exception:
            # A broken capture (e.g. a monkeypatched snap) must not leave
            # the real fds redirected: restore them before propagating.
            self._emergency_suspend()
            raise
        parent_out = "".join(self._subtest_parent_out)
        parent_err = "".join(self._subtest_parent_err)
        self._subtest_parent_out.clear()
        self._subtest_parent_err.clear()
        out = parent_out + out
        err = parent_err + err
        if out:
            self.sections.append((f"Captured stdout {self.when}", out))
        if err:
            self.sections.append((f"Captured stderr {self.when}", err))

    def _emergency_suspend(self):
        with contextlib.suppress(Exception):
            self._capture.suspend_capturing(in_=True)
        self._installed = False

    def finish_item(self):
        if self._installed:
            self._snap_section()
            self._capture.suspend_capturing(in_=True)
            self._installed = False
        self.when = None

    def subtest_enter(self):
        if not self._installed or self._capture is None:
            return
        if self.fixture is not None:
            self.fixture._pop_to_orig()
        out, err = self._capture.readouterr()
        if out:
            self._subtest_parent_out.append(out)
        if err:
            self._subtest_parent_err.append(err)

    def subtest_exit(self):
        if not self._installed or self._capture is None:
            return []
        if self.fixture is not None:
            self.fixture._pop_to_orig()
        out, err = self._capture.readouterr()
        sections = []
        if out:
            sections.append((f"Captured stdout {self.when}", out))
        if err:
            sections.append((f"Captured stderr {self.when}", err))
        return sections

    @staticmethod
    def _peek(cap):
        """The captured-so-far text of one capture half, not consumed."""
        if cap is None:
            return ""
        if isinstance(cap, FDCaptureBase):
            fd = cap.tmpfile.fileno()
            end = os.lseek(fd, 0, os.SEEK_CUR)
            if end <= 0:
                return ""
            return os.pread(fd, end, 0).decode("UTF-8", errors="replace")
        return cap.tmpfile.getvalue()

    def failure_sections(self):
        """(title, text) report sections for a failing report."""
        out = list(self.sections)
        if self._installed and self.when is not None:
            if self.fixture is not None:
                self.fixture._pop_to_orig()
            parent_out = "".join(self._subtest_parent_out)
            parent_err = "".join(self._subtest_parent_err)
            text = parent_out + self._peek(self._capture.out)
            if text:
                out.append((f"Captured stdout {self.when}", text))
            text = parent_err + self._peek(self._capture.err)
            if text:
                out.append((f"Captured stderr {self.when}", text))
        return out

    @contextlib.contextmanager
    def globally_disabled(self):
        """Suspend every capture layer, fixture first (pytest's
        global_and_fixture_disabled): output reaches the real terminal
        (live-log emission, capsys.disabled()). Reentrant: an already
        suspended layer is left alone (the is_started() guards)."""
        do_fixture = (
            self.fixture is not None
            and self.fixture._capture is not None
            and self.fixture._capture.is_started()
        )
        if do_fixture:
            self.fixture._suspend()
        do_global = self._installed and self._capture.is_started()
        if do_global:
            self._capture.suspend_capturing(in_=True)
        try:
            yield
        finally:
            if do_global:
                self._capture.resume_capturing()
            if do_fixture:
                self.fixture._resume()


state = CaptureState()


class _GlobalCaptureManager:
    """The object pluginmanager.getplugin("capturemanager") hands out:
    plugins (e.g. pytest-timeout before dumping stacks and os._exit) use it
    to suspend the global capture so their output reaches the real
    terminal, and to read what the test captured so far."""

    def suspend_global_capture(self, item=None, in_=False):
        if state.fixture is not None and state.fixture._capture is not None:
            if state.fixture._capture.is_started():
                state.fixture._suspend()
        if state._installed and state._capture is not None and state._capture.is_started():
            state._capture.suspend_capturing(in_=bool(item) or in_)

    def resume_global_capture(self):
        if state._installed and state._capture is not None:
            state._capture.resume_capturing()

    def read_global_capture(self):
        if state._capture is None:
            return ("", "")
        return state._capture.readouterr()

    @contextlib.contextmanager
    def global_and_fixture_disabled(self):
        do_fixture = (
            state.fixture is not None
            and state.fixture._capture is not None
            and state.fixture._capture.is_started()
        )
        if do_fixture:
            state.fixture._suspend()
        do_global = state._installed and state._capture is not None and state._capture.is_started()
        if do_global:
            state._capture.suspend_capturing()
        try:
            yield
        finally:
            if do_global:
                state._capture.resume_capturing()
            if do_fixture:
                state.fixture._resume()


manager = _GlobalCaptureManager()


def configure(mode):
    state.configure(mode)


def start_phase(when):
    state.start_phase(when)


def finish_item():
    state.finish_item()


def failure_sections():
    return state.failure_sections()


def passing_phase_sections():
    """Return capture sections for a passing phase report (peeks without draining).

    Used to populate sections on a passing setup report so that capstdout/capstderr
    reflect setup output, matching upstream pytest's behavior."""
    return state.failure_sections()


def begin_scope_teardown():
    """Arm capture for a module/class/session scope teardown running
    between items (the runner defers them past finish_item); its output
    reports as "Captured stdout teardown" like pytest, where the same
    finalizers run inside the last item's teardown."""
    state.start_phase("setup")
    state.when = "teardown"


def suspend_global():
    """Pause the item capture so the runner's own terminal output (live-log
    outcome words) reaches the real fds mid-item."""
    if state._installed and state._capture.is_started():
        state._capture.suspend_capturing(in_=True)


def resume_global():
    if state._installed:
        state._capture.resume_capturing()


def read_global_capture():
    """Drain whatever the global capture has buffered so far, without
    suspending it — used by callers that force-resumed capturing around a
    hook call and want its output to appear immediately (in program order)
    rather than sit buffered until some later readouterr() call."""
    return manager.read_global_capture()


def collect_begin():
    state.collect_begin()


def collect_end():
    return state.collect_end()


def session_end():
    state.session_end()


def reinit_post_fork():
    state.reinit_post_fork()


class CaptureFixture[AnyStr: (str, bytes)]:
    """The capsys/capfd/capsysbinary/capfdbinary backing object."""

    def __init__(self, captureclass, name, *, tee=False):
        self.captureclass = captureclass
        self._name = name
        self._tee = tee
        self._capture = None
        self._captured_out = captureclass.EMPTY_BUFFER
        self._captured_err = captureclass.EMPTY_BUFFER

    def _start(self):
        if state.fixture is not None:
            raise RuntimeError(f"cannot use {self._name} and {state.fixture_name} at the same time")
        state.fixture = self
        if self._capture is None:
            if self._tee:
                out, err = self.captureclass(1, tee=True), self.captureclass(2, tee=True)
            else:
                out, err = self.captureclass(1), self.captureclass(2)
            self._capture = MultiCapture(in_=None, out=out, err=err)
            self._capture.start_capturing()

    def close(self):
        if state.fixture is self:
            state.fixture = None
        if self._capture is not None:
            out, err = self._capture.pop_outerr_to_orig()
            self._captured_out += out
            self._captured_err += err
            self._capture.stop_capturing()
            self._capture = None

    def _pop_to_orig(self):
        """Flush unread captured output to the enclosing (global) capture,
        keeping it readable via readouterr (pytest deactivates/reactivates
        the fixture at phase boundaries to the same effect)."""
        if self._capture is not None and self._capture.is_started():
            out, err = self._capture.pop_outerr_to_orig()
            self._captured_out += out
            self._captured_err += err

    def _suspend(self):
        if self._capture is not None:
            self._capture.suspend_capturing()

    def _resume(self):
        if self._capture is not None:
            self._capture.resume_capturing()

    def readouterr(self):
        """The captured output so far, resetting the buffers."""
        captured_out, captured_err = self._captured_out, self._captured_err
        if self._capture is not None:
            out, err = self._capture.readouterr()
            captured_out += out
            captured_err += err
        self._captured_out = self.captureclass.EMPTY_BUFFER
        self._captured_err = self.captureclass.EMPTY_BUFFER
        return CaptureResult(captured_out, captured_err)

    @contextlib.contextmanager
    def disabled(self):
        # Suspends every capture layer: output reaches the real terminal.
        with state.globally_disabled():
            yield


def _capture_fixture(captureclass, name, *, tee=False):
    capture = CaptureFixture(captureclass, name, tee=tee)
    capture._start()
    try:
        yield capture
    finally:
        capture.close()


@fixture
def capsys():
    yield from _capture_fixture(SysCapture, "capsys")


@fixture
def capteesys():
    yield from _capture_fixture(SysCapture, "capteesys", tee=True)


@fixture
def capsysbinary():
    yield from _capture_fixture(SysCaptureBinary, "capsysbinary")


@fixture
def capfd():
    yield from _capture_fixture(FDCapture, "capfd")


@fixture
def capfdbinary():
    yield from _capture_fixture(FDCaptureBinary, "capfdbinary")
