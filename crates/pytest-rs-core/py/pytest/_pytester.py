"""pytester: run pytest-rs as a child process so upstream test suites can
exercise the runner itself."""

import os as _os
import re as _re
import subprocess as _subprocess

from pytest._fixtures import fixture
from pytest._outcomes import fail

# Captured before any test mutates os.environ: tests sometimes
# mock.patch.dict(os.environ, ..., clear=True) around runpytest(), which would
# otherwise strip the runner path and the import path the subprocess pytester
# needs (in-process pytester upstream shares sys.modules/sys.path, so a cleared
# env still finds installed plugins; we approximate that by remembering both).
_RUNNER_EXE = _os.environ.get("PYTEST_RS_EXE")
_RUNNER_PYTHONPATH = _os.environ.get("PYTHONPATH")
# The engine binary is dynamically linked against libpython; the loader path
# that lets it resolve libpython at runtime (LD_LIBRARY_PATH on linux, the
# DYLD_* vars on macOS) must also survive a clear=True so the nested run can
# even start — on linux a cleared LD_LIBRARY_PATH makes it fail to load.
_LIBPATH_VARS = ("LD_LIBRARY_PATH", "DYLD_LIBRARY_PATH", "DYLD_FALLBACK_LIBRARY_PATH")
_RUNNER_LIBPATH = {v: _os.environ[v] for v in _LIBPATH_VARS if v in _os.environ}

_OUTCOME_RE = _re.compile(
    r"(\d+) (passed|failed|skipped|xfailed|xpassed|errors?|warnings?|deselected|rerun)"
)
_ANSI_RE = _re.compile(r"\x1b\[[0-9;]*m")


def _check_cfg_pytest_section(path, args) -> None:
    """Mimic upstream in-process behaviour: raise pytest.fail.Exception if any
    .cfg config file (auto-discovered or via -c/--config-file) contains a bare
    [pytest] section (which is no longer supported; users must use [tool:pytest])."""
    import configparser
    import pytest

    CFG_MSG = "[pytest] section in {filename} files is no longer supported, change to [tool:pytest] instead."

    def _has_pytest_section(cfg_path) -> bool:
        cp = configparser.ConfigParser()
        try:
            cp.read(str(cfg_path))
        except Exception:
            return False
        return "pytest" in cp and "tool:pytest" not in cp

    # Check explicit -c / --config-file argument first.
    explicit_cfg = None
    args_list = [str(a) for a in args]
    for i, arg in enumerate(args_list):
        if arg in ("-c", "--config-file") and i + 1 < len(args_list):
            explicit_cfg = path / args_list[i + 1]
            break
        if arg.startswith(("-c", "--config-file=")):
            val = arg.split("=", 1)[-1] if "=" in arg else arg[2:]
            if val:
                explicit_cfg = path / val
                break

    if explicit_cfg is not None:
        if explicit_cfg.suffix == ".cfg" and _has_pytest_section(explicit_cfg):
            fail(CFG_MSG.format(filename=explicit_cfg.name), pytrace=False)
        return  # explicit config file given — no auto-discovery

    # Auto-discovery: scan for .cfg files with [pytest] section.
    for cfg_file in path.glob("*.cfg"):
        if _has_pytest_section(cfg_file):
            fail(CFG_MSG.format(filename=cfg_file.name), pytrace=False)


def _validate_required_plugins(config) -> None:
    """Check required_plugins ini; raise UsageError if any are missing or version-mismatched."""
    import importlib.metadata

    try:
        required = config.getini("required_plugins")
    except Exception:
        return
    if not required:
        return

    try:
        from packaging.requirements import Requirement, InvalidRequirement
        from packaging.version import Version
    except ImportError:
        return

    import pytest

    dist_versions: dict = {}
    for dist in importlib.metadata.distributions():
        try:
            name = dist.metadata.get("name") or dist.metadata["name"]
            version = dist.version
            if name:
                dist_versions[name.lower()] = version
        except Exception:
            continue

    missing = []
    for req_str in required:
        try:
            req = Requirement(req_str)
        except InvalidRequirement:
            missing.append(req_str)
            continue
        name = req.name.lower()
        if name not in dist_versions:
            missing.append(req_str)
        elif req.specifier and not req.specifier.contains(
            Version(dist_versions[name]), prereleases=True
        ):
            missing.append(req_str)

    if missing:
        raise pytest.UsageError(
            "Missing required plugins: {}".format(", ".join(missing))
        )


