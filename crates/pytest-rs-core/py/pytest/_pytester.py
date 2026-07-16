"""pytester: run pytest-rs as a child process so upstream test suites can
exercise the runner itself."""

import configparser
import fnmatch
import importlib.util
import io
import itertools
import json
import logging
import os
import pathlib
import pickle
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import warnings

from pytest._fixtures import fixture
from pytest._outcomes import fail, importorskip, skip

# Split-out modules; re-exported so pytest._pytester.<name> keeps resolving
# for pytest/__init__.py and Pytester's internal references.
from pytest._pytester_config import (
    _check_cfg_pytest_section as _check_cfg_pytest_section,
)
from pytest._pytester_config import (
    _validate_required_plugins as _validate_required_plugins,
)
from pytest._pytester_linematcher import LineMatcher as LineMatcher
from pytest._pytester_relay import (
    InlineRunResult as InlineRunResult,
)
from pytest._pytester_relay import (
    _OutcomeReport as _OutcomeReport,
)
from pytest._pytester_relay import (
    _RelayCollector as _RelayCollector,
)
from pytest._pytester_relay import (
    _RelayCollectReport as _RelayCollectReport,
)
from pytest._pytester_relay import (
    _RelayHookCall as _RelayHookCall,
)
from pytest._pytester_relay import (
    _RelayItem as _RelayItem,
)
from pytest._pytester_relay import (
    _RelayItemResult as _RelayItemResult,
)
from pytest._pytester_relay import (
    _RelaySession as _RelaySession,
)
from pytest._pytester_relay import (
    _RelayTestReport as _RelayTestReport,
)

# Captured before any test mutates os.environ: tests sometimes
# mock.patch.dict(os.environ, ..., clear=True) around runpytest(), which would
# otherwise strip the runner path and the import path the subprocess pytester
# needs (in-process pytester upstream shares sys.modules/sys.path, so a cleared
# env still finds installed plugins; we approximate that by remembering both).
_RUNNER_EXE = os.environ.get("PYTEST_RS_EXE")
# Absolutize PYTHONPATH entries at capture time: pytester chdirs to a temp
# directory before running subprocesses, so relative paths would resolve
# against the wrong directory.
_RUNNER_PYTHONPATH = os.environ.get("PYTHONPATH")
if _RUNNER_PYTHONPATH:
    _RUNNER_PYTHONPATH = os.pathsep.join(
        os.path.abspath(p) for p in _RUNNER_PYTHONPATH.split(os.pathsep)
    )
# The engine binary is dynamically linked against libpython; the loader path
# that lets it resolve libpython at runtime (LD_LIBRARY_PATH on linux, the
# DYLD_* vars on macOS) must also survive a clear=True so the nested run can
# even start — on linux a cleared LD_LIBRARY_PATH makes it fail to load.
_LIBPATH_VARS = ("LD_LIBRARY_PATH", "DYLD_LIBRARY_PATH", "DYLD_FALLBACK_LIBRARY_PATH")
_RUNNER_LIBPATH = {v: os.environ[v] for v in _LIBPATH_VARS if v in os.environ}
# Capture original COLUMNS at module load (before monkeypatch): plugins
# capture terminal width at import time (e.g. pytest-icdiff's COLS), and
# the child subprocess must see the pre-monkeypatch width to match
# upstream's in-process pytester where module import precedes monkeypatch.
_RUNNER_COLUMNS = os.environ.get("COLUMNS")

# Staged by _inline_run_inprocess just before the nested engine builds its
# Config, read once by the Rust side (build_py_config) for
# config.invocation_params.plugins; empty outside that narrow window, so a
# normal CLI run or pytester.parseconfig() never sees stale entries.
_pending_invocation_plugins: list = []


class PytesterHelperPlugin:
    """Identity marker matching upstream's inline_run() sentinel plugin, so
    config.invocation_params.plugins has the same shape (upstream relies on
    its pytest_configure to build the HookRecorder; pytest-rs builds it
    directly, so this carries no hooks)."""


_OUTCOME_RE = re.compile(
    r"(\d+) (passed|failed|skipped|xfailed|xpassed|errors?|warnings?|deselected|rerun)"
)
_ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")
_DURATION_RE = re.compile(r"\d+\.\d\ds")


class RunResult:
    def __init__(self, ret, outlines, errlines, duration):
        self.ret = ret
        self.outlines = outlines
        self.errlines = errlines
        self.duration = duration
        self.stdout = LineMatcher(outlines)
        self.stderr = LineMatcher(errlines)

    def __enter__(self):
        return self

    def __exit__(self, *_args):
        pass

    def __repr__(self):
        from pytest import ExitCode

        try:
            ret = str(ExitCode(self.ret))
        except ValueError:
            ret = str(self.ret)
        return (
            f"<RunResult ret={ret} len(stdout.lines)={len(self.stdout.lines)}"
            f" len(stderr.lines)={len(self.stderr.lines)} duration={self.duration:.2f}s>"
        )

    @classmethod
    def parse_summary_nouns(cls, lines):
        """Parse the summary line, normalising to the plural noun pytest
        reports regardless of count (#6505): 1 error -> {"errors": 1}."""
        plural = {"error": "errors", "warning": "warnings"}
        for line in reversed(lines):
            clean = _ANSI_RE.sub("", line)
            if _DURATION_RE.search(clean):
                found = {}
                for count, noun in _OUTCOME_RE.findall(clean):
                    found[plural.get(noun, noun)] = int(count)
                return found
        raise ValueError("Pytest terminal summary report not found")

    def parseoutcomes(self):
        return self.parse_summary_nouns(self.outlines)

    def assert_outcomes(
        self,
        passed=0,
        skipped=0,
        failed=0,
        errors=0,
        xpassed=0,
        xfailed=0,
        warnings=None,
        deselected=None,
    ):
        __tracebackhide__ = True
        actual = self.parseoutcomes()
        expected = {
            "passed": passed,
            "skipped": skipped,
            "failed": failed,
            "errors": errors,
            "xpassed": xpassed,
            "xfailed": xfailed,
        }
        got = {key: actual.get(key, 0) for key in expected}
        assert got == expected, f"assert_outcomes: expected {expected}, got {actual}"
        if warnings is not None:
            assert actual.get("warnings", 0) == warnings
        if deselected is not None:
            assert actual.get("deselected", 0) == deselected


