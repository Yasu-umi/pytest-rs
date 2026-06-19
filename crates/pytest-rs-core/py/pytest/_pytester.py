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

_OUTCOME_RE = re.compile(
    r"(\d+) (passed|failed|skipped|xfailed|xpassed|errors?|warnings?|deselected|rerun)"
)
_ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")


class RunResult:
    def __init__(self, ret, outlines, errlines, duration):
        self.ret = ret
        self.outlines = outlines
        self.errlines = errlines
        self.duration = duration
        self.stdout = LineMatcher(outlines)
        self.stderr = LineMatcher(errlines)

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
            if clean.startswith("====") and " in " in clean:
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
            path.write_text(textwrap.dedent(self._source_text(source)).lstrip("\n"))
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

    def runpytest(self, *args, timeout=None, syspathinsert=False, no_reraise_ctrlc=False):
        # syspathinsert=True: insert self.path into sys.path so the child can
        # import test-local plugins written to self.path.
        if syspathinsert:
            self.syspathinsert()
        # Upstream's default is in-process (shares module state); we default to
        # subprocess but switch to in-process when the env var is set — this is
        # needed for tests that monkeypatch module-level state and check it
        # after the inner run (e.g. subtests pdb tests).
        _check_cfg_pytest_section(self.path, args)
        if os.environ.get("PYTEST_RS_INLINE_INPROCESS"):
            reprec = self._inline_run_inprocess(*args)
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
        existing = env.get("PYTHONPATH") or _RUNNER_PYTHONPATH
        if self._syspaths or existing:
            entries = [*self._syspaths, *([existing] if existing else [])]
            env["PYTHONPATH"] = os.pathsep.join(entries)
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
        return self._runpytest(args, timeout=timeout, forward_filters=False)

    def runpytest_inprocess(self, *args, timeout=None):
        """Run pytest in-process (shares sys state with the outer test).

        When PYTEST_RS_INLINE_INPROCESS is set (as in the conformance suite),
        this uses the native in-process backend; otherwise falls back to the
        subprocess backend like runpytest().
        """
        if os.environ.get("PYTEST_RS_INLINE_INPROCESS"):
            reprec = self.inline_run(*args)
            result = getattr(reprec, "_result", None)
            if result is not None:
                result.reprec = reprec
                return result
            return reprec
        return self.runpytest(*args, timeout=timeout)

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
        if self._syspaths or existing:
            env["PYTHONPATH"] = os.pathsep.join(
                [*self._syspaths, *([existing] if existing else [])]
            )
        return env

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

        new_args = [str(arg) for arg in args]
        config = _native_prepareconfig(new_args)
        self._fire_addoption(config, new_args)
        _validate_required_plugins(config)
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
        try:
            spec = importlib.util.spec_from_file_location("_pytester_parseconfig_conftest", path)
            mod = importlib.util.module_from_spec(spec)
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

    def inline_run(self, *args):
        # Two backends: a subprocess run parsed into a HookRecorder-shaped
        # result (the default, robust for the whole suite) and an in-process
        # nested run that captures live hook-call objects via a real
        # HookRecorder. The in-process path is still being hardened (hook
        # instrumentation + session-global save/restore), so it is opt-in
        # behind PYTEST_RS_INLINE_INPROCESS until it is net-positive.
        if os.environ.get("PYTEST_RS_INLINE_INPROCESS"):
            return self._inline_run_inprocess(*args)
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

    def _inline_run_inprocess(self, *args):
        # In-process nested run: the native engine runs a whole session in
        # this process and fires its hooks through the monitored plugin
        # manager, so the HookRecorder captures live call objects (getcalls,
        # getreports, assertoutcome) — including custom hooks a subprocess
        # JSON relay could never carry.
        from _pytest.pytester import HookRecorder

        import pytest
        import pytest._capture as _capture
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

        reprec = HookRecorder(pluginmanager)
        # The inner run gets a fresh global capture state so its per-item
        # capture bookkeeping does not corrupt the outer session's.
        old_capstate = _capture.state
        inner_capstate = _capture.CaptureState()
        _capture.state = inner_capstate
        # Warning capture: the inner run_session calls _wcapture.uninstall()
        # on exit, breaking the outer capture. Save the full warning state
        # and reinstall afterwards so the outer run's capture survives.
        old_wcapture_captured = list(_wcapture.captured)
        old_wcapture_current_test = _wcapture.current_test
        old_wcapture_original_sw = _wcapture._original_showwarning
        old_wcapture_session_specs = list(_wcapture.session_specs)
        old_warn_filters = list(warnings.filters)
        _wcapture.captured.clear()

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
            sys.stdout.flush()
            sys.stderr.flush()
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
            reprec.finish_recording()
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
            _wcapture._original_showwarning = old_wcapture_original_sw
            _wcapture.session_specs[:] = old_wcapture_session_specs
            warnings.filters[:] = old_warn_filters
            warnings.showwarning = _wcapture._showwarning
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
        if not path_args:
            # Use relay items as the authoritative source
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
    def _expand_params(source_marks, module_marks, base_name, make_fn):
        """Expand the parametrize marks in source_marks into one node per
        combination, building each via make_fn(param_name, all_marks).

        source_marks are the marks that may carry parametrize (the function's
        own marks followed by any class marks); module_marks are folded into
        every node's mark list. pytest.param(..., marks=...) contributes its
        per-param marks only to the combinations that use it, and pytest.param
        ids / the ``ids=`` kwarg override the value-derived fragment."""
        from pytest._marks import ParamSpec, get_unpacked_marks  # noqa: F401

        param_marks = [m for m in source_marks if m.name == "parametrize"]
        base_marks = [m for m in source_marks if m.name != "parametrize"]
        base_marks = [*base_marks, *module_marks]

        if not param_marks:
            return [make_fn(base_name, base_marks)]

        levels = []  # each level: list of (id_fragment, [per_param_marks])
        for pm in param_marks:
            argvalues = list(pm.args[1]) if len(pm.args) > 1 else []
            ids_kwarg = pm.kwargs.get("ids", None)
            level = []
            for i, val in enumerate(argvalues):
                if isinstance(val, ParamSpec):
                    pvalues, pmarks, pid = val.values, list(val.marks), val.id
                else:
                    pvalues, pmarks, pid = (val,), [], None
                if pid is not None and isinstance(pid, str):
                    frag = pid
                elif ids_kwarg is not None and i < len(ids_kwarg) and ids_kwarg[i] is not None:
                    frag = str(ids_kwarg[i])
                else:
                    frag = "-".join(Pytester._param_id(v) for v in pvalues)
                level.append((frag, pmarks))
            if level:
                levels.append(level)

        if not levels:
            return [make_fn(base_name, base_marks)]

        items = []
        for combo in itertools.product(*levels):
            suffix = "-".join(frag for frag, _ in combo)
            combo_marks = [mk for _, marks in combo for mk in marks]
            items.append(make_fn(f"{base_name}[{suffix}]", [*base_marks, *combo_marks]))
        return items

    def _collect_items_from_path(self, path, parent_collector=None):
        """In-process collection of Function items from an existing .py file.

        Returns items with full mark data (own_markers, get_closest_marker,
        keywords) — the same objects getitems() returns, but without needing
        source text."""
        from pytest._marks import get_unpacked_marks
        from pytest._node import Class, File, Function, _ModuleCollector, _NodeSession

        path = pathlib.Path(str(path))
        if not path.is_absolute():
            path = self.path / path
        path = path.resolve()
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
                f"{path.name}::{nodeid_name}",
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
            return node

        # Module-level node for parent chain (getparent/keywords); use File so
        # getparent(pytest.Module) finds it (Module is aliased to File).
        mod_node = File(name=path.name, path=path, config=config, parent=parent_collector)
        mod_node.own_markers = list(module_marks)
        # getparent(pytest.Module).obj is the imported module (test_getmodulecollector).
        mod_node.obj = module

        items = []
        for name, obj in vars(module).items():
            if _is_test_func(name) and callable(obj) and not isinstance(obj, type):
                sub = Pytester._expand_params(
                    get_unpacked_marks(obj),
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
                    if not _is_test_func(mname) or mname in seen:
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
                    sub = Pytester._expand_params(
                        [*get_unpacked_marks(func), *class_marks],
                        module_marks,
                        f"{name}::{mname}",
                        lambda nm, mks, _f=func, _cls=obj, _cn=cls_node: make_item(
                            _f, nm, mks, cls=_cls, parent=_cn
                        ),
                    )
                    for item in sub:
                        item.instance = obj
                    items.extend(sub)
        return items

    def getitems(self, source):
        """Collect Function item nodes from the source in-process (a light
        collection: module import + test functions/Test-class methods with
        merged marks — enough for the mark-evaluation tests; no fixtures)."""
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
                                get_unpacked_marks(obj), module_marks, name, _mk
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
                        return fn

                    children.extend(
                        Pytester._expand_params(
                            [*get_unpacked_marks(func), *class_marks], module_marks, mname, _mk
                        )
                    )
                self._children = children
                return list(children)

        return _IPModule()

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