class LineMatcher:
    """Flexible matching of text (port of upstream pytester.LineMatcher).

    Built from a list of lines without trailing newlines; the various
    matchers log their match/no-match trail so failures show exactly which
    pattern stopped matching and against which lines.
    """

    def __init__(self, lines):
        self.lines = lines
        self._log_output = []

    def __str__(self):
        return "\n".join(self.lines)

    def str(self):
        return str(self)

    def _getlines(self, lines2):
        from _pytest._code import Source

        if isinstance(lines2, str):
            lines2 = Source(lines2)
        if isinstance(lines2, Source):
            lines2 = lines2.strip().lines
        return lines2

    def fnmatch_lines_random(self, lines2):
        __tracebackhide__ = True
        import fnmatch

        self._match_lines_random(lines2, fnmatch.fnmatch)

    def re_match_lines_random(self, lines2):
        __tracebackhide__ = True
        import re

        self._match_lines_random(lines2, lambda name, pat: bool(re.match(pat, name)))

    def _match_lines_random(self, lines2, match_func):
        __tracebackhide__ = True
        lines2 = self._getlines(lines2)
        for line in lines2:
            for x in self.lines:
                if line == x or match_func(x, line):
                    self._log("matched: ", repr(line))
                    break
            else:
                msg = f"line {line!r} not found in output"
                self._log(msg)
                self._fail(msg)

    def get_lines_after(self, fnline):
        import fnmatch

        for i, line in enumerate(self.lines):
            if fnline == line or fnmatch.fnmatch(line, fnline):
                return self.lines[i + 1 :]
        raise ValueError(f"line {fnline!r} not found in output")

    def _log(self, *args):
        self._log_output.append(" ".join(str(x) for x in args))

    @property
    def _log_text(self):
        return "\n".join(self._log_output)

    def fnmatch_lines(self, lines2, *, consecutive=False):
        __tracebackhide__ = True
        import fnmatch

        self._match_lines(lines2, fnmatch.fnmatch, "fnmatch", consecutive=consecutive)

    def re_match_lines(self, lines2, *, consecutive=False):
        __tracebackhide__ = True
        import re

        self._match_lines(
            lines2,
            lambda name, pat: bool(re.match(pat, name)),
            "re.match",
            consecutive=consecutive,
        )

    def _match_lines(self, lines2, match_func, match_nickname, *, consecutive=False):
        import collections.abc

        if not isinstance(lines2, collections.abc.Sequence):
            raise TypeError(f"invalid type for lines2: {type(lines2).__name__}")
        lines2 = self._getlines(lines2)
        lines1 = self.lines[:]
        extralines = []
        __tracebackhide__ = True
        wnick = len(match_nickname) + 1
        started = False
        for line in lines2:
            nomatchprinted = False
            while lines1:
                nextline = lines1.pop(0)
                if line == nextline:
                    self._log("exact match:", repr(line))
                    started = True
                    break
                elif match_func(nextline, line):
                    self._log(f"{match_nickname}:", repr(line))
                    self._log("{:>{width}}".format("with:", width=wnick), repr(nextline))
                    started = True
                    break
                else:
                    if consecutive and started:
                        msg = f"no consecutive match: {line!r}"
                        self._log(msg)
                        self._log(
                            "{:>{width}}".format("with:", width=wnick), repr(nextline)
                        )
                        self._fail(msg)
                    if not nomatchprinted:
                        self._log(
                            "{:>{width}}".format("nomatch:", width=wnick), repr(line)
                        )
                        nomatchprinted = True
                    self._log("{:>{width}}".format("and:", width=wnick), repr(nextline))
                extralines.append(nextline)
            else:
                msg = f"remains unmatched: {line!r}"
                self._log(msg)
                self._fail(msg)
        self._log_output = []

    def no_fnmatch_line(self, pat):
        __tracebackhide__ = True
        import fnmatch

        self._no_match_line(pat, fnmatch.fnmatch, "fnmatch")

    def no_re_match_line(self, pat):
        __tracebackhide__ = True
        import re

        self._no_match_line(pat, lambda name, pat: bool(re.match(pat, name)), "re.match")

    def _no_match_line(self, pat, match_func, match_nickname):
        __tracebackhide__ = True
        nomatch_printed = False
        wnick = len(match_nickname) + 1
        for line in self.lines:
            if match_func(line, pat):
                msg = f"{match_nickname}: {pat!r}"
                self._log(msg)
                self._log("{:>{width}}".format("with:", width=wnick), repr(line))
                self._fail(msg)
            else:
                if not nomatch_printed:
                    self._log("{:>{width}}".format("nomatch:", width=wnick), repr(pat))
                    nomatch_printed = True
                self._log("{:>{width}}".format("and:", width=wnick), repr(line))
        self._log_output = []

    def _fail(self, msg):
        __tracebackhide__ = True
        log_text = self._log_text
        self._log_output = []
        fail(log_text)


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
    TimeoutExpired = _subprocess.TimeoutExpired
    # Sentinel for popen()/run()'s stdin: close the child's stdin pipe. None is
    # a distinct, valid value (leave stdin inherited) per upstream.
    CLOSE_STDIN = object()

    def __init__(self, path, request_name, request=None):
        import pathlib

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
            _RUNNER_EXE = _os.environ.get("PYTEST_RS_EXE")
        if _RUNNER_PYTHONPATH is None:
            _RUNNER_PYTHONPATH = _os.environ.get("PYTHONPATH")
        for _var in _LIBPATH_VARS:
            if _var not in _RUNNER_LIBPATH and _var in _os.environ:
                _RUNNER_LIBPATH[_var] = _os.environ[_var]

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
        import sys

        entry = str(path if path is not None else self.path)
        # The current process (tests import what they just wrote) and the
        # child runner via PYTHONPATH (runs are subprocesses).
        sys.path.insert(0, entry)
        self._syspaths.insert(0, entry)

    def runpytest(self, *args, timeout=None):
        # Upstream's (default) in-process runs share the outer test's
        # warning-filter state: mirror the item's filterwarnings marks into
        # the child as -W options (farthest first, so the closest wins).
        _check_cfg_pytest_section(self.path, args)
        return self._runpytest(args, timeout=timeout, forward_filters=True)

    def _runpytest(self, args, *, timeout=None, forward_filters=False, hook_relay=None):
        import os
        import subprocess
        import time

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
        import io
        import logging
        import pickle

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

    runpytest_inprocess = runpytest

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
        import os

        env = os.environ.copy()
        for _var, _value in _RUNNER_LIBPATH.items():
            env.setdefault(_var, _value)
        existing = env.get("PYTHONPATH") or _RUNNER_PYTHONPATH
        if self._syspaths or existing:
            env["PYTHONPATH"] = os.pathsep.join(
                [*self._syspaths, *([existing] if existing else [])]
            )
        return env

    def popen(self, cmdargs, stdout=_subprocess.PIPE, stderr=_subprocess.PIPE, stdin=CLOSE_STDIN, **kw):
        """Spawn a subprocess. ``stdin`` may be CLOSE_STDIN (close the pipe),
        bytes (written then left open for communicate()), or any value passed
        straight to Popen (e.g. PIPE, None)."""
        import subprocess

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
        import subprocess
        import time

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
        import os

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
        import importlib.util

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
        # No in-process runner: a subprocess -v run parsed into a
        # HookRecorder-shaped result (ret / assertoutcome / listoutcomes).
        # The child's output is echoed so capsys sees what an in-process
        # run would have printed.
        import json
        import sys

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

    def inline_runsource(self, source, *args):
        path = self.makepyfile(source)
        return self.inline_run(*args, path)

    def inline_genitems(self, *args):
        """Run collection-only mode and return (items, reprec).

        Items are lightweight objects with .nodeid, .name, and .parent attributes.
        """
        import json

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
        import pathlib as _pathlib

        items = []
        for arg in args:
            path_str = str(arg)
            if "::" in path_str:
                file_part = path_str.split("::")[0]
                if file_part.endswith(".py"):
                    items.extend(self._collect_items_from_path(file_part))
                    continue
            p = _pathlib.Path(path_str)
            if not p.is_absolute():
                p = self.path / p
            if p.is_dir():
                config = self._request.config if self._request is not None else None
                python_files = "test_*.py *.py" if config is None else (
                    config.getini("python_files") if hasattr(config, "getini") else "test_*.py"
                )
                patterns = python_files.split() if isinstance(python_files, str) else list(python_files)
                for pat in patterns:
                    for py_file in sorted(p.glob(pat)):
                        if py_file.is_file():
                            items.extend(self._collect_items_from_path(py_file))
            elif path_str.endswith(".py"):
                items.extend(self._collect_items_from_path(path_str))
            elif path_str.endswith((".txt", ".rst", ".md")):
                from _pytest.doctest import DoctestItem, DoctestTextfile

                parent = DoctestTextfile(path_str, None)
                item = DoctestItem(path_str, None)
                item.parent = parent
                item.nodeid = path_str
                items.append(item)
        return items, reprec

    def _collect_items_from_path(self, path, parent_collector=None):
        """In-process collection of Function items from an existing .py file.

        Returns items with full mark data (own_markers, get_closest_marker,
        keywords) — the same objects getitems() returns, but without needing
        source text."""
        import importlib.util
        import itertools
        import pathlib
        import sys

        from pytest._marks import get_unpacked_marks
        from pytest._node import Function, _ModuleCollector, _NodeSession

        path = pathlib.Path(str(path))
        if not path.is_absolute():
            path = self.path / path
        module_name = path.stem
        spec = importlib.util.spec_from_file_location(module_name, path)
        if spec is None or spec.loader is None:
            return []
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
        import configparser
        import fnmatch as _fnmatch

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
            return any(name.startswith(p) or _fnmatch.fnmatch(name, p) for p in _func_patterns)

        def _is_test_class(name):
            if _class_patterns is None:
                return name.startswith("Test")
            return any(name.startswith(p) or _fnmatch.fnmatch(name, p) for p in _class_patterns)

        def _param_id(val):
            if val is None:
                return "None"
            return str(val)

        def make_item(func, nodeid_name, all_marks, cls=None, parent=None):
            lineno = getattr(getattr(func, "__code__", None), "co_firstlineno", 0)
            node = Function(
                f"{path.name}::{nodeid_name}",
                nodeid_name.rsplit("::", 1)[-1],
                all_marks,
                [],
                func,
                str(path),
                lineno,
            )
            node.module = module
            node.cls = cls
            node.parent = parent
            node._module_collector = module_collector
            if config is not None:
                node.config = config
            return node

        def expand_parametrize(func, base_name, extra_marks, cls=None, parent=None):
            """Return one item per parametrize combination, or one item if none."""
            func_marks = get_unpacked_marks(func)
            param_marks = [m for m in func_marks if m.name == "parametrize"]
            non_param_marks = [m for m in func_marks if m.name != "parametrize"]
            all_marks = [*non_param_marks, *extra_marks, *module_marks]

            if not param_marks:
                return [make_item(func, base_name, all_marks, cls, parent)]

            all_id_lists = []
            for pm in param_marks:
                argvalues = list(pm.args[1]) if len(pm.args) > 1 else []
                ids_kwarg = pm.kwargs.get("ids", None)
                level_ids = [
                    str(ids_kwarg[i]) if ids_kwarg is not None and i < len(ids_kwarg)
                    else _param_id(val)
                    for i, val in enumerate(argvalues)
                ]
                if level_ids:
                    all_id_lists.append(level_ids)

            if not all_id_lists:
                return [make_item(func, base_name, all_marks, cls, parent)]

            items = []
            for combo in itertools.product(*all_id_lists):
                suffix = "-".join(combo)
                param_name = f"{base_name}[{suffix}]"
                items.append(make_item(func, param_name, all_marks, cls, parent))
            return items

        items = []
        for name, obj in vars(module).items():
            if _is_test_func(name) and callable(obj) and not isinstance(obj, type):
                items.extend(expand_parametrize(obj, name, [], parent=parent_collector))
            elif _is_test_class(name) and isinstance(obj, type):
                class_marks = get_unpacked_marks(obj)
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
                    sub = expand_parametrize(
                        func, f"{name}::{mname}", class_marks, cls=obj,
                        parent=parent_collector,
                    )
                    for item in sub:
                        item.instance = obj
                    items.extend(sub)
        return items

    def getitems(self, source):
        """Collect Function item nodes from the source in-process (a light
        collection: module import + test functions/Test-class methods with
        merged marks — enough for the mark-evaluation tests; no fixtures)."""
        import pathlib

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
        import importlib.util
        import pathlib
        import sys

        from pytest._marks import get_unpacked_marks
        from pytest._node import Class, File, Function, _ModuleCollector, _NodeSession

        if withinit:
            (self.path / "__init__.py").touch()
        path = pathlib.Path(str(self.makepyfile(source)))
        config = self._request.config if self._request is not None else None

        # Import the module in-process
        module_name = path.stem
        spec = importlib.util.spec_from_file_location(module_name, path)
        if spec is not None and spec.loader is not None:
            mod = importlib.util.module_from_spec(spec)
            sys.modules[module_name] = mod
            try:
                spec.loader.exec_module(mod)
            except Exception:
                mod = None
        else:
            mod = None

        session = _NodeSession(config)
        module_collector = _ModuleCollector(mod, session, path) if mod is not None else None
        module_marks = get_unpacked_marks(mod) if mod is not None else []

        pytester_self = self

        class _IPModule(File):
            """In-process Module collector returned by getmodulecol."""

            def __init__(self):
                super().__init__(name=path.name, config=config, path=path, nodeid=path.name)
                self.module = mod
                self.cls = None
                self.instance = None
                self._children = None

            def collect(self):
                if self._children is not None:
                    return list(self._children)
                if mod is None:
                    self._children = []
                    return []
                children = []
                for name, obj in vars(mod).items():
                    if name.startswith("test") and callable(obj) and not isinstance(obj, type):
                        marks = [*get_unpacked_marks(obj), *module_marks]
                        lineno = getattr(getattr(obj, "__code__", None), "co_firstlineno", 0)
                        fn = Function(
                            f"{path.name}::{name}", name, marks, [], obj, str(path), lineno,
                        )
                        fn.module = mod
                        fn.cls = None
                        fn.parent = self
                        if module_collector is not None:
                            fn._module_collector = module_collector
                        if config is not None:
                            fn.config = config
                        children.append(fn)
                    elif name.startswith("Test") and isinstance(obj, type):
                        cls_node = _IPClass(name, obj, self)
                        children.append(cls_node)
                self._children = children
                return list(children)

        class _IPClass(Class):
            """In-process Class collector returned by collect_by_name on a Module."""

            def __init__(self, name, cls_obj, parent_module):
                super().__init__(name=name, config=config, path=path, nodeid=f"{path.name}::{name}")
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
                    marks = [*get_unpacked_marks(func), *class_marks, *module_marks]
                    lineno = getattr(getattr(func, "__code__", None), "co_firstlineno", 0)
                    fn = Function(
                        f"{path.name}::{self.name}::{mname}",
                        mname, marks, [], func, str(path), lineno,
                    )
                    fn.module = mod
                    fn.cls = self._cls_obj
                    fn.instance = self._cls_obj
                    fn.parent = self
                    if module_collector is not None:
                        fn._module_collector = module_collector
                    if config is not None:
                        fn.config = config
                    children.append(fn)
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
        import pathlib
        import shutil

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
        import os

        env = os.environ.copy()
        shim_root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
        existing = env.get("PYTHONPATH")
        env["PYTHONPATH"] = os.pathsep.join([shim_root, *([existing] if existing else [])])
        return env

    def runpython(self, script):
        import subprocess
        import sys
        import time

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
        import subprocess
        import sys
        import time

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