def run_inprocess(args, plugins=(), helper_plugin=None, forwarded_filter_marks=()):
    """Run a whole pytest session in this same interpreter (the native engine
    runs a nested Engine, firing its hooks through the monitored plugin
    manager so a HookRecorder — when one is listening — captures live call
    objects a subprocess JSON relay could never carry).

    Shared by Pytester._inline_run_inprocess (the pytester test-harness API:
    `helper_plugin` is its upstream-parity sentinel, `forwarded_filter_marks`
    forwards the enclosing test's @pytest.mark.filterwarnings) and
    pytest.main() (a real top-level call some other code makes — there is no
    enclosing Pytester, so both of those are absent). Either way this is
    still a *nested* run in the pytest-rs sense: the `pytest` package only
    resolves at all inside an already-running pytest-rs-embedded interpreter,
    so a bare `import pytest; pytest.main()` is already running inside one
    engine session before this ever starts a second.
    """
    from _pytest.pytester import HookRecorder

    import pytest
    import pytest._capture as _capture
    import pytest._logging as _logging
    import pytest._marks as _marks
    import pytest._node as _node
    import pytest._tmp_path as _tmp_path
    import pytest._wcapture as _wcapture
    from pytest._pluginmanager import pluginmanager

    run_args = [str(arg) for arg in args]

    # Snapshot module/path/cwd/env so the inner run does not pollute ours
    # (a separate subprocess used to give this isolation for free).
    modules_before = sys.modules.copy()
    syspath_before = list(sys.path)
    cwd_before = os.getcwd()
    env_before = dict(os.environ)

    # Every non-package conftest.py imports under the bare module name
    # "conftest" (module_name_for). If the OUTER (already-running) session
    # already loaded ITS OWN conftest.py under that same bare name, the inner
    # run's own (different-file) conftest.py collides with it — import file
    # mismatch — purely because this in-process nested run shares sys.modules
    # with the outer one; a real subprocess wouldn't have this problem at
    # all. Drop it here; modules_before/the restore below already puts the
    # outer session's entry back afterward.
    sys.modules.pop("conftest", None)

    # --runxfail monkeypatches pytest.xfail; save/restore so it doesn't
    # leak between nested and outer runs.
    old_xfail = pytest.xfail

    # Per-session global state the native engine mutates lives in shim
    # module singletons; a nested run would leak it to the outer session
    # (e.g. an inner `-x` run setting session.shouldstop, or reconfiguring
    # the mark generator). Swap in fresh state and restore afterwards.
    _sentinel = object()
    old_session_state = _node._session_state
    _node._session_state = {
        "shouldfail": None,
        "shouldstop": None,
        "items": [],
        "session_markers": [],
        "session_keywords": {},
    }
    old_added_marks = list(_node._added_marks)
    _node._added_marks.clear()
    _mark_attrs = ("_config", "_strict", "_markers", "_strict_parametrization_ids")
    old_mark_state = {k: getattr(_marks.mark, k, _sentinel) for k in _mark_attrs}
    # tmp_path machinery: the nested run reconfigures basetemp/retention and
    # records its own pass/fail outcomes; snapshot so the outer run's
    # retention bookkeeping survives (run_nested resets these).
    old_tmp_state = {
        "_given_basetemp": _tmp_path._given_basetemp,
        "_retention_count": _tmp_path._retention_count,
        "_retention_policy": _tmp_path._retention_policy,
        "_call_results": dict(_tmp_path._call_results),
        "_any_failed": _tmp_path._any_failed,
    }

    old_pm_plugins = list(pluginmanager._plugins)
    old_pm_names = dict(pluginmanager._names)
    old_pm_blocked = set(pluginmanager._blocked_plugins)
    old_pm_conftest = set(pluginmanager._conftest_plugins)
    old_pm_specs = dict(pluginmanager._specs)
    old_pm_monitors = list(pluginmanager._call_monitors)
    old_pm_configured = pluginmanager._configured

    reprec = HookRecorder(pluginmanager)
    invocation_plugins = list(plugins)
    if helper_plugin is not None:
        invocation_plugins.append(helper_plugin)
    for plugin in invocation_plugins:
        pluginmanager.register(plugin)
    # The inner run gets a fresh global capture state so its per-item
    # capture bookkeeping does not corrupt the outer session's.
    old_capstate = _capture.state
    inner_capstate = _capture.CaptureState()
    _capture.state = inner_capstate
    # Logging: the inner run reconfigures _logging.state (log_file,
    # log_cli handlers) and may lower the root logger level; swap in
    # fresh state and restore everything afterwards.
    old_logging_state = _logging.state
    old_root_level = logging.getLogger().level
    old_root_handlers = list(logging.getLogger().handlers)
    _logging.state = _logging.LoggingState()
    # Warning capture: the inner run_session calls _wcapture.uninstall()
    # on exit, breaking the outer capture. Save the full warning state
    # and reinstall afterwards so the outer run's capture survives.
    old_wcapture_captured = list(_wcapture.captured)
    old_wcapture_current_test = _wcapture.current_test
    old_wcapture_current_when = _wcapture.current_when
    old_wcapture_original_sw = _wcapture._original_showwarning
    old_wcapture_session_specs = list(_wcapture.session_specs)
    old_warn_filters = list(warnings.filters)
    _wcapture.captured.clear()
    _wcapture.current_test = None
    _wcapture.current_when = "config"
    # Clear __warningregistry__ in all loaded modules so "default" filters
    # don't suppress warnings that were already shown in a previous inner run.
    for _mod in list(sys.modules.values()):
        if _mod is not None and hasattr(_mod, "__warningregistry__"):
            _mod.__warningregistry__.clear()
    # Save/restore threadexception and unraisable module state: the inner
    # engine calls configure()/session_cleanup() which mutate module-level
    # globals (_deque, _prev_hook). Without save/restore the outer engine's
    # hook is clobbered by the inner cleanup.
    import pytest._threadexception as _threadexc
    import pytest._unraisable as _unraisable

    old_threadexc_deque = _threadexc._deque
    old_threadexc_prev = _threadexc._prev_hook
    old_threadexc_hook = threading.excepthook
    old_unraisable_deque = _unraisable._deque
    old_unraisable_prev = _unraisable._prev_hook
    old_unraisable_hook = sys.unraisablehook

    # Redirect fds 1/2 to temp files: the inner run's terminal output
    # (printed by the native engine straight to the fds) is collected
    # here, then echoed into the outer streams so capsys/caplog of the
    # enclosing test see what an in-process run would have printed.
    out_f = tempfile.TemporaryFile()
    err_f = tempfile.TemporaryFile()
    sys.stdout.flush()
    sys.stderr.flush()
    saved_out = os.dup(1)
    saved_err = os.dup(2)
    os.dup2(out_f.fileno(), 1)
    os.dup2(err_f.fileno(), 2)
    # When capture is disabled (-s/--capture=no) the engine leaves the
    # Python-level streams alone, so the inner test's print() output flows
    # to whatever sys.stdout points at — which is the OUTER session's
    # capture object, not the redirected fd. Point sys.stdout/err at the
    # redirected fds so that output lands in out_f/err_f (and thus
    # result.outlines). With capture enabled the engine installs its own
    # fd-level capture, so leave the Python streams untouched to avoid
    # bypassing it.
    _args_str = [str(a) for a in run_args]

    # Short options that consume the rest of the cluster as their own value
    # (e.g. "-rs" is "-r" with value "s", not "-r" clustered with "-s").
    _VALUE_TAKING_SHORT_FLAGS = frozenset("rmkpcnoW")

    def _is_short_flag_cluster_with_s(arg):
        # Clustered short options (e.g. "-vs", "-sv") also disable capture,
        # not just the standalone "-s" — but only up to the first
        # value-taking flag in the cluster, which absorbs the remaining chars.
        if not (arg.startswith("-") and not arg.startswith("--") and arg[1:].isalpha()):
            return False
        for ch in arg[1:]:
            if ch == "s":
                return True
            if ch in _VALUE_TAKING_SHORT_FLAGS:
                return False
        return False

    capture_disabled = (
        any(_is_short_flag_cluster_with_s(a) for a in _args_str)
        or "--capture=no" in _args_str
        or any(
            _args_str[i] == "--capture" and i + 1 < len(_args_str) and _args_str[i + 1] == "no"
            for i in range(len(_args_str))
        )
    )
    saved_sys_out = saved_sys_err = None
    if capture_disabled:
        saved_sys_out, saved_sys_err = sys.stdout, sys.stderr
        sys.stdout = os.fdopen(os.dup(1), "w", buffering=1, errors="replace")
        sys.stderr = os.fdopen(os.dup(2), "w", buffering=1, errors="replace")
    # Forward the outer test's @pytest.mark.filterwarnings to the inner
    # in-process run via env var (same mechanism as subprocess mode).
    _forwarded_env_set = False
    if forwarded_filter_marks:
        os.environ["PYTEST_RS_FORWARDED_FILTERS"] = "\n".join(
            reversed(list(forwarded_filter_marks))
        )
        _forwarded_env_set = True
    _pending_invocation_plugins[:] = invocation_plugins
    try:
        try:
            ret = pytest._native_inline_run(run_args)
        except SystemExit as exc:
            ret = int(exc.code) if exc.code is not None else 0
        except Exception as exc:
            if type(exc).__name__ == "UsageError":
                os.write(2, f"ERROR: {exc}\n".encode())
                ret = 4
            else:
                raise
    finally:
        _pending_invocation_plugins.clear()
        sys.stdout.flush()
        sys.stderr.flush()
        # Restore the Python streams (and close our fd wrappers while fd
        # 1/2 still point at out_f/err_f) before reverting the fds.
        if saved_sys_out is not None:
            for _wrapper, _orig in ((sys.stdout, saved_sys_out), (sys.stderr, saved_sys_err)):
                try:
                    _wrapper.close()
                except Exception:
                    pass
            sys.stdout = saved_sys_out
            sys.stderr = saved_sys_err
        os.dup2(saved_out, 1)
        os.dup2(saved_err, 2)
        os.close(saved_out)
        os.close(saved_err)
        # Stop the inner capture if a run path left it active, so its
        # temp files are closed (else a ResourceWarning leaks).
        try:
            if inner_capstate._capture is not None:
                inner_capstate.session_end()
        except Exception:
            pass
        _capture.state = old_capstate
        # Logging: close the inner run's handlers, restore the root
        # logger's level and handler list, then restore the outer state.
        inner_log = _logging.state
        inner_log.end_phase()
        if inner_log.log_file_handler is not None:
            logging.getLogger().removeHandler(inner_log.log_file_handler)
            inner_log.log_file_handler.close()
        if inner_log.log_cli_handler is not None and not isinstance(
            inner_log.log_cli_handler, _logging._NullCliHandler
        ):
            logging.getLogger().removeHandler(inner_log.log_cli_handler)
        root = logging.getLogger()
        root.handlers[:] = old_root_handlers
        root.setLevel(old_root_level)
        _logging.state = old_logging_state
        reprec.finish_recording()
        pluginmanager._plugins[:] = old_pm_plugins
        pluginmanager._names.clear()
        pluginmanager._names.update(old_pm_names)
        pluginmanager._blocked_plugins.clear()
        pluginmanager._blocked_plugins.update(old_pm_blocked)
        pluginmanager._conftest_plugins.clear()
        pluginmanager._conftest_plugins.update(old_pm_conftest)
        pluginmanager._specs.clear()
        pluginmanager._specs.update(old_pm_specs)
        pluginmanager._call_monitors[:] = old_pm_monitors
        pluginmanager._configured = old_pm_configured
        # Restore the session-state singletons swapped in above.
        pytest.xfail = old_xfail
        _node._session_state = old_session_state
        _node._added_marks[:] = old_added_marks
        for key, value in old_mark_state.items():
            if value is _sentinel:
                if hasattr(_marks.mark, key):
                    delattr(_marks.mark, key)
            else:
                setattr(_marks.mark, key, value)
        _tmp_path._given_basetemp = old_tmp_state["_given_basetemp"]
        _tmp_path._retention_count = old_tmp_state["_retention_count"]
        _tmp_path._retention_policy = old_tmp_state["_retention_policy"]
        _tmp_path._call_results.clear()
        _tmp_path._call_results.update(old_tmp_state["_call_results"])
        _tmp_path._any_failed = old_tmp_state["_any_failed"]
        # Restore warning capture: the inner run_session called
        # _wcapture.uninstall() which broke the outer capture.
        _wcapture.captured[:] = old_wcapture_captured
        _wcapture.current_test = old_wcapture_current_test
        _wcapture.current_when = old_wcapture_current_when
        _wcapture._original_showwarning = old_wcapture_original_sw
        _wcapture.session_specs[:] = old_wcapture_session_specs
        warnings.filters[:] = old_warn_filters
        warnings.showwarning = _wcapture._showwarning
        # Restore threadexception/unraisable module state so the outer
        # engine's hooks survive inner cleanup.
        _threadexc._deque = old_threadexc_deque
        _threadexc._prev_hook = old_threadexc_prev
        threading.excepthook = old_threadexc_hook
        _unraisable._deque = old_unraisable_deque
        _unraisable._prev_hook = old_unraisable_prev
        sys.unraisablehook = old_unraisable_hook
        # Restore the module table, sys.path, cwd, and env.
        for name in list(sys.modules):
            if name not in modules_before:
                del sys.modules[name]
        sys.modules.update(modules_before)
        sys.path[:] = syspath_before
        os.chdir(cwd_before)
        # Restore env: add back removed vars, update changed ones, remove
        # new ones — but do NOT restore PYTEST_CURRENT_TEST (the inner
        # run's teardown already unset it, and the outer runner will re-set
        # it for the outer item's teardown phase).
        _no_restore = {"PYTEST_CURRENT_TEST"}
        for key in list(os.environ):
            if key not in env_before and key not in _no_restore:
                del os.environ[key]
        for key, val in env_before.items():
            if key in _no_restore:
                continue
            if os.environ.get(key) != val:
                os.environ[key] = val

    out_f.seek(0)
    err_f.seek(0)
    outlines = out_f.read().decode(errors="replace").splitlines()
    errlines = err_f.read().decode(errors="replace").splitlines()
    out_f.close()
    err_f.close()
    # Always echo inner run output so outer test capsys/--tb sees it.
    for line in outlines:
        print(line)
    for line in errlines:
        print(line, file=sys.stderr)

    reprec.ret = ret
    # Carry the captured output for tests reaching past the HookRecorder
    # API (e.g. result.stdout / .outlines like the old InlineRunResult).
    result = RunResult(ret, outlines, errlines, 0.0)
    result.reprec = reprec
    reprec._result = result
    reprec.outlines = outlines
    reprec.errlines = errlines
    return reprec


