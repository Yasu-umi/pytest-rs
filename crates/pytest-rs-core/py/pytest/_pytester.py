"""pytester: run pytest-rs as a child process so upstream test suites can
exercise the runner itself."""

import os as _os
import re as _re

from pytest._fixtures import fixture
from pytest._outcomes import fail

# Captured before any test mutates os.environ: tests sometimes
# mock.patch.dict(os.environ, ..., clear=True) around runpytest(), which would
# otherwise strip the runner path and the import path the subprocess pytester
# needs (in-process pytester upstream shares sys.modules/sys.path, so a cleared
# env still finds installed plugins; we approximate that by remembering both).
_RUNNER_EXE = _os.environ.get("PYTEST_RS_EXE")
_RUNNER_PYTHONPATH = _os.environ.get("PYTHONPATH")

_OUTCOME_RE = _re.compile(
    r"(\d+) (passed|failed|skipped|xfailed|xpassed|errors?|warnings?|deselected|rerun)"
)
_ANSI_RE = _re.compile(r"\x1b\[[0-9;]*m")


class LineMatcher:
    def __init__(self, lines):
        self.lines = lines

    def __str__(self):
        return "\n".join(self.lines)

    def str(self):
        return str(self)

    @staticmethod
    def _pattern_lines(patterns):
        """A multi-line string becomes a dedented pattern list (pytest's
        Source semantics); a plain one-line string is a single pattern."""
        if not isinstance(patterns, str):
            return patterns
        if "\n" not in patterns:
            return [patterns]
        import textwrap

        lines = textwrap.dedent(patterns).splitlines()
        while lines and not lines[0].strip():
            lines.pop(0)
        while lines and not lines[-1].strip():
            lines.pop()
        return lines

    def fnmatch_lines(self, patterns, *, consecutive=False):
        __tracebackhide__ = True
        import fnmatch

        patterns = self._pattern_lines(patterns)

        # Upstream checks equality before globbing, so a literal "[1, 2, 3]"
        # pattern matches itself despite being a valid character class.
        def matches(line, pattern):
            return line == pattern or fnmatch.fnmatch(line, pattern)

        if consecutive:
            # The whole pattern block must match a consecutive run of lines.
            for start in range(len(self.lines)):
                window = self.lines[start : start + len(patterns)]
                if len(window) == len(patterns) and all(
                    matches(line, pattern) for line, pattern in zip(window, patterns)
                ):
                    return
            fail(f"fnmatch_lines: no consecutive match for {patterns!r} in:\n{self}")
        remaining = list(self.lines)
        for pattern in patterns:
            for index, line in enumerate(remaining):
                if matches(line, pattern):
                    remaining = remaining[index + 1 :]
                    break
            else:
                fail(f"fnmatch_lines: no line matches {pattern!r} in:\n{self}")

    def no_fnmatch_line(self, pattern):
        __tracebackhide__ = True
        import fnmatch

        for line in self.lines:
            if line == pattern or fnmatch.fnmatch(line, pattern):
                fail(f"no_fnmatch_line: unexpectedly matched {pattern!r}: {line!r}")

    def re_match_lines(self, patterns, *, consecutive=False):
        __tracebackhide__ = True
        import re

        patterns = self._pattern_lines(patterns)

        def matches(line, pattern):
            return re.match(pattern, line) is not None

        if consecutive:
            for start in range(len(self.lines)):
                window = self.lines[start : start + len(patterns)]
                if len(window) == len(patterns) and all(
                    matches(line, pattern) for line, pattern in zip(window, patterns)
                ):
                    return
            fail(f"re_match_lines: no consecutive match for {patterns!r} in:\n{self}")
        remaining = list(self.lines)
        for pattern in patterns:
            for index, line in enumerate(remaining):
                if matches(line, pattern):
                    remaining = remaining[index + 1 :]
                    break
            else:
                fail(f"re_match_lines: no line matches {pattern!r} in:\n{self}")

    def re_match_lines_random(self, patterns):
        __tracebackhide__ = True
        import re

        patterns = self._pattern_lines(patterns)
        for pattern in patterns:
            if not any(re.match(pattern, line) for line in self.lines):
                fail(f"re_match_lines_random: no line matches {pattern!r} in:\n{self}")

    def no_re_match_line(self, pattern):
        __tracebackhide__ = True
        import re

        for line in self.lines:
            if re.match(pattern, line):
                fail(f"no_re_match_line: unexpectedly matched {pattern!r}: {line!r}")

    def fnmatch_lines_random(self, patterns):
        __tracebackhide__ = True
        import fnmatch

        patterns = self._pattern_lines(patterns)
        for pattern in patterns:
            if not any(line == pattern or fnmatch.fnmatch(line, pattern) for line in self.lines):
                fail(f"fnmatch_lines_random: no line matches {pattern!r} in:\n{self}")