class _OutcomeReport:
    def __init__(self, nodeid, outcome=None, when="call", longrepr=None):
        self.nodeid = nodeid
        self.when = when
        self.outcome = outcome
        self.longrepr = longrepr

    @property
    def passed(self):
        return self.outcome == "passed"

    @property
    def skipped(self):
        return self.outcome == "skipped"

    @property
    def failed(self):
        return self.outcome == "failed"

    def __repr__(self):
        return f"<OutcomeReport {self.nodeid!r} {self.when} {self.outcome}>"


class _RelayItem:
    """Lightweight reconstruction of a pytest node from relay JSON data."""

    def __init__(self, name, nodeid):
        self.name = name
        self.nodeid = nodeid

    def __repr__(self):
        return f"<_RelayItem {self.nodeid!r}>"


class _RelaySession:
    """Lightweight reconstruction of session with .items from relay JSON data."""

    def __init__(self, items):
        self.items = items


class _RelayCollectReport:
    """Lightweight CollectReport reconstructed from relay JSON."""

    def __init__(self, nodeid, outcome, longrepr):
        self.nodeid = nodeid
        self.outcome = outcome
        self.longrepr = longrepr
        self.failed = outcome == "failed"
        self.passed = outcome == "passed"
        self.skipped = outcome == "skipped"