class Pytester:
    # Raised by run()/runpytest_subprocess() when a child overruns its timeout
    # (the same class subprocess raises, so callers can catch either).
    TimeoutExpired = subprocess.TimeoutExpired
    # Sentinel for popen()/run()'s stdin: close the child's stdin pipe. None is
    # a distinct, valid value (leave stdin inherited) per upstream.
    CLOSE_STDIN = object()

    def __init__(self, path, request_name, request=None):
        self.path = pathlib.Path(path)
        self._name = request_name
        self._request = request
        self._syspaths = []
        self.plugins = []
        # Capture the runner path now (fixture setup, before a test body can
        # mock.patch.dict(os.environ, clear=True) around runpytest). The
        # module-level import runs too early — pytest imports this before the
        # engine sets PYTEST_RS_EXE.
        global _RUNNER_EXE, _RUNNER_PYTHONPATH
        if _RUNNER_EXE is None:
            _RUNNER_EXE = os.environ.get("PYTEST_RS_EXE")
        if _RUNNER_PYTHONPATH is None:
            raw = os.environ.get("PYTHONPATH")
            if raw:
                _RUNNER_PYTHONPATH = os.pathsep.join(
                    os.path.abspath(p) for p in raw.split(os.pathsep)
                )
        for _var in _LIBPATH_VARS:
            if _var not in _RUNNER_LIBPATH and _var in os.environ:
                _RUNNER_LIBPATH[_var] = os.environ[_var]

    @staticmethod
    def _source_text(source):
        # makepyfile accepts utf-8 bytes as well as str/Source (#2738).
        # A list/tuple (upstream's Source(obj)) is joined line-by-line —
        # str(source) would otherwise write the Python repr of the list.
        if isinstance(source, (list, tuple)):
            return "\n".join(Pytester._source_text(line) for line in source)
        return source.decode("utf-8") if isinstance(source, bytes) else str(source)

    def _makefile(self, ext, args, kwargs):
        items = list(kwargs.items())
        if args:
            source = "\n".join(self._source_text(arg) for arg in args)
            items.insert(0, (self._name, source))
        paths = []
        for basename, source in items:
            import textwrap

            # with_suffix both appends and replaces ("pkg/test_1.py" stays
            # itself), matching upstream pytester.
            path = (self.path / basename).with_suffix(ext)
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(textwrap.dedent(self._source_text(source)).strip())
            paths.append(path)
        # pytest returns the first file's path even for multiple files.
        return paths[0]

    def makepyfile(self, *args, **kwargs):
        return self._makefile(".py", args, kwargs)

    def makeconftest(self, source):
        return self._makefile(".py", [], {"conftest": source})

    def maketxtfile(self, *args, **kwargs):
        return self._makefile(".txt", args, kwargs)

    def makeini(self, source):
        return self._makefile(".ini", [], {"tox": source})

    def makepyprojecttoml(self, source):
        return self._makefile(".toml", [], {"pyproject": source})

    def maketoml(self, source):
        """Write a pytest.toml file."""
        return self._makefile(".toml", [], {"pytest": source})

    def makefile(self, ext, *args, **kwargs):
        if ext and not ext.startswith("."):
            raise ValueError(
                f"pytester.makefile expects a file extension, try .{ext} instead of {ext}"
            )
        return self._makefile(ext, args, kwargs)

    def mkdir(self, name):
        path = self.path / name
        path.mkdir()
        return path

    def syspathinsert(self, path=None):
        entry = str(path if path is not None else self.path)
        # The current process (tests import what they just wrote) and the
        # child runner via PYTHONPATH (runs are subprocesses).
        sys.path.insert(0, entry)
        self._syspaths.insert(0, entry)

    def runpytest(
        self, *args, timeout=None, syspathinsert=False, no_reraise_ctrlc=False, plugins=()
    ):
        # syspathinsert=True: insert self.path into sys.path so the child can
        # import test-local plugins written to self.path.
        if syspathinsert:
            self.syspathinsert()
        # Upstream's default is in-process (shares module state); we default to
        # subprocess but switch to in-process when the env var is set — this is
        # needed for tests that monkeypatch module-level state and check it
        # after the inner run (e.g. subtests pdb tests).
        _check_cfg_pytest_section(self.path, args)
        if "run" in vars(self):
            # A test replaced .run() to intercept the raw subprocess (e.g. to
            # spy on a long-running --looponfail process). Mirror upstream's
            # Pytester.runpytest_subprocess, which always spawns through
            # self.run(), so the monkeypatch takes effect.
            return self._runpytest_subprocess_via_run(args, timeout=timeout)
        if os.environ.get("PYTEST_RS_INLINE_INPROCESS"):
            reprec = self._inline_run_inprocess(*args, plugins=plugins)
            return getattr(reprec, "_result", reprec)
        return self._runpytest(args, timeout=timeout, forward_filters=True)

    def _runpytest(self, args, *, timeout=None, forward_filters=False, hook_relay=None):
        forwarded_filters = []
        if forward_filters and self._request is not None:
            # Only the outer item's filterwarnings marks — forwarding the
            # whole session ini filter set (e.g. a suite-wide "error")
            # changes far more child behavior than upstream's in-process
            # nesting is worth. The child applies these at the LOWEST
            # priority (before its own ini filters), matching upstream's
            # in-process nesting where the inner run's filters layer on top.
            marks = [
                str(mark.args[0])
                for mark in self._request.node.iter_markers("filterwarnings")
                if mark.args
            ]
            forwarded_filters = list(reversed(marks))  # farthest first

        exe = os.environ.get("PYTEST_RS_EXE") or _RUNNER_EXE
        if exe is None:
            fail("PYTEST_RS_EXE is not set; pytester cannot run the runner")
        env = os.environ.copy()
        # The engine binary needs its loader path to find libpython; restore it
        # if the test cleared the environment (setdefault keeps a test's own).
        for _var, _value in _RUNNER_LIBPATH.items():
            env.setdefault(_var, _value)
        # Restore pre-monkeypatch COLUMNS so plugins that capture terminal
        # width at import time (e.g. pytest-icdiff) see the original width,
        # matching upstream's in-process pytester where imports precede tests.
        if _RUNNER_COLUMNS is not None:
            env["COLUMNS"] = _RUNNER_COLUMNS
        else:
            env.pop("COLUMNS", None)
        # Keep installed plugins importable even when the test cleared the
        # environment (upstream's in-process pytester shares the parent's
        # sys.path); fall back to the PYTHONPATH captured at fixture setup.
        # Upstream popen() also prepends os.getcwd() (= pytester.path after
        # chdir) so test-local packages are importable in nested runs.
        existing = env.get("PYTHONPATH") or _RUNNER_PYTHONPATH
        entries = [os.getcwd(), *self._syspaths, *([existing] if existing else [])]
        env["PYTHONPATH"] = os.pathsep.join(filter(None, entries))
        # Upstream pytester parity: nested runs get a numbered --basetemp
        # under this pytester dir, so their tmp dirs are cleaned up with it
        # (a later user-passed --basetemp still wins).
        n = sum(1 for p in self.path.glob("runpytest-*"))
        basetemp = self.path / f"runpytest-{n}"
        # The child relays its log records here; they are replayed into this
        # process after the run (upstream's in-process runpytest propagates
        # the inner run's records to the parent's caplog).
        relay = self.path / f".logrelay-{n}"
        relay.unlink(missing_ok=True)
        env["PYTEST_RS_LOG_RELAY"] = str(relay)
        if forwarded_filters:
            env["PYTEST_RS_FORWARDED_FILTERS"] = "\n".join(forwarded_filters)
        else:
            env.pop("PYTEST_RS_FORWARDED_FILTERS", None)
        # Hook relay: when set, the child records selected hook events to a
        # JSON file so InlineRunResult.getcalls() can reconstruct them.
        extra_args = []
        if hook_relay is not None:
            hook_relay.unlink(missing_ok=True)
            env["PYTEST_RS_HOOK_RELAY"] = str(hook_relay)
            extra_args = ["-p", "pytest._hook_relay_plugin"]
        else:
            env.pop("PYTEST_RS_HOOK_RELAY", None)
        start = time.perf_counter()
        # cwd: use the live process cwd so the inner run's invocation dir
        # matches upstream's in-process runpytest. The pytester fixture
        # chdir's to self.path at setup, so this is normally self.path; a
        # test that os.chdir()s (e.g. via monkeypatch) is honored, which
        # rootdir discovery (determine_setup's invocation_dir) depends on.
        proc = subprocess.run(
            [exe, f"--basetemp={basetemp}", *extra_args, *[str(arg) for arg in args]],
            cwd=os.getcwd(),
            capture_output=True,
            text=True,
            timeout=timeout if timeout is not None else 120,
            env=env,
        )
        duration = time.perf_counter() - start
        self._replay_child_logs(relay)
        # Color is gated by --color/tty detection in the engine; pytester
        # passes output through raw so color tests can assert escapes.
        outlines = proc.stdout.splitlines()
        errlines = proc.stderr.splitlines()
        return RunResult(proc.returncode, outlines, errlines, duration)

    @staticmethod
    def _replay_child_logs(path):
        """Re-emit log records the child run relayed (PYTEST_RS_LOG_RELAY)
        into this process's logging system, gated by each logger's effective
        level like a live emission would be."""
        try:
            data = path.read_bytes()
        except OSError:
            return
        path.unlink(missing_ok=True)
        buf = io.BytesIO(data)
        while True:
            try:
                payload = pickle.load(buf)
            except Exception:
                break
            record = logging.makeLogRecord(payload)
            logger = logging.getLogger(record.name)
            if logger.isEnabledFor(record.levelno):
                logger.handle(record)

    def runpytest_subprocess(self, *args, timeout=None):
        # Upstream subprocess runs do NOT inherit the outer warning filters.
        __tracebackhide__ = True
        for plugin in self.plugins:
            if not isinstance(plugin, str):
                raise ValueError(
                    f"plugins as objects is not supported in pytester subprocess mode; "
                    f"specify by name instead: {plugin}"
                )
        if "run" in vars(self):
            # A test replaced .run() to intercept the raw subprocess (e.g. to
            # spy on a long-running --looponfail process). Mirror upstream's
            # Pytester.runpytest_subprocess, which always spawns through
            # self.run(), so the monkeypatch takes effect.
            return self._runpytest_subprocess_via_run(args, timeout=timeout)
        return self._runpytest(args, timeout=timeout, forward_filters=False)

    def _runpytest_subprocess_via_run(self, args, *, timeout=None):
        # Spawn the pytest-rs binary directly (not upstream's `python -mpytest`)
        # so a test's monkeypatched .run() still exercises pytest-rs itself.
        exe = os.environ.get("PYTEST_RS_EXE") or _RUNNER_EXE
        if exe is None:
            fail("PYTEST_RS_EXE is not set; pytester cannot run the runner")
        n = sum(1 for p in self.path.glob("runpytest-*"))
        basetemp = self.path / f"runpytest-{n}"
        basetemp.mkdir(mode=0o700)
        args = (f"--basetemp={basetemp}", *args)
        for plugin in self.plugins:
            args = ("-p", plugin, *args)
        args = (exe, *args)
        return self.run(*args, timeout=timeout)

    def runpytest_inprocess(self, *args, timeout=None, plugins=()):
        """Run pytest in-process (shares sys state with the outer test)."""
        reprec = self._inline_run_inprocess(*args, plugins=plugins)
        result = getattr(reprec, "_result", None)
        if result is not None:
            result.reprec = reprec
            return result
        return reprec

    def make_hook_recorder(self, pluginmanager):
        """Attach a HookRecorder to ``pluginmanager`` and finish recording at
        the end of the test (upstream API)."""
        from _pytest.pytester import HookRecorder

        pluginmanager.reprec = reprec = HookRecorder(pluginmanager, _ispytest=True)
        if self._request is not None:
            self._request.addfinalizer(reprec.finish_recording)
        return reprec

    def _subprocess_env(self):
        """Child env that keeps libpython + installed plugins reachable even
        after a test clears os.environ (shared by popen/run)."""
        env = os.environ.copy()
        for _var, _value in _RUNNER_LIBPATH.items():
            env.setdefault(_var, _value)
        existing = env.get("PYTHONPATH") or _RUNNER_PYTHONPATH
        # Upstream popen() prepends os.getcwd() (= pytester.path after chdir)
        # so that test-local packages (e.g. tpkg/) are importable in subprocesses.
        cwd = os.getcwd()
        entries = [cwd, *self._syspaths, *([existing] if existing else [])]
        env["PYTHONPATH"] = os.pathsep.join(filter(None, entries))
        # Ensure console_scripts (e.g. pytest-bdd) installed in the venv's
        # bin/ directory are discoverable by subprocess calls.
        bindir = os.path.dirname(sys.executable)
        path = env.get("PATH", "")
        if bindir not in path.split(os.pathsep):
            env["PATH"] = bindir + os.pathsep + path
        return env

    def _getpytestargs(self):
        # Match upstream Pytester._getpytestargs: spawn `python -mpytest`.
        # Under the conformance PYTHONPATH this resolves to the upstream
        # pytest package, so behaviors such as broken-pipe suppression
        # (test_no_brokenpipeerror_message) match real pytest.
        return (sys.executable, "-mpytest")

    def popen(
        self, cmdargs, stdout=subprocess.PIPE, stderr=subprocess.PIPE, stdin=CLOSE_STDIN, **kw
    ):
        """Spawn a subprocess. ``stdin`` may be CLOSE_STDIN (close the pipe),
        bytes (written then left open for communicate()), or any value passed
        straight to Popen (e.g. PIPE, None)."""
        cmdargs = [str(arg) for arg in cmdargs]
        kw["env"] = self._subprocess_env()
        if stdin is self.CLOSE_STDIN or isinstance(stdin, bytes):
            kw["stdin"] = subprocess.PIPE
        else:
            kw["stdin"] = stdin
        popen = subprocess.Popen(cmdargs, stdout=stdout, stderr=stderr, **kw)
        if stdin is self.CLOSE_STDIN:
            assert popen.stdin is not None
            popen.stdin.close()
        elif isinstance(stdin, bytes):
            assert popen.stdin is not None
            popen.stdin.write(stdin)
        return popen

    def run(self, *cmdargs, timeout=None, stdin=CLOSE_STDIN):
        """Run a command, capturing stdout/stderr to RunResult. Raises
        ``Pytester.TimeoutExpired`` (== subprocess.TimeoutExpired) on overrun."""
        __tracebackhide__ = True
        cmdargs = [str(arg) for arg in cmdargs]
        p1 = self.path / "stdout"
        p2 = self.path / "stderr"
        start = time.perf_counter()
        with open(p1, "w", encoding="utf8") as f1, open(p2, "w", encoding="utf8") as f2:
            popen = self.popen(cmdargs, stdout=f1, stderr=f2, stdin=stdin)
            if isinstance(stdin, bytes):
                popen.stdin.close()
            try:
                ret = popen.wait(timeout)
            except subprocess.TimeoutExpired:
                popen.kill()
                popen.wait()
                raise
            finally:
                # Close any stdin pipe we left open (e.g. stdin=PIPE) so it
                # doesn't surface later as an unraisable ResourceWarning.
                if popen.stdin is not None and not popen.stdin.closed:
                    popen.stdin.close()
        duration = time.perf_counter() - start
        out = p1.read_text(encoding="utf8").splitlines()
        err = p2.read_text(encoding="utf8").splitlines()
        return RunResult(ret, out, err, duration)

    def chdir(self):
        """Cd into the pytester temporary directory. The pytester fixture
        already chdir's here at setup; this restores it after a test that
        wandered elsewhere (upstream API parity)."""
        os.chdir(self.path)

    def parseconfig(self, *args):
        """Return an in-process pytest Config built from the given
        command-line args (rootdir discovery, ini reading, option parsing),
        without running a session — upstream's _prepareconfig."""
        from _pytest.config import _native_prepareconfig
        from _pytest.config import parse as _warn_on_initial_conftest_failure

        from pytest._pluginmanager import pluginmanager

        new_args = [str(arg) for arg in args]
        # Register pytester plugins into the shared pluginmanager up front so
        # they receive hook calls (e.g. pytest_warning_recorded) fired either
        # below (--help/--version) or during conftest loading inside
        # _fire_addoption.
        extra_plugins = [p for p in getattr(self, "plugins", []) if not isinstance(p, str)]
        for plugin in extra_plugins:
            pluginmanager.register(plugin)
        try:
            if any(arg in ("-h", "--help", "-V", "--version") for arg in new_args):
                # --help/--version short-circuits to SystemExit before the
                # in-process config ever reaches conftest loading
                # (_fire_addoption below); attempt it here so a broken
                # conftest still surfaces as the warning upstream issues
                # instead of being silently skipped.
                conftest = self.path / "conftest.py"
                if conftest.is_file():
                    _warn_on_initial_conftest_failure(conftest)
            try:
                config = _native_prepareconfig(new_args)
            except SystemExit:
                # --help/--version prints output and exits; tolerate so
                # callers can still inspect warnings fired before the early
                # exit.
                return None
            config._mark_as_parsed()
            self._fire_addoption(config, new_args)
        finally:
            for plugin in extra_plugins:
                pluginmanager.unregister(plugin)
        _validate_required_plugins(config)
        pm = config.pluginmanager
        pm.consider_preparse(new_args)
        if not config.getoption("disable_plugin_autoload", default=False):
            pm.consider_setuptools_entrypoints()
        pm.consider_env()
        if self._request is not None:
            self._request.addfinalizer(config._ensure_unconfigure)
        return config

    def _fire_addoption(self, config, args):
        """Fire pytest_addoption from the rootdir conftest and any
        ``pytester.plugins`` so custom addini/addoption declarations resolve
        through config.getini/getoption. The shared parser registries are
        snapshot/restored around this config's lifetime to avoid leaking the
        test's custom options into the outer session."""
        from pytest import _parser
        from pytest._pluginmanager import _accepted_kwargs, pluginmanager

        snapshots = {
            reg: dict(getattr(_parser, reg))
            for reg in ("ini_specs", "ini_aliases", "option_specs", "flag_dests")
        }

        def restore():
            for reg, snap in snapshots.items():
                live = getattr(_parser, reg)
                live.clear()
                live.update(snap)

        if self._request is not None:
            self._request.addfinalizer(restore)

        plugins = []
        conftest = self.path / "conftest.py"
        if conftest.is_file():
            mod = self._import_parseconfig_conftest(conftest)
            if mod is not None:
                plugins.append(mod)
        for plugin in getattr(self, "plugins", []):
            if not isinstance(plugin, str):
                plugins.append(plugin)

        for plugin in plugins:
            add = getattr(plugin, "pytest_addoption", None)
            if callable(add):
                add(
                    **_accepted_kwargs(
                        add, {"parser": _parser.parser, "pluginmanager": pluginmanager}
                    )
                )

        # Apply the parseconfig CLI flags (e.g. "--hello=this") now that their
        # options are registered, so config.getoption sees the parsed values.
        _parser.apply_cli_args(config.option, list(args))

    @staticmethod
    def _import_parseconfig_conftest(path):
        conftest_dir = str(pathlib.Path(path).parent)
        added_to_path = conftest_dir not in sys.path
        if added_to_path:
            sys.path.insert(0, conftest_dir)
        try:
            spec = importlib.util.spec_from_file_location("_pytester_parseconfig_conftest", path)
            mod = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(mod)
            return mod
        except Exception:
            return None
        finally:
            if added_to_path and conftest_dir in sys.path:
                sys.path.remove(conftest_dir)

    @staticmethod
    def _import_conftest_module(path):
        """Import a conftest.py under a path-derived unique module name so that
        multiple conftests along a package chain don't clobber one another."""
        path = pathlib.Path(str(path)).resolve()
        mod_name = "_pytester_conftest_" + str(abs(hash(str(path))))
        try:
            spec = importlib.util.spec_from_file_location(mod_name, path)
            if spec is None or spec.loader is None:
                return None
            mod = importlib.util.module_from_spec(spec)
            sys.modules[mod_name] = mod
            spec.loader.exec_module(mod)
            return mod
        except Exception:
            return None

    def parseconfigure(self, *args):
        """Like parseconfig, but also runs the pytest_configure step."""
        config = self.parseconfig(*args)
        config._do_configure()
        if self._request is not None:
            self._request.addfinalizer(config._ensure_unconfigure)
        return config

    def _hook_relay_path(self, prefix):
        n = sum(1 for p in self.path.glob(f".{prefix}-*"))
        return self.path / f".{prefix}-{n}"

    def inline_run(self, *args, plugins=()):
        # Two backends: a subprocess run parsed into a HookRecorder-shaped
        # result (the default, robust for the whole suite) and an in-process
        # nested run that captures live hook-call objects via a real
        # HookRecorder. The in-process path is still being hardened (hook
        # instrumentation + session-global save/restore), so it is opt-in
        # behind PYTEST_RS_INLINE_INPROCESS until it is net-positive.
        if os.environ.get("PYTEST_RS_INLINE_INPROCESS"):
            return self._inline_run_inprocess(*args, plugins=plugins)
        return self._inline_run_subprocess(*args)

    def _inline_run_subprocess(self, *args):
        # A subprocess -v run parsed into a HookRecorder-shaped result
        # (ret / assertoutcome / listoutcomes). The child's output is echoed
        # so capsys sees what an in-process run would have printed.
        hook_relay = self._hook_relay_path("hookrelay")
        result = self._runpytest(
            ("-v", *[str(arg) for arg in args]),
            forward_filters=True,
            hook_relay=hook_relay,
        )
        if result.outlines:
            sys.stdout.write("\n".join(result.outlines) + "\n")
        if result.errlines:
            sys.stderr.write("\n".join(result.errlines) + "\n")
        hook_events = []
        try:
            hook_events = json.loads(hook_relay.read_text(encoding="utf-8"))
        except (OSError, Exception):
            pass
        return InlineRunResult(result, hook_events)

    def _inline_run_inprocess(self, *args, plugins=()):
        # In-process nested run: the native engine runs a whole session in
        # this process and fires its hooks through the monitored plugin
        # manager, so the HookRecorder captures live call objects (getcalls,
        # getreports, assertoutcome) — including custom hooks a subprocess
        # JSON relay could never carry. `run_inprocess` (module-level, shared
        # with pytest.main()) does the actual work; this just supplies the
        # two pieces that only make sense with a real Pytester behind them.
        forwarded_filter_marks = ()
        if self._request is not None:
            forwarded_filter_marks = [
                str(mark.args[0])
                for mark in self._request.node.iter_markers("filterwarnings")
                if mark.args
            ]
        return run_inprocess(
            args,
            plugins=plugins,
            helper_plugin=PytesterHelperPlugin(),
            forwarded_filter_marks=forwarded_filter_marks,
        )

    def inline_runsource(self, source, *args):
        path = self.makepyfile(source)
        return self.inline_run(*args, path)

    def inline_genitems(self, *args):
        """Run collection-only mode and return (items, reprec).

        Items are lightweight objects with .nodeid, .name, and .parent attributes.
        """
        hook_relay = self._hook_relay_path("hookrelay")
        result = self._runpytest(
            ("--collect-only", "-q", *[str(arg) for arg in args]),
            hook_relay=hook_relay,
        )
        hook_events = []
        try:
            hook_events = json.loads(hook_relay.read_text(encoding="utf-8"))
        except (OSError, Exception):
            pass
        reprec = InlineRunResult(result, hook_events)
        # Collect items in-process so they carry full mark data
        # (keywords, get_closest_marker) — needed by mark-evaluation tests.
        # Fall back to nodeid-only stubs for doctest/text files.

        # When called with no path args (or only CLI flags), the subprocess knows
        # the right collection (respects testpaths, cwd, etc.) — use relay items directly.
        def _is_path_arg(a):
            s = str(a)
            return not s.startswith("-") and (
                s.endswith(".py") or "::" in s or pathlib.Path(s).exists()
            )

        path_args = [a for a in args if _is_path_arg(a)]
        if path_args:
            collect_args = path_args
        else:
            # No path given: collect the pytester rootdir in-process so callers
            # get live Function nodes (isinstance checks, TopRequest(item)) rather
            # than relay stubs. Fall back to relay items if it yields nothing.
            inproc, _ = self._genitems_from_dir(self.path, args)
            if inproc:
                return inproc, reprec
            items = []
            for event in hook_events:
                if event.get("hook") == "pytest_collection_finish":
                    items = [
                        _RelayItemResult(it["name"], it["nodeid"], it.get("path", ""))
                        for it in event.get("session_items", [])
                    ]
                    break
            return items, reprec

        collect_args = path_args

        # Doctest collection mode, for the DoctestItem-producing branches below.
        args_str = [str(a) for a in args]
        doctest_modules = "--doctest-modules" in args_str
        glob_patterns = []
        for _i, _a in enumerate(args_str):
            if _a.startswith("--doctest-glob="):
                glob_patterns.append(_a.split("=", 1)[1])
            elif _a == "--doctest-glob" and _i + 1 < len(args_str):
                glob_patterns.append(args_str[_i + 1])
        if not glob_patterns:
            glob_patterns = ["test*.txt"]
        config_obj = self._request.config if self._request is not None else None

        def _doctest_items(file_path):
            from _pytest.doctest import inprocess_doctest_items

            fp = pathlib.Path(file_path)
            try:
                nb = str(fp.relative_to(self.path))
            except ValueError:
                nb = fp.name
            return inprocess_doctest_items(str(fp), config_obj, doctest_modules, glob_patterns, nb)

        items = []
        for arg in collect_args:
            path_str = str(arg)
            # Handle "file.py::Class::method" — collect file then filter by nodeid suffix
            nodeid_filter = None
            if "::" in path_str:
                file_part, _, nodeid_filter = path_str.partition("::")
                if file_part.endswith(".py"):
                    all_items = self._collect_items_from_path(file_part)
                    # Filter to items whose nodeid ends with the requested suffix
                    filter_suffix = "::" + nodeid_filter
                    matched = [i for i in all_items if i.nodeid.endswith(filter_suffix)]
                    items.extend(matched if matched else all_items)
                    continue
            p = pathlib.Path(path_str)
            if not p.is_absolute():
                p = self.path / p
            if p.is_dir():
                config = self._request.config if self._request is not None else None
                python_files = (
                    "test_*.py"
                    if config is None
                    else (
                        config.getini("python_files") if hasattr(config, "getini") else "test_*.py"
                    )
                )
                patterns = (
                    python_files.split() if isinstance(python_files, str) else list(python_files)
                )
                seen_files = set()
                for pat in patterns:
                    for py_file in sorted(p.rglob(pat)):
                        if py_file.is_file() and py_file not in seen_files:
                            # Skip __pycache__ and hidden dirs
                            if any(part.startswith((".", "__pycache__")) for part in py_file.parts):
                                continue
                            seen_files.add(py_file)
                            items.extend(self._collect_items_from_path(py_file))
                # Doctests in the directory: module doctests under
                # --doctest-modules, plus text files matching --doctest-glob.
                seen_dt: set = set()
                if doctest_modules:
                    for py_file in sorted(p.rglob("*.py")):
                        if not py_file.is_file() or py_file in seen_dt:
                            continue
                        if any(part.startswith((".", "__pycache__")) for part in py_file.parts):
                            continue
                        seen_dt.add(py_file)
                        items.extend(_doctest_items(py_file))
                for pat in glob_patterns:
                    for tf in sorted(p.rglob(pat)):
                        if not tf.is_file() or tf in seen_dt:
                            continue
                        if any(part.startswith((".", "__pycache__")) for part in tf.parts):
                            continue
                        seen_dt.add(tf)
                        items.extend(_doctest_items(tf))
            elif path_str.endswith(".py"):
                in_proc = self._collect_items_from_path(path_str)
                # Under --doctest-modules a .py file also yields DoctestItems.
                doctest_items = _doctest_items(p) if doctest_modules else []
                # Supplement with relay items for custom collectors (e.g. pytest_collect_file
                # hooks that return non-standard File subclasses alongside the normal Module),
                # skipping anything already produced in-process (functions + doctests).
                known_nodeids = {i.nodeid for i in in_proc}
                known_nodeids.update(i.nodeid for i in doctest_items)
                # Under --doctest-modules the subprocess also reports the
                # doctests (different nodeid shape than ours), so skip the
                # custom-collector relay supplement to avoid double-counting —
                # the doctests already come from _doctest_items above.
                for event in hook_events if not doctest_modules else []:
                    if event.get("hook") == "pytest_collection_finish":
                        for it in event.get("session_items", []):
                            it_path_str = it.get("path", "")
                            it_nodeid = it.get("nodeid", "")
                            if it_nodeid in known_nodeids:
                                continue
                            if it_path_str:
                                it_abs = pathlib.Path(it_path_str).resolve()
                                src_abs = (
                                    pathlib.Path(path_str)
                                    if pathlib.Path(path_str).is_absolute()
                                    else self.path / path_str
                                ).resolve()
                                if it_abs == src_abs:
                                    in_proc.append(
                                        _RelayItemResult(it["name"], it_nodeid, it_path_str)
                                    )
                        break
                items.extend(in_proc)
                items.extend(doctest_items)
            elif path_str.endswith((".txt", ".rst", ".md")):
                # Real DoctestItem(s) with a DoctestTextfile parent when the
                # file matches a --doctest-glob pattern; empty/non-matching
                # files yield nothing.
                items.extend(_doctest_items(p))
        return items, reprec

    @staticmethod
    def _param_id(val):
        """A node-ID fragment for a single parametrize value (str(val), with
        None spelled out). Faithful enough for the int/string/explicit-id cases
        the in-process collection helpers exercise."""
        if val is None:
            return "None"
        return str(val)

    @staticmethod
    def _expand_params(source_marks, module_marks, base_name, make_fn, scope_caches=None):
        """Expand the parametrize marks in source_marks into one node per
        combination, building each via make_fn(param_name, all_marks).

        source_marks are the marks that may carry parametrize (the function's
        own marks followed by any class marks); module_marks are folded into
        every node's mark list. pytest.param(..., marks=...) contributes its
        per-param marks only to the combinations that use it, and pytest.param
        ids / the ``ids=`` kwarg override the value-derived fragment.

        scope_caches (optional): {"module": dict, "class": dict} identity
        caches for direct (non-indirect) parametrize argnames at scope >
        function — mirrors upstream's per-scope-node ``name2pseudofixturedef``
        stash, so e.g. a "class"-scoped parametrize with no real Class (which
        upstream falls back to module scope for) shares its pseudo FixtureDef
        with another function's explicit "module"-scoped parametrize of the
        same argname when both caches point at the same dict. Package/session
        scope isn't cached here (always a fresh FixtureDef) — this pytester
        shim only serves single-module static introspection."""
        from pytest._marks import HIDDEN_PARAM, ParamSpec, get_unpacked_marks  # noqa: F401

        param_marks = [m for m in source_marks if m.name == "parametrize"]
        base_marks = [m for m in source_marks if m.name != "parametrize"]
        base_marks = [*base_marks, *module_marks]

        if not param_marks:
            return [make_fn(base_name, base_marks)]

        # Pseudo FixtureDefs for direct (no indirect=) parametrize argnames:
        # upstream's Metafunc.parametrize always registers one in
        # _fixtureinfo.name2fixturedefs, even for plain positional test args.
        from pytest._fixturemanager import ShimFixtureDef

        pseudo_fixturedefs = {}
        for pm in param_marks:
            argnames_raw = pm.args[0] if pm.args else pm.kwargs.get("argnames", "")
            names = (
                [a.strip() for a in argnames_raw.split(",") if a.strip()]
                if isinstance(argnames_raw, str)
                else list(argnames_raw)
            )
            indirect = pm.kwargs.get("indirect", False)
            indirect_names = set(names) if indirect is True else set(indirect or ())
            scope = pm.kwargs.get("scope") or "function"
            cache = scope_caches.get(scope) if scope_caches else None
            for argname in names:
                if argname in indirect_names:
                    continue
                if cache is not None and argname in cache:
                    fixturedef = cache[argname]
                else:
                    fixturedef = ShimFixtureDef(argname=argname, func=None, scope=scope)
                    if cache is not None:
                        cache[argname] = fixturedef
                pseudo_fixturedefs[argname] = fixturedef

        levels = []  # each level: list of (id_fragment, [per_param_marks])
        for pm in param_marks:
            argvalues = list(pm.args[1]) if len(pm.args) > 1 else []
            argnames_raw = pm.args[0] if pm.args else pm.kwargs.get("argnames", "")
            nargs = (
                len(argnames_raw.split(",")) if isinstance(argnames_raw, str) else len(argnames_raw)
            )
            ids_kwarg = pm.kwargs.get("ids", None)
            level = []
            for i, val in enumerate(argvalues):
                if isinstance(val, ParamSpec):
                    pvalues, pmarks, pid = val.values, list(val.marks), val.id
                else:
                    if nargs > 1 and isinstance(val, (tuple, list)):
                        pvalues, pmarks, pid = tuple(val), [], None
                    else:
                        pvalues, pmarks, pid = (val,), [], None
                if pid is HIDDEN_PARAM:
                    frag = None
                elif ids_kwarg is not None and i < len(ids_kwarg) and ids_kwarg[i] is HIDDEN_PARAM:
                    frag = None
                elif pid is not None and isinstance(pid, str):
                    frag = pid
                elif ids_kwarg is not None and i < len(ids_kwarg) and ids_kwarg[i] is not None:
                    frag = str(ids_kwarg[i])
                else:
                    frag = "-".join(Pytester._param_id(v) for v in pvalues)
                level.append((frag, pmarks))
            if level:
                levels.append(level)

        if not levels:
            items = [make_fn(base_name, base_marks)]
        else:
            items = []
            for combo in itertools.product(*levels):
                frags = [frag for frag, _ in combo if frag is not None]
                combo_marks = [mk for _, marks in combo for mk in marks]
                if frags:
                    items.append(
                        make_fn(f"{base_name}[{'-'.join(frags)}]", [*base_marks, *combo_marks])
                    )
                else:
                    items.append(make_fn(base_name, [*base_marks, *combo_marks]))

        if pseudo_fixturedefs:
            from _pytest.fixtures import FuncFixtureInfo

            name2fixturedefs = {name: [fd] for name, fd in pseudo_fixturedefs.items()}
            for item in items:
                fixturenames = list(getattr(item, "fixturenames", []))
                item._fixtureinfo = FuncFixtureInfo(
                    argnames=tuple(fixturenames),
                    initialnames=tuple(fixturenames),
                    names_closure=fixturenames,
                    name2fixturedefs=name2fixturedefs,
                )
        return items

    def _collect_items_from_path(self, path, parent_collector=None):
        """In-process collection of Function items from an existing .py file.

        Returns items with full mark data (own_markers, get_closest_marker,
        keywords) — the same objects getitems() returns, but without needing
        source text."""
        from _pytest.compat import getfuncargnames

        from pytest._marks import get_unpacked_marks
        from pytest._metafunc import Metafunc
        from pytest._node import Class, File, Function, _ModuleCollector, _NodeSession
        from pytest._outcomes import OutcomeException
        from pytest._pluginmanager import instance_hook_impls

        # Scope-sorted fixture closure (request.fixturenames) for the collected
        # Function nodes, mirroring getfixtureclosure: autouse + requested seed,
        # transitive deps, then a stable sort by scope. Covers module- and
        # class-defined fixtures (conftest/package fixtures are not gathered).
        _scope_order = {"session": 0, "package": 1, "module": 2, "class": 3, "function": 4}

        def _gather_fixturedefs(namespace, owning_cls=None):
            defs, autouse = {}, []
            for nm, ob in namespace:
                marker = getattr(ob, "_pytestfixturefunction", None)
                if marker is None:
                    continue
                fname = getattr(marker, "name", None) or nm
                scope = getattr(marker, "scope", "function")
                if not isinstance(scope, str):
                    scope = "function"
                try:
                    real = ob.__wrapped__ if hasattr(ob, "__wrapped__") else ob
                    # A class-defined fixture's first parameter binds to the
                    # class/instance (cls/self) — drop it like pytest does.
                    anames = list(getfuncargnames(real, name=nm, cls=owning_cls))
                except Exception as _e:
                    emsg = str(_e)
                    if emsg:
                        print(emsg)
                    anames = []
                    real = ob
                # The 4-tuple carries the callable and owning class so an
                # in-process request can actually execute the fixture
                # (_fillfixtures / getfixturevalue); _closure_for only reads
                # [0]=scope and [1]=argnames, so the extra fields are inert there.
                defs[fname] = (scope, anames, real, owning_cls)
                if getattr(marker, "autouse", False):
                    autouse.append(fname)
            return defs, autouse

        def _closure_for(requested, cls):
            fdefs = dict(_mod_fdefs)
            autouse = list(_mod_autouse)
            if cls is not None:
                cdefs, cau = _gather_fixturedefs(
                    [(n, getattr(cls, n, None)) for n in dir(cls)], owning_cls=cls
                )
                fdefs.update(cdefs)
                autouse += [a for a in cau if a not in autouse]
            names, seen = [], set()
            for n in [*autouse, *requested]:
                if n not in seen:
                    seen.add(n)
                    names.append(n)
            i = 0
            while i < len(names):
                for dep in fdefs.get(names[i], ("function", []))[1]:
                    if dep != "request" and dep not in seen:
                        seen.add(dep)
                        names.append(dep)
                i += 1
            names.sort(key=lambda n: _scope_order.get(fdefs.get(n, ("function", []))[0], 4))
            return names

        path = pathlib.Path(str(path))
        if not path.is_absolute():
            path = self.path / path
        path = path.resolve()
        # The nodeid's file part is the path relative to the pytester rootdir
        # (e.g. "pkg/__init__.py"), not just the bare filename — a nested
        # file's directory prefix must survive so its nodeid matches the
        # real engine's (relevant e.g. when comparing against relay items
        # from a subprocess run of the same file).
        try:
            nodeid_file = path.relative_to(self.path).as_posix()
        except ValueError:
            nodeid_file = path.name
        module_name = path.stem
        spec = importlib.util.spec_from_file_location(module_name, path)
        if spec is None or spec.loader is None:
            return []
        # Apply assertion rewriting when enabled (spec_from_file_location
        # bypasses sys.meta_path, so we must apply the rewrite loader manually).
        try:
            from pytest._rewrite import _is_rewrite_target, _rewrite_enabled, _RewriteLoader

            if _rewrite_enabled and spec.origin and _is_rewrite_target(str(spec.origin)):
                spec.loader = _RewriteLoader(spec.name, str(spec.origin))
        except Exception:
            pass
        module = importlib.util.module_from_spec(spec)
        sys.modules[module_name] = module
        try:
            spec.loader.exec_module(module)
        except Exception:
            return []

        _mod_fdefs, _mod_autouse = _gather_fixturedefs(vars(module).items())

        # Eagerly scan classes for fixture signature issues (mirrors
        # pytest's parsefactories, which fires for every Class node).
        for _nm, _ob in vars(module).items():
            if isinstance(_ob, type) and not _nm.startswith("_"):
                _gather_fixturedefs(
                    [(_a, getattr(_ob, _a, None)) for _a in dir(_ob)],
                    owning_cls=_ob,
                )

        # Walk conftest.py files from the rootdir down to the source file's
        # directory (root-first), gathering their fixturedefs. Same-scope
        # autouse fixtures defined closer to the root must sort first, which
        # the stable scope-sort below preserves given root-to-leaf seed order.
        def _walk_conftest_fixturedefs(src_path):
            root = self.path.resolve()
            dirs, d = [], src_path.parent
            while True:
                dirs.append(d)
                if d == root or d.parent == d:
                    break
                d = d.parent
            dirs.reverse()  # root-first
            fdefs, autouse = {}, []
            for d in dirs:
                cf = d / "conftest.py"
                if not cf.is_file():
                    continue
                mod = Pytester._import_conftest_module(cf)
                if mod is None:
                    continue
                cdefs, cau = _gather_fixturedefs(vars(mod).items())
                fdefs.update(cdefs)
                autouse += [a for a in cau if a not in autouse]
            return fdefs, autouse

        _cf_fdefs, _cf_autouse = _walk_conftest_fixturedefs(path)
        # conftest fixtures come first (root-to-leaf); module fixtures override
        # same-named conftest fixtures and their autouse follows the conftests'.
        _mod_fdefs = {**_cf_fdefs, **_mod_fdefs}
        _mod_autouse = _cf_autouse + [a for a in _mod_autouse if a not in _cf_autouse]

        config = self._request.config if self._request is not None else None
        module_marks = get_unpacked_marks(module)
        session = _NodeSession(config)
        module_collector = _ModuleCollector(module, session, path)

        # Read python_classes / python_functions from the local ini file
        # near the source file (not the outer session config, which belongs to
        # the conformance suite itself and would override pytester.makeini()).
        def _read_local_ini_patterns(src_path):
            """Walk up from src_path looking for pytest.ini/setup.cfg/tox.ini
            and return (class_patterns, func_patterns) or (None, None)."""
            for d in [src_path.parent, *src_path.parent.parents]:
                for fname, section, cls_key, fn_key in [
                    ("pytest.ini", "pytest", "python_classes", "python_functions"),
                    ("setup.cfg", "tool:pytest", "python_classes", "python_functions"),
                    ("tox.ini", "pytest", "python_classes", "python_functions"),
                ]:
                    cfg_path = d / fname
                    if not cfg_path.is_file():
                        continue
                    cp = configparser.ConfigParser()
                    try:
                        cp.read(str(cfg_path), encoding="utf-8")
                    except Exception:
                        continue
                    if not cp.has_section(section):
                        continue
                    cls_val = cp.get(section, cls_key, fallback=None)
                    fn_val = cp.get(section, fn_key, fallback=None)
                    if cls_val is None and fn_val is None:
                        continue
                    cls_pats = cls_val.split() if cls_val else None
                    fn_pats = fn_val.split() if fn_val else None
                    return cls_pats, fn_pats
                # Stop at the pytester base dir — don't bleed into the outer tree
                if (d / "pyproject.toml").is_file():
                    # If pyproject exists but we didn't find ini patterns, stop
                    break
            return None, None

        _class_patterns, _func_patterns = _read_local_ini_patterns(path)

        def _is_test_func(name):
            if _func_patterns is None:
                return name.startswith("test")
            return any(name.startswith(p) or fnmatch.fnmatch(name, p) for p in _func_patterns)

        def _is_test_class(name):
            if _class_patterns is None:
                return name.startswith("Test")
            return any(name.startswith(p) or fnmatch.fnmatch(name, p) for p in _class_patterns)

        def make_item(func, nodeid_name, all_marks, cls=None, parent=None):
            lineno = getattr(getattr(func, "__code__", None), "co_firstlineno", 0)
            node = Function(
                f"{nodeid_file}::{nodeid_name}",
                nodeid_name.rsplit("::", 1)[-1],
                all_marks,
                [],
                func,
                path,
                lineno,
            )
            node.module = module
            node.cls = cls
            node.parent = parent
            node._module_collector = module_collector
            if config is not None:
                node.config = config
            try:
                requested = list(getfuncargnames(func, name=node.name, cls=cls))
            except (Exception, OutcomeException):
                # getfuncargnames raises pytest.fail() (OutcomeException, a
                # BaseException subclass, not Exception) for a method with
                # no usable signature (e.g. missing self) — degrade to no
                # requested args rather than letting it escape uncaught.
                requested = []
            node.fixturenames = _closure_for(requested, cls)
            # The full fixturedef map (conftest+module, plus this item's class
            # fixtures) so the item's request can resolve and execute fixtures
            # in-process (item.session._setupstate.setup → _fillfixtures).
            full = dict(_mod_fdefs)
            if cls is not None:
                cdefs, _ = _gather_fixturedefs(
                    [(n, getattr(cls, n, None)) for n in dir(cls)], owning_cls=cls
                )
                full.update(cdefs)
            node._fixturedefs_full = full
            # A single, stable session per item so its _setupstate persists
            # across item.session accesses (test_request_addfinalizer registers
            # finalizers on it in setup and drains them later).
            node._session_obj = session
            node.funcargs = {}
            from _pytest.fixtures import TopRequest

            node._request = TopRequest(node, _ispytest=True)
            return node

        # Module-level node for parent chain (getparent/keywords); use File so
        # getparent(pytest.Module) finds it (Module is aliased to File).
        mod_node = File(name=path.name, path=path, config=config, parent=parent_collector)
        mod_node.own_markers = list(module_marks)
        # getparent(pytest.Module).obj is the imported module (test_getmodulecollector).
        mod_node.obj = module

        from pytest._pycollect import IGNORED_ATTRIBUTES as _IGNORED

        # Collect pytest_pycollect_makeitem implementations from conftests
        # along the path to the source file.  These are gathered directly from
        # the imported conftest modules (not via pluginmanager) so that in-
        # process pytester collection honours them without registering plugins
        # as real session-wide side effects.
        def _gather_makeitem_impls(src_path):
            root = self.path.resolve()
            dirs, d = [], src_path.parent
            while True:
                dirs.append(d)
                if d == root or d.parent == d:
                    break
                d = d.parent
            dirs.reverse()
            impls = []
            for d in dirs:
                cf = d / "conftest.py"
                if not cf.is_file():
                    continue
                mod = Pytester._import_conftest_module(cf)
                if mod is None:
                    continue
                fn = getattr(mod, "pytest_pycollect_makeitem", None)
                if fn is not None:
                    impls.append(fn)
            return impls

        _makeitem_impls = _gather_makeitem_impls(path)

        def _gather_generate_tests_impls(src_path):
            """Conftest-declared pytest_generate_tests along the path, plus
            any registered plugin's own (e.g. pytest-order's, which splits a
            function with multiple stacked @pytest.mark.order(...) into
            parametrized items) — instance_hook_impls already excludes
            conftest/internal modules, so it's the third-party-plugin half."""
            root = self.path.resolve()
            dirs, d = [], src_path.parent
            while True:
                dirs.append(d)
                if d == root or d.parent == d:
                    break
                d = d.parent
            dirs.reverse()
            impls = []
            for d in dirs:
                cf = d / "conftest.py"
                if not cf.is_file():
                    continue
                mod = Pytester._import_conftest_module(cf)
                if mod is None:
                    continue
                fn = getattr(mod, "pytest_generate_tests", None)
                if fn is not None:
                    impls.append(fn)
            impls.extend(instance_hook_impls("pytest_generate_tests"))
            return impls

        _generate_tests_impls = _gather_generate_tests_impls(path)

        def _run_generate_tests(func, cls):
            """Fire pytest_generate_tests and return any marks a hookimpl
            added via metafunc.parametrize() — folded in by the caller
            alongside the function's own (possibly hook-mutated, e.g.
            pytest-order removes its own marks from func.pytestmark)
            decorator marks, mirroring the real engine's push_test_items.

            Returns (prepend_marks, append_marks): a trylast hookimpl (e.g.
            pytest-repeat's) keeps its marks innermost, appended after the
            decorator marks (previous always-append behavior). A hookimpl
            with no explicit priority or tryfirst (e.g. pytest-order's plain
            pytest_generate_tests) must come first/outermost instead, mirroring
            pluggy's LIFO ordering against the built-in decorator-processing
            hookimpl (itself a normal-tier hookimpl registered very early —
            a later-registered same-tier plugin hookimpl runs before it). See
            pytest_generate_tests_hook_priority_merge_order_gap in MCP memory."""
            if not _generate_tests_impls:
                return [], []
            try:
                requested = list(getfuncargnames(func, name=func.__name__, cls=cls))
            except (Exception, OutcomeException):
                # getfuncargnames raises pytest.fail() (OutcomeException,
                # a BaseException subclass, not Exception) for a method
                # with no usable signature (e.g. missing self) — degrade to
                # no requested args, matching make_item's own handling of
                # the same call below.
                requested = []
            closure = _closure_for(requested, cls)
            metafunc = Metafunc(func, closure, module, cls, config, list(get_unpacked_marks(func)))
            prepend_marks = []
            append_marks = []
            for impl in _generate_tests_impls:
                before = len(metafunc._parametrize_marks)
                try:
                    impl(metafunc)
                except Exception:
                    continue
                added = metafunc._parametrize_marks[before:]
                opts = getattr(impl, "pytest_impl", None) or {}
                if opts.get("trylast"):
                    append_marks.extend(added)
                else:
                    prepend_marks.extend(added)
            return prepend_marks, append_marks

        def _try_makeitem(name, obj, parent=mod_node):
            """Fire pytest_pycollect_makeitem; return custom node list or None."""
            if not _makeitem_impls:
                return None
            for impl in _makeitem_impls:
                try:
                    import inspect as _inspect

                    sig = _inspect.signature(impl)
                    kw = {}
                    if "collector" in sig.parameters:
                        kw["collector"] = parent
                    if "name" in sig.parameters:
                        kw["name"] = name
                    if "obj" in sig.parameters:
                        kw["obj"] = obj
                    result = impl(**kw)
                except Exception:
                    continue
                if result is None:
                    continue
                # firstresult: may return a single node or list of nodes
                nodes = (
                    list(result)
                    if hasattr(result, "__iter__") and not isinstance(result, type)
                    else [result]
                )
                valid = [n for n in nodes if n is not None]
                if valid:
                    return valid
            return None

        def _validate_parametrize_argnames(func, marks, cls=None):
            import inspect as _insp

            param_marks = [m for m in marks if m.name == "parametrize"]
            if not param_marks:
                return
            try:
                func_params = set(getfuncargnames(func, name=func.__name__, cls=cls))
            except (Exception, OutcomeException):
                func_params = set()
            try:
                all_params = set(_insp.signature(func).parameters.keys())
            except (ValueError, TypeError):
                all_params = set()
            func_params.add("request")
            fixture_closure = set(_closure_for(list(func_params), cls))
            avail = func_params | all_params | fixture_closure
            for pm in param_marks:
                argnames_raw = pm.args[0] if pm.args else pm.kwargs.get("argnames", "")
                if isinstance(argnames_raw, str):
                    argnames = [x.strip() for x in argnames_raw.split(",") if x.strip()]
                else:
                    argnames = list(argnames_raw)
                indirect = pm.kwargs.get("indirect", False)
                if isinstance(indirect, str):
                    indirect = [indirect]
                for arg in argnames:
                    if arg in avail:
                        continue
                    if isinstance(indirect, (list, tuple)):
                        kind = "fixture" if arg in indirect else "argument"
                    else:
                        kind = "fixture" if indirect else "argument"
                    fail(
                        f"In {func.__name__}: function uses no {kind} '{arg}'",
                        pytrace=False,
                    )

        items = []
        for name, obj in vars(module).items():
            if name in _IGNORED:
                continue
            if _is_test_func(name) and callable(obj) and not isinstance(obj, type):
                custom = _try_makeitem(name, obj)
                if custom is not None:
                    for custom_node in custom:
                        # Wire up the in-process attrs that make_item normally sets
                        custom_node.module = module
                        custom_node.parent = mod_node
                        custom_node._module_collector = module_collector
                        if config is not None:
                            custom_node.config = config
                        try:
                            requested = list(getfuncargnames(obj, name=custom_node.name, cls=None))
                        except Exception:
                            requested = []
                        custom_node.fixturenames = _closure_for(requested, None)
                        custom_node._fixturedefs_full = dict(_mod_fdefs)
                        custom_node._session_obj = session
                        custom_node.funcargs = {}
                        from _pytest.fixtures import TopRequest

                        custom_node._request = TopRequest(custom_node, _ispytest=True)
                        items.append(custom_node)
                    continue
                # Run pytest_generate_tests BEFORE reading marks: a hookimpl
                # (e.g. pytest-order's) may mutate obj.pytestmark in place
                # (removing marks it consumed), so get_unpacked_marks must
                # see the post-hook state. Its own extra_marks are excluded
                # from _validate_parametrize_argnames — Metafunc.parametrize()
                # already validated them against metafunc.fixturenames while
                # the hook ran, which is where a hook registers any synthetic
                # argname it introduces (e.g. pytest-order's own "order").
                prepend_marks, append_marks = _run_generate_tests(obj, None)
                decl_marks = get_unpacked_marks(obj)
                _validate_parametrize_argnames(obj, decl_marks)
                fn_marks = [*prepend_marks, *decl_marks, *append_marks]
                sub = Pytester._expand_params(
                    fn_marks,
                    module_marks,
                    name,
                    lambda nm, mks, _obj=obj: make_item(_obj, nm, mks, parent=mod_node),
                )
                items.extend(sub)
            elif _is_test_class(name) and isinstance(obj, type):
                class_marks = get_unpacked_marks(obj)
                cls_node = Class(name=name, parent=mod_node, obj=obj)
                cls_node.own_markers = list(class_marks)
                methods = []
                seen = set()
                for mname in dir(obj):
                    if mname in _IGNORED or not _is_test_func(mname) or mname in seen:
                        continue
                    seen.add(mname)
                    mobj = getattr(obj, mname, None)
                    if mobj is None or not callable(mobj):
                        continue
                    func = getattr(mobj, "__func__", mobj)
                    if callable(func):
                        lineno = getattr(getattr(func, "__code__", None), "co_firstlineno", 0)
                        methods.append((lineno, mname, func))
                for _ln, mname, func in sorted(methods):
                    prepend_marks, append_marks = _run_generate_tests(func, obj)
                    decl_marks = [*get_unpacked_marks(func), *class_marks]
                    _validate_parametrize_argnames(func, decl_marks, cls=obj)
                    fn_marks = [*prepend_marks, *decl_marks, *append_marks]
                    sub = Pytester._expand_params(
                        fn_marks,
                        module_marks,
                        f"{name}::{mname}",
                        lambda nm, mks, _f=func, _cls=obj, _cn=cls_node: make_item(
                            _f, nm, mks, cls=_cls, parent=_cn
                        ),
                    )
                    # item.instance is an instance of the test class (pytest
                    # instantiates per item); fall back to the class if it
                    # cannot be constructed without arguments.
                    try:
                        instance = obj()
                    except Exception:
                        instance = obj
                    for item in sub:
                        item.instance = instance
                    items.extend(sub)
        return items

    def _genitems_from_dir(self, directory, args):
        """In-process collection of every python_files-matching test module
        under `directory` (recursive). Returns (items, None). Used by
        inline_genitems() with no path args so callers get live Function nodes."""
        config = self._request.config if self._request is not None else None
        python_files = (
            config.getini("python_files")
            if config is not None and hasattr(config, "getini")
            else "test_*.py"
        )
        patterns = python_files.split() if isinstance(python_files, str) else list(python_files)
        directory = pathlib.Path(str(directory))
        roots = self._testpaths_roots(directory)
        seen_files: set = set()
        items = []
        for root in roots:
            for pat in patterns:
                for py_file in sorted(root.rglob(pat)):
                    if not py_file.is_file() or py_file in seen_files:
                        continue
                    if any(part.startswith((".", "__pycache__")) for part in py_file.parts):
                        continue
                    seen_files.add(py_file)
                    items.extend(self._collect_items_from_path(py_file))
        return items, None

    def _testpaths_roots(self, directory):
        """Resolve testpaths ini (with glob support) to a list of collection
        root directories. Falls back to [directory] when unset. Testpaths
        only applies when the invocation dir matches rootdir (pytest rule)."""
        cwd = pathlib.Path.cwd()
        if cwd != directory:
            return [cwd]
        inner_config = self._parse_inner_ini(directory)
        testpaths = inner_config.get("testpaths", "").strip()
        if not testpaths:
            return [directory]
        roots = []
        for entry in testpaths.split():
            if any(c in entry for c in "*?["):
                roots.extend(sorted(directory.glob(entry)))
            else:
                candidate = directory / entry
                if candidate.exists():
                    roots.append(candidate)
        return roots or [directory]

    def _parse_inner_ini(self, directory):
        """Read pytest ini settings from the pytester directory's config file."""
        for name in ("pytest.ini", "pyproject.toml", "tox.ini", "setup.cfg"):
            path = directory / name
            if not path.is_file():
                continue
            if name == "pyproject.toml":
                try:
                    import tomllib
                except ImportError:
                    import tomli as tomllib
                with open(path, "rb") as f:
                    data = tomllib.load(f)
                return data.get("tool", {}).get("pytest", {}).get("ini_options", {})
            cp = configparser.ConfigParser()
            cp.read(str(path), encoding="utf-8")
            for section in ("pytest", "tool:pytest"):
                if cp.has_section(section):
                    return dict(cp.items(section))
        return {}

    def getitems(self, source):
        """Collect Function item nodes from the source in-process (a light
        collection: module import + test functions/Test-class methods with
        merged marks — enough for the mark-evaluation tests; no fixtures).

        `source` may be a Path to an already-present file (e.g. after
        copy_example()), in which case it is collected directly rather than
        written out as source text (upstream getmodulecol special-cases Path)."""
        if isinstance(source, pathlib.Path):
            path = source if source.is_absolute() else (self.path / source)
        else:
            path = pathlib.Path(str(self.makepyfile(source)))
        return self._collect_items_from_path(path)

    def getitem(self, source, funcname="test_func"):
        """The single collected item named funcname (upstream getitem)."""
        for item in self.getitems(source):
            if item.name == funcname:
                return item
        fail(f"{funcname!r} item not found in module:\n{source}")

    def runitem(self, source, funcname="test_func"):
        """Run the single item named funcname in-process and return its
        [setup, call, teardown] reports (upstream Pytester.runitem)."""
        from _pytest.runner import runtestprotocol

        return runtestprotocol(self.getitem(source, funcname), log=False)

    def getmodulecol(self, source, *, configargs=(), withinit=False):
        """An in-process Module collector for the source. Supports .collect()
        (returns Class + Function children) and .module/.cls/.instance attrs."""
        from pytest._marks import get_unpacked_marks
        from pytest._node import (
            Class,
            Collector,
            File,
            Function,
            Session,
            _ModuleCollector,
            _NodeSession,
        )

        if isinstance(source, os.PathLike):
            # Already a written file (e.g. the Path makepyfile() returned):
            # use it directly, matching upstream — calling makepyfile again
            # would treat the path string itself as source code.
            path = self.path.joinpath(source)
        else:
            if withinit:
                (self.path / "__init__.py").touch()
            path = pathlib.Path(str(self.makepyfile(source)))
        config = self._request.config if self._request is not None else None

        # Import the module in-process. A failed import (ImportError, syntax
        # error, …) is captured so the live node's collect()/obj surface it the
        # way pytest does (Collector.CollectError / the original exception).
        module_name = path.stem
        import_error: BaseException | None = None
        spec = importlib.util.spec_from_file_location(module_name, path)
        if spec is not None and spec.loader is not None:
            mod = importlib.util.module_from_spec(spec)
            sys.modules[module_name] = mod
            try:
                spec.loader.exec_module(mod)
            except BaseException as exc:  # noqa: BLE001 - re-surfaced on access
                mod = None
                import_error = exc
        else:
            mod = None

        session = _NodeSession(config)
        module_collector = _ModuleCollector(mod, session, path) if mod is not None else None
        module_marks = get_unpacked_marks(mod) if mod is not None else []
        # Identity cache for direct-parametrize pseudo FixtureDefs at module
        # scope (see _expand_params) — shared by every function/class built
        # below, mirroring upstream's per-Module `name2pseudofixturedef` stash.
        _module_pseudofixturedefs = {}
        # A real Session backs modcol.session.perform_collect (test_modulecol_roundtrip).
        # Resolve collection args relative to the module's own directory, not the
        # outer invocation rootdir (the pytester file lives in a tmp dir).
        real_session = Session.from_config(config) if config is not None else None
        if real_session is not None:
            real_session.path = path.parent

        class _IPModule(File):
            """In-process Module collector returned by getmodulecol."""

            def __init__(self):
                super().__init__(name=path.name, config=config, path=path, nodeid=path.name)
                self.module = mod
                self.cls = None
                self.instance = None
                self.session = real_session
                self._children = None

            @property
            def obj(self):
                # Accessing .obj surfaces an import failure (test_failing_import)
                # and validates module-level pytest_plugins, raising ImportError
                # for a missing plugin (test_module_considers_pluginmanager_at_import).
                if import_error is not None:
                    raise import_error
                plugins = getattr(mod, "pytest_plugins", None)
                if plugins is not None:
                    names = [plugins] if isinstance(plugins, str) else list(plugins)
                    for name in names:
                        importlib.import_module(name)
                return mod

            def collect(self):
                if self._children is not None:
                    return list(self._children)
                if mod is None:
                    if import_error is not None:
                        raise Collector.CollectError(str(import_error)) from import_error
                    self._children = []
                    return []
                children = []
                for name, obj in vars(mod).items():
                    if name.startswith("test") and callable(obj) and not isinstance(obj, type):

                        def _mk(nm, mks, _obj=obj):
                            lineno = getattr(getattr(_obj, "__code__", None), "co_firstlineno", 0)
                            fn = Function(
                                f"{path.name}::{nm}",
                                nm.rsplit("::", 1)[-1],
                                mks,
                                [],
                                _obj,
                                str(path),
                                lineno,
                            )
                            fn.module = mod
                            fn.cls = None
                            fn.parent = self
                            if module_collector is not None:
                                fn._module_collector = module_collector
                            if config is not None:
                                fn.config = config
                            return fn

                        children.extend(
                            Pytester._expand_params(
                                get_unpacked_marks(obj),
                                module_marks,
                                name,
                                _mk,
                                # No real Class here: "class" scope falls back
                                # to module scope, same as upstream.
                                scope_caches={
                                    "module": _module_pseudofixturedefs,
                                    "class": _module_pseudofixturedefs,
                                },
                            )
                        )
                    elif name.startswith("Test") and isinstance(obj, type):
                        cls_node = _IPClass(name, obj, self)
                        children.append(cls_node)
                self._children = children
                return list(children)

        class _IPClass(Class):
            """In-process Class collector returned by collect_by_name on a Module."""

            def __init__(self, name, cls_obj, parent_module):
                super().__init__(
                    name=name, config=config, path=path, nodeid=f"{path.name}::{name}", obj=cls_obj
                )
                self.parent = parent_module
                self._cls_obj = cls_obj
                self.module = mod
                self.cls = cls_obj
                self.instance = None
                self._children = None
                # This real Class is its own scope node for "class"-scoped
                # direct parametrize; "module" still shares the outer dict.
                self._pseudofixturedefs = {}

            def collect(self):
                if self._children is not None:
                    return list(self._children)
                children = []
                class_marks = get_unpacked_marks(self._cls_obj)
                seen = set()
                methods = []
                for mname in dir(self._cls_obj):
                    if not mname.startswith("test") or mname in seen:
                        continue
                    seen.add(mname)
                    mobj = getattr(self._cls_obj, mname, None)
                    if mobj is None or not callable(mobj):
                        continue
                    func = getattr(mobj, "__func__", mobj)
                    if callable(func):
                        lineno = getattr(getattr(func, "__code__", None), "co_firstlineno", 0)
                        methods.append((lineno, mname, func))
                for _ln, mname, func in sorted(methods):

                    def _mk(nm, mks, _f=func):
                        lineno = getattr(getattr(_f, "__code__", None), "co_firstlineno", 0)
                        fn = Function(
                            f"{path.name}::{self.name}::{nm}",
                            nm.rsplit("::", 1)[-1],
                            mks,
                            [],
                            _f,
                            str(path),
                            lineno,
                        )
                        fn.module = mod
                        fn.cls = self._cls_obj
                        fn.instance = self._cls_obj
                        fn.parent = self
                        if module_collector is not None:
                            fn._module_collector = module_collector
                        if config is not None:
                            fn.config = config
                        from _pytest.compat import getfuncargnames
                        from _pytest.fixtures import TopRequest

                        try:
                            fn.fixturenames = list(
                                getfuncargnames(_f, name=fn.name, cls=self._cls_obj)
                            )
                        except Exception:
                            fn.fixturenames = []
                        fn._request = TopRequest(fn, _ispytest=True)
                        return fn

                    children.extend(
                        Pytester._expand_params(
                            [*get_unpacked_marks(func), *class_marks],
                            module_marks,
                            mname,
                            _mk,
                            scope_caches={
                                "module": _module_pseudofixturedefs,
                                "class": self._pseudofixturedefs,
                            },
                        )
                    )
                self._children = children
                return list(children)

        return _IPModule()

    def genitems(self, colitems):
        """Recursively collect the Item nodes from the given collectors
        (upstream Pytester.genitems): walk each collector's .collect() until a
        Function (leaf Item) is reached."""
        from pytest._node import Function

        items = []

        def _rec(col):
            if isinstance(col, Function):
                items.append(col)
                return
            collect = getattr(col, "collect", None)
            if collect is None:
                items.append(col)
                return
            for child in collect():
                _rec(child)

        for col in colitems:
            _rec(col)
        return items

    def collect_by_name(self, modcol, name):
        """Return the first child of modcol whose .name == name, or None.
        Caches the result of modcol.collect() across calls (upstream behaviour)."""
        if not hasattr(self, "_mod_collections"):
            self._mod_collections = {}
        if modcol not in self._mod_collections:
            self._mod_collections[modcol] = list(modcol.collect())
        for colitem in self._mod_collections[modcol]:
            if colitem.name == name:
                return colitem
        return None

    def getnode(self, config, arg):
        """Return the collector/item for `arg` under a fresh Session built from `config`."""
        from pytest._node import Session

        session = Session.from_config(config)
        p = pathlib.Path(os.path.abspath(str(arg)))
        results = session.perform_collect([str(p)], genitems=False)
        return results[0] if results else None

    def getpathnode(self, path):
        """Return the collector/item for `path` (parses config from path)."""
        config = self.parseconfigure(path)
        return self.getnode(config, path)

    def mkpydir(self, name):
        path = self.path / name
        path.mkdir(parents=True)
        (path / "__init__.py").touch()
        return path

    def copy_example(self, name=None):
        """Copy a file or directory from the suite's example_scripts tree
        into the pytester dir. The example dir is found by walking up from
        the requesting test's file (we don't see the suite's
        `pytester_example_dir` ini; pytest's layout keeps examples next to
        the tests)."""
        function = getattr(self._request.node, "function", None) if self._request else None
        if function is None:
            fail("copy_example: originating test function is unknown")
        here = pathlib.Path(function.__code__.co_filename).resolve().parent
        example_dir = next(
            (
                base / "example_scripts"
                for base in (here, *here.parents)
                if (base / "example_scripts").is_dir()
            ),
            None,
        )
        if example_dir is None:
            fail(f"copy_example: no example_scripts directory above {here}")
        for mark in self._request.node.iter_markers("pytester_example_path"):
            example_dir = example_dir.joinpath(*mark.args)

        if name is None:
            maybe_dir = example_dir / self._name
            maybe_file = example_dir / (self._name + ".py")
            if maybe_dir.is_dir():
                example_path = maybe_dir
            elif maybe_file.is_file():
                example_path = maybe_file
            else:
                raise LookupError(
                    f"{self._name} can't be found as module or package in {example_dir}"
                )
        else:
            example_path = example_dir.joinpath(name)

        if example_path.is_dir() and not (example_path / "__init__.py").is_file():
            shutil.copytree(example_path, self.path, dirs_exist_ok=True)
            return self.path
        if example_path.is_file():
            result = self.path / example_path.name
            shutil.copy(example_path, result)
            return result
        raise LookupError(f'example "{example_path}" is not found as a file or directory')

    @staticmethod
    def _python_env():
        """os.environ with the pytest/_pytest shim importable, matching a
        real pytest install where the child just imports site-packages."""
        env = os.environ.copy()
        shim_root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
        existing = env.get("PYTHONPATH")
        env["PYTHONPATH"] = os.pathsep.join([shim_root, *([existing] if existing else [])])
        return env

    def runpython(self, script):
        start = time.perf_counter()
        proc = subprocess.run(
            [sys.executable, str(script)],
            cwd=self.path,
            capture_output=True,
            text=True,
            timeout=120,
            env=self._python_env(),
        )
        duration = time.perf_counter() - start
        return RunResult(
            proc.returncode,
            proc.stdout.splitlines(),
            proc.stderr.splitlines(),
            duration,
        )

    def runpython_c(self, command):
        start = time.perf_counter()
        proc = subprocess.run(
            [sys.executable, "-c", command],
            cwd=self.path,
            capture_output=True,
            text=True,
            timeout=120,
            env=self._python_env(),
        )
        duration = time.perf_counter() - start
        return RunResult(
            proc.returncode,
            proc.stdout.splitlines(),
            proc.stderr.splitlines(),
            duration,
        )

    def spawn_pytest(self, string, expect_timeout=10.0):
        basetemp = self.path / "temp-pexpect"
        basetemp.mkdir(mode=0o700, exist_ok=True)
        exe = os.environ.get("PYTEST_RS_EXE") or _RUNNER_EXE
        if exe is None:
            fail("PYTEST_RS_EXE is not set; pytester cannot spawn the runner")
        cmd = f"{exe} --basetemp={basetemp} {string}"
        return self.spawn(cmd, expect_timeout=expect_timeout)

    def spawn(self, cmd, expect_timeout=10.0):
        import platform as _platform

        pexpect = importorskip("pexpect", "3.0")
        if hasattr(sys, "pypy_version_info") and "64" in _platform.machine():
            skip("pypy-64 bit not supported")
        if not hasattr(pexpect, "spawn"):
            skip("pexpect.spawn not available")
        logfile = self.path.joinpath("spawn.out").open("wb")
        env = os.environ.copy()
        for _var, _value in _RUNNER_LIBPATH.items():
            env.setdefault(_var, _value)
        if _RUNNER_PYTHONPATH:
            env.setdefault("PYTHONPATH", _RUNNER_PYTHONPATH)
        env["TERM"] = "dumb"
        env.pop("NO_COLOR", None)
        child = pexpect.spawn(
            cmd,
            logfile=logfile,
            timeout=expect_timeout,
            cwd=str(self.path),
            env=env,
        )
        self._request.addfinalizer(logfile.close)
        return child


