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

    def fnmatch_lines(self, patterns):
        __tracebackhide__ = True
        import fnmatch

        if isinstance(patterns, str):
            patterns = [patterns]
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
    def __init__(self, path, request_name):
        import pathlib

        self.path = pathlib.Path(path)
        self._name = request_name
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
        return paths[0] if len(paths) == 1 else paths

    def makepyfile(self, *args, **kwargs):
        return self._makefile(".py", args, kwargs)

    def makeconftest(self, source):
        return self._makefile(".py", [], {"conftest": source})

    def maketxtfile(self, *args, **kwargs):
        return self._makefile(".txt", args, kwargs)

    def makeini(self, source):
        return self._makefile(".ini", [], {"tox": source})

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


def _make_runner_dir(request, cls):
    import os
    import re
    import shutil
    import sys
    import tempfile

    name = re.sub(r"\W", "_", request.node.name)
    path = tempfile.mkdtemp(prefix="pytest-rs-pytester-")
    old_cwd = os.getcwd()
    os.chdir(path)
    runner = cls(path, name)
    yield runner
    for entry in runner._syspaths:
        if entry in sys.path:
            sys.path.remove(entry)
    os.chdir(old_cwd)
    shutil.rmtree(path, ignore_errors=True)


@fixture
def pytester(request):
    yield from _make_runner_dir(request, Pytester)


@fixture
def testdir(request):
    yield from _make_runner_dir(request, Testdir)
