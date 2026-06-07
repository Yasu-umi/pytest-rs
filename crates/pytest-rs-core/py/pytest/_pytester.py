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

    def re_match_lines(self, patterns):
        __tracebackhide__ = True
        import re

        patterns = self._pattern_lines(patterns)
        remaining = list(self.lines)
        for pattern in patterns:
            for index, line in enumerate(remaining):
                if re.match(pattern, line):
                    remaining = remaining[index + 1 :]
                    break
            else:
                fail(f"re_match_lines: no line matches {pattern!r} in:\n{self}")

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
        # Upstream pytester parity: nested runs get a numbered --basetemp
        # under this pytester dir, so their tmp dirs are cleaned up with it
        # (a later user-passed --basetemp still wins).
        n = sum(1 for p in self.path.glob("runpytest-*"))
        basetemp = self.path / f"runpytest-{n}"
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
        # Color is gated by --color/tty detection in the engine; pytester
        # passes output through raw so color tests can assert escapes.
        outlines = proc.stdout.splitlines()
        errlines = proc.stderr.splitlines()
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
                outcomes[bucket].append(_OutcomeReport(nodeid))
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
                outcomes[bucket].append(_OutcomeReport("<collect report>"))
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