class Testdir(Pytester):
    """Legacy pytester alias (the pre-7.0 testdir fixture API): paths are
    py.path-like LocalPath objects instead of pathlib.Path."""

    @property
    def monkeypatch(self):
        return self._monkeypatch

    @property
    def tmpdir(self):
        from pytest._tmp_path import LocalPath

        return LocalPath(self.path)

    def _makefile(self, ext, args, kwargs):
        from pytest._tmp_path import LocalPath

        result = super()._makefile(ext, args, kwargs)
        if isinstance(result, list):
            return [LocalPath(path) for path in result]
        return LocalPath(result)

    def mkdir(self, name):
        from pytest._tmp_path import LocalPath

        return LocalPath(super().mkdir(name))


def _make_runner_dir(request, tmp_path_factory, cls, monkeypatch=None):
    # Numbered dirs named after the test, under the session basetemp shared
    # with tmp_path/tmpdir — upstream pytester layout (relative nodeids of
    # nested runs can include this dir name when rootdir lands on basetemp).
    # Upstream pytester names dirs after the bare function name (params and
    # truncation are tmp_path behaviors, not pytester's).
    name = request.node.name.split("[")[0]
    path = tmp_path_factory.mktemp(name, numbered=True)
    old_cwd = os.getcwd()
    os.chdir(path)
    runner = cls(path, name, request)
    runner._monkeypatch = monkeypatch
    # Upstream: nested runs root their tmp dirs under a per-pytester
    # directory (tests inspect it via pytester._test_tmproot).
    runner._test_tmproot = tmp_path_factory.mktemp(f"tmp-{name}", numbered=True)
    old_temproot = os.environ.get("PYTEST_DEBUG_TEMPROOT")
    os.environ["PYTEST_DEBUG_TEMPROOT"] = str(runner._test_tmproot)
    # Upstream pytester sanitizes the outer PYTEST_ADDOPTS at setup; a test
    # monkeypatch.setenv afterwards still reaches the nested run.
    old_addopts = os.environ.pop("PYTEST_ADDOPTS", None)
    # Upstream pytester sets HOME to the pytester temp directory so nested
    # runs don't pick up the real user's home config.
    tmphome = str(path)
    old_home = os.environ.get("HOME")
    old_userprofile = os.environ.get("USERPROFILE")
    os.environ["HOME"] = tmphome
    os.environ["USERPROFILE"] = tmphome
    yield runner
    if old_temproot is None:
        os.environ.pop("PYTEST_DEBUG_TEMPROOT", None)
    else:
        os.environ["PYTEST_DEBUG_TEMPROOT"] = old_temproot
    if old_addopts is not None:
        os.environ["PYTEST_ADDOPTS"] = old_addopts
    if old_home is None:
        os.environ.pop("HOME", None)
    else:
        os.environ["HOME"] = old_home
    if old_userprofile is None:
        os.environ.pop("USERPROFILE", None)
    else:
        os.environ["USERPROFILE"] = old_userprofile
    for entry in runner._syspaths:
        if entry in sys.path:
            sys.path.remove(entry)
    os.chdir(old_cwd)