class _RelayHookCall:
    """Reconstructed hook call record; named attributes come from relay JSON."""

    def __init__(self, hook_name, kwargs):
        self.__dict__.update(kwargs)
        self._name = hook_name

    def __repr__(self):
        d = {k: v for k, v in self.__dict__.items() if k != "_name"}
        return f"<_RelayHookCall {self._name!r}(**{d!r})>"

    @classmethod
    def _from_event(cls, event):
        hook = event["hook"]
        if hook == "pytest_deselected":
            items = [_RelayItem(i["name"], i["nodeid"]) for i in event.get("items", [])]
            return cls(hook, {"items": items})
        if hook == "pytest_collection_finish":
            items = [_RelayItem(i["name"], i["nodeid"]) for i in event.get("session_items", [])]
            return cls(hook, {"session": _RelaySession(items)})
        if hook == "pytest_collectreport":
            report = _RelayCollectReport(
                event.get("nodeid", ""),
                event.get("outcome", ""),
                event.get("longrepr", ""),
            )
            return cls(hook, {"report": report})
        return cls(hook, {k: v for k, v in event.items() if k != "hook"})


class InlineRunResult:
    """The subset of pytester's HookRecorder API used by upstream suites."""

    _WORDS = {
        "PASSED": "passed",
        "XPASS": "passed",
        "SKIPPED": "skipped",
        "XFAIL": "skipped",
        "FAILED": "failed",
        "ERROR": "failed",
    }

    def __init__(self, run_result, hook_events=None):
        self._result = run_result
        self.ret = run_result.ret
        self._hook_calls = [_RelayHookCall._from_event(e) for e in (hook_events or [])]

    @property
    def calls(self):
        """All recorded hook calls (upstream HookRecorder.calls list)."""
        return self._hook_calls

    def getcalls(self, names):
        """Return recorded hook calls matching the given name(s) (space-sep string or list)."""
        if isinstance(names, str):
            names = names.split()
        return [c for c in self._hook_calls if c._name in names]

    def getfailedcollections(self):
        return [rep for rep in self.getreports("pytest_collectreport") if rep.failed]

    def getreports(self, names=("pytest_collectreport", "pytest_runtest_logreport")):
        return [c.report for c in self.getcalls(names) if hasattr(c, "report")]

    def assert_contains(self, entries):
        """Assert that recorded hook calls contain the given (name, expr) pairs in order."""
        import sys

        __tracebackhide__ = True
        i = 0
        entries = list(entries)
        backlocals = dict(sys._getframe(1).f_locals)
        while entries:
            name, check = entries.pop(0)
            for ind, call in enumerate(self._hook_calls[i:]):
                if call._name == name:
                    if eval(check, backlocals, call.__dict__):  # noqa: S307
                        pass
                    else:
                        continue
                    i += ind + 1
                    break
            else:
                from pytest._outcomes import fail
                fail(f"could not find {name!r} check {check!r}")

    def listoutcomes(self):
        outcomes = {"passed": [], "skipped": [], "failed": []}
        seen = set()
        for line in self._result.outlines:
            parts = line.split()
            if len(parts) < 2:
                continue
            # Format 1: "nodeid WORD [progress]" — verbose run-time output
            # Format 2: "WORD nodeid - message" — short test summary info section
            if parts[0] in self._WORDS:
                # Short summary format: "FAILED nodeid - ..."
                word = parts[0]
                nodeid = parts[1]
            else:
                # Verbose format: "nodeid WORD [progress]"
                nodeid = parts[0]
                word = parts[1]

            bucket = self._WORDS.get(word)
            is_test_node = (
                "::" in nodeid
                or nodeid.endswith((".txt", ".rst", ".md"))
                or (nodeid.endswith(".py") and "." in nodeid.split("/")[-1][:-3])
            )
            if bucket is not None and is_test_node and nodeid not in seen:
                seen.add(nodeid)
                outcomes[bucket].append(_OutcomeReport(nodeid, bucket))
        # Collect-level reports (e.g. a skipped DoctestModule, a module that
        # failed to import) have no per-item lines; the final summary counts
        # are authoritative, so pad each bucket up to them. Upstream's
        # HookRecorder counts collect reports too: xpassed→passed,
        # xfailed→skipped, errors→failed.
        totals = self._result.parseoutcomes()
        expected = {
            "passed": totals.get("passed", 0) + totals.get("xpassed", 0),
            "skipped": totals.get("skipped", 0) + totals.get("xfailed", 0),
            "failed": totals.get("failed", 0) + totals.get("errors", 0),
        }
        for bucket, want in expected.items():
            while len(outcomes[bucket]) < want:
                outcomes[bucket].append(_OutcomeReport("<collect report>", bucket))
        return outcomes["passed"], outcomes["skipped"], outcomes["failed"]

    def _teardown_reports(self):
        """Failed teardown reports parsed from the "ERROR at teardown of X"
        failure sections."""
        import re

        text = "\n".join(self._result.outlines)
        return [
            _OutcomeReport(match.group(1), "failed", "teardown", match.group(2))
            for match in re.finditer(
                r"_{6,} ERROR at teardown of (.+?) _{6,}\n(.*?)(?=\n_{6,} |\n={6,} |\Z)",
                text,
                re.S,
            )
        ]

    def matchreport(self, inamepart="", when=None):
        """The single report whose nodeid's last part contains inamepart
        (HookRecorder.matchreport: call reports unless `when` says else)."""
        if when == "teardown":
            candidates = self._teardown_reports()
        else:
            passed, skipped, failed = self.listoutcomes()
            candidates = [*passed, *skipped, *failed]
        values = [
            rep for rep in candidates if not inamepart or inamepart in rep.nodeid.split("::")[-1]
        ]
        if not values:
            raise ValueError(f"could not find test report matching {inamepart!r}")
        if len(values) > 1:
            raise ValueError(f"found 2 or more testreports matching {inamepart!r}: {values}")
        return values[0]

    def assertoutcome(self, passed=0, skipped=0, failed=0):
        __tracebackhide__ = True
        got_passed, got_skipped, got_failed = self.listoutcomes()
        got = (len(got_passed), len(got_skipped), len(got_failed))
        assert got == (passed, skipped, failed), (
            f"assertoutcome: expected (passed={passed}, skipped={skipped}, "
            f"failed={failed}), got {got}:\n{self._result.stdout}"
        )

    def countoutcomes(self):
        return [len(outcome) for outcome in self.listoutcomes()]


class Testdir(Pytester):
    """Legacy pytester alias (the pre-7.0 testdir fixture API): paths are
    py.path-like LocalPath objects instead of pathlib.Path."""

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
    import os
    import sys

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
        from io import StringIO

        self.stringio = StringIO()

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
    from _pytest.pytester import SysPathsSnapshot, SysModulesSnapshot
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
