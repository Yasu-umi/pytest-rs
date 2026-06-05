"""pytester: run pytest-rs as a child process so upstream test suites can
exercise the runner itself."""

import re as _re

from pytest._fixtures import fixture
from pytest._outcomes import fail

_OUTCOME_RE = _re.compile(
    r"(\d+) (passed|failed|skipped|xfailed|xpassed|errors?|warnings?|deselected)"
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
        if consecutive:
            # The whole pattern block must match a consecutive run of lines.
            for start in range(len(self.lines)):
                window = self.lines[start : start + len(patterns)]
                if len(window) == len(patterns) and all(
                    fnmatch.fnmatch(line, pattern) for line, pattern in zip(window, patterns)
                ):
                    return
            fail(f"fnmatch_lines: no consecutive match for {patterns!r} in:\n{self}")
        remaining = list(self.lines)
        for pattern in patterns:
            for index, line in enumerate(remaining):
                if fnmatch.fnmatch(line, pattern):
                    remaining = remaining[index + 1 :]
                    break
            else:
                fail(f"fnmatch_lines: no line matches {pattern!r} in:\n{self}")

    def no_fnmatch_line(self, pattern):
        __tracebackhide__ = True
        import fnmatch

        for line in self.lines:
            if fnmatch.fnmatch(line, pattern):
                fail(f"no_fnmatch_line: unexpectedly matched {pattern!r}: {line!r}")

    def fnmatch_lines_random(self, patterns):
        __tracebackhide__ = True
        import fnmatch

        patterns = self._pattern_lines(patterns)
        for pattern in patterns:
            if not any(fnmatch.fnmatch(line, pattern) for line in self.lines):
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

    def _makefile(self, ext, args, kwargs):
        items = list(kwargs.items())
        if args:
            source = "\n".join(str(arg) for arg in args)
            items.insert(0, (self._name, source))
        paths = []
        for basename, source in items:
            import textwrap

            path = self.path / (basename + ext)
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

    def runpytest(self, *args):
        import os
        import subprocess
        import time

        exe = os.environ.get("PYTEST_RS_EXE")
        if exe is None:
            fail("PYTEST_RS_EXE is not set; pytester cannot run the runner")
        env = os.environ.copy()
        if self._syspaths:
            existing = env.get("PYTHONPATH")
            entries = [*self._syspaths, *([existing] if existing else [])]
            env["PYTHONPATH"] = os.pathsep.join(entries)
        start = time.perf_counter()
        proc = subprocess.run(
            [exe, *[str(arg) for arg in args]],
            cwd=self.path,
            capture_output=True,
            text=True,
            timeout=120,
            env=env,
        )
        duration = time.perf_counter() - start
        outlines = [_ANSI_RE.sub("", line) for line in proc.stdout.splitlines()]
        errlines = [_ANSI_RE.sub("", line) for line in proc.stderr.splitlines()]
        return RunResult(proc.returncode, outlines, errlines, duration)

    runpytest_subprocess = runpytest
    runpytest_inprocess = runpytest

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
        raise LookupError(
            f'example "{example_path}" is not found as a file or directory'
        )

    def runpython(self, script):
        import os
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
            env=os.environ.copy(),
        )
        duration = time.perf_counter() - start
        return RunResult(
            proc.returncode,
            proc.stdout.splitlines(),
            proc.stderr.splitlines(),
            duration,
        )

    def runpython_c(self, command):
        import os
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
            env=os.environ.copy(),
        )
        duration = time.perf_counter() - start
        return RunResult(
            proc.returncode,
            proc.stdout.splitlines(),
            proc.stderr.splitlines(),
            duration,
        )


class _OutcomeReport:
    def __init__(self, nodeid):
        self.nodeid = nodeid

    def __repr__(self):
        return f"<OutcomeReport {self.nodeid!r}>"


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
        for line in self._result.outlines:
            parts = line.split()
            if len(parts) >= 2 and "::" in parts[0]:
                bucket = self._WORDS.get(parts[1])
                if bucket is not None:
                    outcomes[bucket].append(_OutcomeReport(parts[0]))
        return outcomes["passed"], outcomes["skipped"], outcomes["failed"]

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
    yield runner
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