class LineComp:
    """A StringIO plus assert_contains_lines, for driving an in-process
    TerminalReporter in tests (upstream's `linecomp` fixture)."""

    def __init__(self):
        self.stringio = io.StringIO()

    def assert_contains_lines(self, lines2):
        __tracebackhide__ = True
        val = self.stringio.getvalue()
        self.stringio.truncate(0)
        self.stringio.seek(0)
        LineMatcher(val.split("\n")).fnmatch_lines(lines2)


@fixture
def linecomp():
    return LineComp()


@fixture
def pytester(request, tmp_path_factory, monkeypatch):
    yield from _make_runner_dir(request, tmp_path_factory, Pytester, monkeypatch)


@fixture
def testdir(request, tmp_path_factory, monkeypatch):
    yield from _make_runner_dir(request, tmp_path_factory, Testdir, monkeypatch)


@fixture
def _sys_snapshot():
    from _pytest.pytester import SysModulesSnapshot, SysPathsSnapshot

    snappaths = SysPathsSnapshot()
    snapmods = SysModulesSnapshot()
    yield
    snapmods.restore()
    snappaths.restore()


@fixture
def _config_for_test():
    from _pytest.config import _native_prepareconfig

    config = _native_prepareconfig([])
    yield config
    config._ensure_unconfigure()