class RunResult:
    def __init__(self, ret, outlines, errlines, duration):
        self.ret = ret
        self.outlines = outlines
        self.errlines = errlines
        self.duration = duration
        self.stdout = LineMatcher(outlines)
        self.stderr = LineMatcher(errlines)

    def parseoutcomes(self):
        for line in reversed(self.outlines):
            clean = _ANSI_RE.sub("", line)
            if clean.startswith("====") and " in " in clean:
                found = {}
                for count, key in _OUTCOME_RE.findall(clean):
                    found[key.rstrip("s") if key in ("errors", "warnings") else key] = int(count)
                return found
        return {}

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
            "error": errors,
            "xpassed": xpassed,
            "xfailed": xfailed,
        }
        got = {key: actual.get(key, 0) for key in expected}
        assert got == expected, f"assert_outcomes: expected {expected}, got {actual}"
        if warnings is not None:
            assert actual.get("warning", 0) == warnings
        if deselected is not None:
            assert actual.get("deselected", 0) == deselected


class Pytester:
    def __init__(self, path, request_name, request=None):
        import pathlib

        self.path = pathlib.Path(path)
        self._name = request_name
        self._request = request
        self._syspaths = []
        # Capture the runner path now (fixture setup, before a test body can
        # mock.patch.dict(os.environ, clear=True) around runpytest). The
        # module-level import runs too early — pytest imports this before the
        # engine sets PYTEST_RS_EXE.
        global _RUNNER_EXE, _RUNNER_PYTHONPATH
        if _RUNNER_EXE is None:
            _RUNNER_EXE = _os.environ.get("PYTEST_RS_EXE")
        if _RUNNER_PYTHONPATH is None:
            _RUNNER_PYTHONPATH = _os.environ.get("PYTHONPATH")

    def _makefile(self, ext, args, kwargs):
        items = list(kwargs.items())
        if args:
            source = "\n".join(str(arg) for arg in args)
            items.insert(0, (self._name, source))
        paths = []
        for basename, source in items:
            import textwrap

            # with_suffix both appends and replaces ("pkg/test_1.py" stays
            # itself), matching upstream pytester.
            path = (self.path / basename).with_suffix(ext)
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(textwrap.dedent(str(source)).lstrip("\n"))
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
        return self._runpytest(args, timeout=timeout, forward_filters=True)

    def _runpytest(self, args, *, timeout=None, forward_filters=False):
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
        start = time.perf_counter()
        # cwd inherits like upstream's subprocess runs: the pytester fixture
        # chdir'd to self.path at setup, and a test that os.chdir()s deeper
        # means the nested run to resolve relative args from there.
        proc = subprocess.run(
            [exe, f"--basetemp={basetemp}", *[str(arg) for arg in args]],
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
        return self._runpytest(args, timeout=timeout, forward_filters=False)

    runpytest_inprocess = runpytest

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
                add(**_accepted_kwargs(add, {"parser": _parser.parser, "pluginmanager": pluginmanager}))

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

    def inline_run(self, *args):
        # No in-process runner: a subprocess -v run parsed into a
        # HookRecorder-shaped result (ret / assertoutcome / listoutcomes).
        # The child's output is echoed so capsys sees what an in-process
        # run would have printed.
        import sys

        result = self.runpytest("-v", *[str(arg) for arg in args])
        if result.outlines:
            sys.stdout.write("\n".join(result.outlines) + "\n")
        if result.errlines:
            sys.stderr.write("\n".join(result.errlines) + "\n")
        return InlineRunResult(result)

    def inline_runsource(self, source, *args):
        path = self.makepyfile(source)
        return self.inline_run(*args, path)

    def inline_genitems(self, *args):
        """Run collection-only mode and return (items, reprec).

        Items are lightweight objects with .nodeid, .name, and .parent attributes.
        """
        result = self.runpytest("--collect-only", "-q", *[str(arg) for arg in args])
        reprec = InlineRunResult(result)
        items = []
        parents: dict = {}
        for line in result.outlines:
            line = line.strip()
            if (
                not line
                or line.startswith("=")
                or line.startswith("<")
                or line.startswith("no tests")
            ):
                continue
            if "::" in line or line.endswith((".txt", ".rst", ".md")):
                # Deduce type from nodeid
                nodeid = line.split()[0] if line.split() else line
                from _pytest.doctest import DoctestItem, DoctestModule, DoctestTextfile

                filename = nodeid.split("::")[0]
                if filename not in parents:
                    if filename.endswith((".txt", ".rst", ".md")):
                        parents[filename] = DoctestTextfile(filename, None)
                    else:
                        parents[filename] = DoctestModule(filename, None)
                item = DoctestItem(nodeid, None)
                item.parent = parents[filename]
                item.nodeid = nodeid
                item._pytester_path = self.path
                items.append(item)
        return items, reprec

    def getitems(self, source):
        """Collect Function item nodes from the source in-process (a light
        collection: module import + test functions/Test-class methods with
        merged marks — enough for the mark-evaluation tests; no fixtures)."""
        import importlib.util
        import sys

        from pytest._marks import get_unpacked_marks
        from pytest._node import Function

        path = self.makepyfile(source)
        module_name = path.stem
        spec = importlib.util.spec_from_file_location(module_name, path)
        assert spec is not None and spec.loader is not None
        module = importlib.util.module_from_spec(spec)
        sys.modules[module_name] = module
        spec.loader.exec_module(module)

        config = self._request.config if self._request is not None else None
        module_marks = get_unpacked_marks(module)

        def make_item(func, nodeid_name, extra_marks):
            marks = [*get_unpacked_marks(func), *extra_marks, *module_marks]
            lineno = getattr(getattr(func, "__code__", None), "co_firstlineno", 0)
            node = Function(
                f"{path.name}::{nodeid_name}",
                nodeid_name.rsplit("::", 1)[-1],
                marks,
                [],
                func,
                str(path),
                lineno,
            )
            node.module = module
            node.parent = None
            if config is not None:
                node.config = config
            return node

        items = []
        for name, obj in vars(module).items():
            if name.startswith("test") and callable(obj) and not isinstance(obj, type):
                items.append(make_item(obj, name, []))
            elif name.startswith("Test") and isinstance(obj, type):
                class_marks = get_unpacked_marks(obj)
                for mname, mobj in vars(obj).items():
                    mobj = getattr(mobj, "__func__", mobj)
                    if mname.startswith("test") and callable(mobj):
                        items.append(make_item(mobj, f"{name}::{mname}", class_marks))
        return items

    def getitem(self, source, funcname="test_func"):
        """The single collected item named funcname (upstream getitem)."""
        for item in self.getitems(source):
            if item.name == funcname:
                return item
        fail(f"{funcname!r} item not found in module:\n{source}")

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

    def __init__(self, run_result):
        self._result = run_result
        self.ret = run_result.ret

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
            "failed": totals.get("failed", 0) + totals.get("error", 0),
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


def _make_runner_dir(request, tmp_path_factory, cls):
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
    # Upstream: nested runs root their tmp dirs under a per-pytester
    # directory (tests inspect it via pytester._test_tmproot).
    runner._test_tmproot = tmp_path_factory.mktemp(f"tmp-{name}", numbered=True)
    old_temproot = os.environ.get("PYTEST_DEBUG_TEMPROOT")
    os.environ["PYTEST_DEBUG_TEMPROOT"] = str(runner._test_tmproot)
    # Upstream pytester sanitizes the outer PYTEST_ADDOPTS at setup; a test
    # monkeypatch.setenv afterwards still reaches the nested run.
    old_addopts = os.environ.pop("PYTEST_ADDOPTS", None)
    yield runner
    if old_temproot is None:
        os.environ.pop("PYTEST_DEBUG_TEMPROOT", None)
    else:
        os.environ["PYTEST_DEBUG_TEMPROOT"] = old_temproot
    if old_addopts is not None:
        os.environ["PYTEST_ADDOPTS"] = old_addopts
    for entry in runner._syspaths:
        if entry in sys.path:
            sys.path.remove(entry)
    os.chdir(old_cwd)


@fixture
def pytester(request, tmp_path_factory):
    yield from _make_runner_dir(request, tmp_path_factory, Pytester)


@fixture
def testdir(request, tmp_path_factory):
    yield from _make_runner_dir(request, tmp_path_factory, Testdir)
