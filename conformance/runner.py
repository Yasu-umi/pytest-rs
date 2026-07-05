"""Conformance harness: run upstream test suites (pytest, pytest-asyncio, ...)
under pytest-rs, file by file, and score the results.

Usage:
    uv run python conformance/runner.py                 # all enabled suites
    uv run python conformance/runner.py --suite pytest  # one suite
    uv run python conformance/runner.py --check         # gate on expected/*.toml
    uv run python conformance/runner.py --suite pytest-cov --test path/to/test.py::node  # one node (diagnostic)
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import tomllib
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CACHE = ROOT / ".tmp" / "conformance"
BINARY = ROOT / "target" / "debug" / "pytest-rs-bin"
# Results are platform-scoped: counts differ between linux and darwin
# (platform-specific skips, system deps), so each platform owns its
# scoreboard; linux is canonical (regenerated and committed from CI).
PLATFORM = "linux" if sys.platform.startswith("linux") else sys.platform
SCOREBOARD = ROOT / "conformance" / "scoreboard"
RESULTS_DOC = ROOT / "conformance" / "RESULTS.md"
TIMEOUT_S = 120

SUMMARY_RE = re.compile(
    r"(?:(?P<failed>\d+) failed)?(?:, )?"
    r"(?:(?P<passed>\d+) passed)?(?:, )?"
    r"(?:(?P<skipped>\d+) skipped)?(?:, )?"
    r"(?:(?P<errors>\d+) errors?)?(?:, )?"
    r"(?:(?P<deselected>\d+) deselected)?"
)
ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")


def _failed_nodeid(row: str) -> str | None:
    """Extract the nodeid from a "short test summary info" row, e.g.
    'FAILED path::Class::test - AssertionError: ...' -> 'path::Class::test'.
    Returns None for rows that aren't a FAILED/ERROR summary line."""
    for prefix in ("FAILED ", "ERROR "):
        if row.startswith(prefix):
            return row[len(prefix) :].split(" - ", 1)[0].strip()
    return None


@dataclass
class FileResult:
    file: str
    status: str  # "passed" | "failed" | "error" | "timeout" | "no-tests"
    exit_code: int | None
    passed: int = 0
    failed: int = 0
    skipped: int = 0
    errors: int = 0
    deselected: int = 0
    known_failed: int = 0
    stdout: str = ""
    stderr: str = ""


class Suite:
    def __init__(self, name: str, config: dict):
        self.name = name
        self.repo = config.get("repo", "")
        self.tag = config["tag"]
        self.testpaths = config["testpaths"]
        # Filename/path globs that count as test files, mirroring the suite's
        # own `python_files` ini setting. Patterns with a "/" are resolved
        # relative to the checkout root; bare patterns are matched recursively
        # under each testpath by basename. Defaults to pytest's stock globs.
        self.python_files: list[str] = config.get("python_files", ["test_*.py", "*_test.py"])
        self.enabled = config.get("enabled", False)
        self.local = config.get("local")
        # Some src-layout packages generate their version module at build time
        # (absent from a git checkout); use_src = false skips the checkout's
        # src/ shadow so the installed dist (with the generated file) is used.
        self.use_src = config.get("use_src", True)
        self.deps: list[str] = config.get("deps", [])
        self.exclude: list[str] = config.get("exclude", [])
        # Node ids never run (flaky under load, or environmentally invalid —
        # e.g. depend on a mock/CI-detection mechanism pytest-rs's execution
        # model doesn't apply to — so even attempting them would destabilize
        # the committed results the release gate compares against). Matched
        # by nodeid PREFIX (mirrors pytest's own --deselect semantics), so a
        # short name here can collaterally swallow longer sibling nodeids
        # (e.g. "test_foo" also deselects "test_foo_bar") — prefer
        # known_failures below when the test runs fine and just fails
        # predictably; it matches by exact nodeid instead.
        self.deselect: list[str] = config.get("deselect", [])
        # Node ids that run normally but are known, permanent, accepted
        # non-conformance (e.g. a cosmetic path-display difference from
        # pytest-rs's own module layout). Unlike deselect, these are matched
        # by EXACT nodeid against the "short test summary info" FAILED/ERROR
        # lines post-hoc in Python (see _failed_nodeid) — never passed to the
        # pytest-rs CLI — so they can't collaterally swallow a sibling test,
        # and they keep verifying the test still fails as expected (if it
        # starts passing, it's silently absorbed into the normal passed
        # count instead of masking a real fix).
        self.known_failures: list[str] = config.get("known_failures", [])
        # "ecosystem" (pytest + its plugins, the reimplementation targets)
        # or "real-world" (famous suites run as drop-in evidence).
        self.category: str = config.get("category", "ecosystem")
        # Extra PYTHONPATH entries relative to the checkout (e.g. "." for
        # flat package layouts).
        self.pythonpath: list[str] = config.get("pythonpath", [])
        # Files that legitimately run for minutes (wall-clock-bound
        # upstream too): they get a 3x timeout and submit first so they
        # never start at the tail of the pool.
        self.slow_files: list[str] = config.get("slow_files", [])
        self.slow_timeout_multiplier: int = config.get("slow_timeout_multiplier", 3)
        # Extra environment variables for this suite's runner invocations
        # (e.g. PYTEST_RS_INLINE_INPROCESS=1 for suites whose pytester
        # inline_run benefits from the in-process backend). Per-suite because
        # the runner launches each suite as its own process.
        self.env: dict[str, str] = {str(k): str(v) for k, v in config.get("env", {}).items()}
        # Install from PyPI wheel instead of cloning source (for packages
        # with C extensions whose tests live inside the package).
        self.package: str | None = config.get("package")
        # Extra CLI arguments passed to the runner for every file in this
        # suite (e.g. ["-p", "pytest_run_parallel.plugin"] to force-load a
        # plugin whose entry points are unreachable via PYTHONPATH deps).
        self.extra_args: list[str] = config.get("extra_args", [])
        self.checkout = CACHE / f"{self.name}-{self.tag}"
        self.src_dir: Path | None = None

    def deps_dir(self) -> Path | None:
        """Install the suite's extra runtime deps into a --target dir, used
        as PYTHONPATH so upstream tests can import them."""
        if self.package:
            return self.checkout
        if not self.deps:
            return None
        target = CACHE / "deps" / self.name
        marker = target / ".deps.txt"
        wanted = "\n".join(sorted(self.deps))
        if marker.exists() and marker.read_text() == wanted:
            return target
        target.mkdir(parents=True, exist_ok=True)
        subprocess.run(
            ["uv", "pip", "install", "--target", str(target), *self.deps],
            check=True,
            capture_output=True,
        )
        marker.write_text(wanted)
        return target

    def _install_package(self) -> None:
        """Install a PyPI wheel into the checkout dir (for suites whose tests
        live inside a compiled package — the wheel provides .so files)."""
        marker = self.checkout / ".installed.txt"
        assert self.package is not None
        all_pkgs = [self.package] + self.deps
        wanted = "\n".join(sorted(all_pkgs))
        if marker.exists() and marker.read_text() == wanted:
            return
        self.checkout.mkdir(parents=True, exist_ok=True)
        subprocess.run(
            ["uv", "pip", "install", "--target", str(self.checkout), *all_pkgs],
            check=True,
            capture_output=True,
        )
        marker.write_text(wanted)

    def fetch(self, use_local: bool) -> None:
        if self.package:
            self._install_package()
            return
        if use_local and self.local is not None:
            local = (ROOT / self.local).resolve()
            if local.exists():
                self.checkout = local
                self._detect_src()
                self._ensure_scm_version_stub()
                return
        if not self.checkout.exists():
            CACHE.mkdir(parents=True, exist_ok=True)
            subprocess.run(
                [
                    "git",
                    "clone",
                    "--depth",
                    "1",
                    "--branch",
                    self.tag,
                    self.repo,
                    str(self.checkout),
                ],
                check=True,
                capture_output=True,
            )
        self._detect_src()
        self._ensure_scm_version_stub()

    def _detect_src(self) -> None:
        """src-layout checkouts import the package from src/."""
        if not self.use_src:
            return
        src = self.checkout / "src"
        if src.is_dir():
            self.src_dir = src

    def _ensure_scm_version_stub(self) -> None:
        """setuptools_scm's `write_to` file is generated at build/install
        time; a bare `git clone` never creates it. Some modules (e.g.
        pytest's own _pytest/assertion/rewrite.py) import it unconditionally,
        so a freshly cloned checkout breaks `import pytest` outright. Stub it
        from the pinned tag so a clone behaves like an installed copy."""
        pyproject = self.checkout / "pyproject.toml"
        if not pyproject.exists():
            return
        config = tomllib.loads(pyproject.read_text())
        write_to = config.get("tool", {}).get("setuptools_scm", {}).get("write_to")
        if not write_to:
            return
        target = self.checkout / write_to
        if target.exists():
            return
        version = self.tag.lstrip("v")
        version_tuple = tuple(int(p) if p.isdigit() else p for p in version.split("."))
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(
            "from __future__ import annotations\n\n"
            f"__version__ = version = {version!r}\n"
            f"__version_tuple__ = version_tuple = {version_tuple!r}\n"
        )

    def test_files(self) -> tuple[list[Path], int]:
        """(files to run, number excluded by configuration)."""
        files: list[Path] = []
        seen: set[Path] = set()

        def add(found):
            for p in sorted(found):
                if p not in seen and p.is_file():
                    seen.add(p)
                    files.append(p)

        # Path-glob patterns (containing "/") are resolved once against the
        # checkout root; bare patterns are matched recursively per testpath.
        path_patterns = [pat for pat in self.python_files if "/" in pat]
        name_patterns = [pat for pat in self.python_files if "/" not in pat]
        for pat in path_patterns:
            add(self.checkout.glob(pat))
        for testpath in self.testpaths:
            base = self.checkout / testpath
            # Flat-layout suites point straight at a test file.
            if base.is_file():
                add([base])
                continue
            for pat in name_patterns:
                add(base.rglob(pat))
        kept = [f for f in files if not any(part in self.exclude for part in f.parts)]
        return kept, len(files) - len(kept)

    def _launch_env(self, rel: str) -> dict[str, str]:
        """PYTHONPATH + per-suite env for running `rel` under pytest-rs."""
        env = dict(os.environ)
        deps_dir = self.deps_dir()
        extra_paths = [str(p) for p in [self.src_dir, deps_dir] if p is not None]
        extra_paths.extend(str((self.checkout / entry).resolve()) for entry in self.pythonpath)
        if extra_paths:
            env["PYTHONPATH"] = ":".join(extra_paths)
        env.update(self.env)
        return env

    @staticmethod
    def _nodeid_in_file(nodeid: str, rel: str) -> bool:
        """True if `nodeid`'s file part (before the first '::') is `rel`."""
        nodeid_file = nodeid.split("::")[0]
        return nodeid_file == rel or rel.endswith("/" + nodeid_file)

    @staticmethod
    def _nodeid_matches(candidate: str, want: str) -> bool:
        """True if `want` (a suites.toml-listed nodeid, written relative to
        `testpaths`) identifies the same test as `candidate` (a full nodeid
        parsed from a FAILED/ERROR summary line, relative to the checkout
        root — which can have an extra path prefix, e.g. a `package`
        install's top-level package dir, same mismatch _nodeid_in_file
        handles for the file part alone). Suffix-matching the WHOLE nodeid
        (not just the file part, unlike --deselect's bare prefix match) is
        safe: colliding would need another file to coincidentally end with
        the same path AND contain a test of the exact same qualified name."""
        return candidate == want or candidate.endswith("/" + want)

    def _deselect_args(self, rel: str) -> list[str]:
        """--deselect args for the node ids pinned out of this file."""
        deselects: list[str] = []
        for nodeid in self.deselect:
            if self._nodeid_in_file(nodeid, rel):
                deselects.extend(("--deselect", nodeid))
        return deselects

    def _known_failures_for(self, rel: str) -> set[str]:
        """Nodeids (as written in suites.toml, relative to testpaths) in
        this file that are pre-approved permanent failures (see
        known_failures docstring above)."""
        return {nodeid for nodeid in self.known_failures if self._nodeid_in_file(nodeid, rel)}

    def _timeout(self, rel: str) -> float:
        return TIMEOUT_S * self.slow_timeout_multiplier if rel in self.slow_files else TIMEOUT_S

    def run_file(self, path: Path) -> FileResult:
        rel = str(path.relative_to(self.checkout))
        cmd = [str(BINARY), rel, *self._deselect_args(rel), *self.extra_args]
        try:
            proc = subprocess.run(
                cmd,
                cwd=self.checkout,
                capture_output=True,
                text=True,
                timeout=self._timeout(rel),
                env=self._launch_env(rel),
            )
        except subprocess.TimeoutExpired:
            return FileResult(file=rel, status="timeout", exit_code=None)

        out = ANSI_RE.sub("", proc.stdout)
        counts = self._parse_summary(out)
        known = self._known_failures_for(rel)
        if known:
            reclassified = 0
            for line in out.splitlines():
                nodeid = _failed_nodeid(line)
                if nodeid is None or not any(self._nodeid_matches(nodeid, k) for k in known):
                    continue
                bucket = "failed" if line.startswith("FAILED ") else "errors"
                counts[bucket] = max(0, counts.get(bucket, 0) - 1)
                reclassified += 1
            if reclassified:
                counts["known_failed"] = reclassified
        if proc.returncode == 0 and counts.get("passed", 0) > 0:
            status = "passed"
        elif proc.returncode == 0:
            status = "no-tests"
        elif proc.returncode == 1 and counts.get("failed", 0) == 0 and counts.get("errors", 0) == 0:
            # All real failures in this file are pre-approved known_failures.
            status = "passed"
        elif proc.returncode == 1:
            status = "failed"
        elif proc.returncode == 5:
            status = "no-tests"
        else:
            status = "error"
        is_unexpected = proc.returncode not in (0, 1, 5)
        return FileResult(
            file=rel,
            status=status,
            exit_code=proc.returncode,
            passed=counts.get("passed", 0),
            failed=counts.get("failed", 0),
            skipped=counts.get("skipped", 0),
            errors=counts.get("errors", 0),
            deselected=counts.get("deselected", 0),
            known_failed=counts.get("known_failed", 0),
            # Keep output for diagnosing pin regressions (failed) and crashes
            # (unexpected exit codes); passing files are dropped to save memory.
            stdout=out if (is_unexpected or status == "failed") else "",
            stderr=proc.stderr if is_unexpected else "",
        )

    def run_one(self, path: Path, node: str | None) -> subprocess.CompletedProcess[str]:
        """Run one file or a single node within it for diagnosis. Env, cwd,
        deselect and extra_args match a normal suite run so the node behaves
        exactly as inside the file. Returns the raw completed process; the
        caller inspects stdout/stderr. Used by `--test`, which is diagnostic
        only and never updates the scoreboard."""
        rel = str(path.relative_to(self.checkout))
        spec = f"{rel}::{node}" if node else rel
        return subprocess.run(
            [str(BINARY), spec, *self._deselect_args(rel), *self.extra_args],
            cwd=self.checkout,
            capture_output=True,
            text=True,
            timeout=self._timeout(rel),
            env=self._launch_env(rel),
        )

    @staticmethod
    def _parse_summary(out: str) -> dict[str, int]:
        for line in reversed(out.splitlines()):
            # Acceptance test summaries can be long enough that pytest-rs
            # emits only "==" (2 equals) instead of "===="; accept 2+.
            if line.startswith("==") and (" in " in line or "no tests ran" in line):
                body = line.strip("= ")
                match = SUMMARY_RE.match(body)
                if match:
                    return {k: int(v) for k, v in match.groupdict().items() if v is not None}
        return {}


def load_suites(only: str | None) -> list[Suite]:
    config = tomllib.loads((ROOT / "conformance" / "suites.toml").read_text())
    suites = [Suite(name, c) for name, c in config.items()]
    if only is not None:
        names = [n.strip() for n in only.split(",")]
        suites = [s for s in suites if s.name in names]
        missing = set(names) - {s.name for s in suites}
        if missing:
            sys.exit(f"unknown suite(s): {', '.join(sorted(missing))}")
        return suites
    return [s for s in suites if s.enabled]


def load_expected(suite: Suite) -> dict[str, str]:
    path = ROOT / "conformance" / "expected" / f"{suite.name}.toml"
    if not path.exists():
        return {}
    data = tomllib.loads(path.read_text())
    return data.get("files", {})


def load_excluded(suite: Suite) -> dict[str, str]:
    """Files intentionally not run, with the reason (e.g. tests of pytest
    internals that pytest-rs deliberately does not replicate)."""
    path = ROOT / "conformance" / "expected" / f"{suite.name}.toml"
    if not path.exists():
        return {}
    data = tomllib.loads(path.read_text())
    return data.get("excluded", {})


def suite_summary(results: list[FileResult], excluded: int) -> str:
    """Aligned stats: tests conformant/total (%), file tally, oddity notes.

    "tests" counts individual test outcomes summed across every file, with
    total = passed + failed + errors + skipped + deselected + known_failed;
    conformant = passed + skipped (reproducing an upstream skip matches
    pytest's behaviour, so it counts toward conformance; deselected and
    known_failed tests count against the total but not toward it — see
    Suite.deselect / Suite.known_failures for the difference between them).
    "files" counts per-file runs (a file is all-pass only when its whole run
    exited cleanly, i.e. any known_failed tests in it were its only
    failures)."""
    passed = sum(r.passed for r in results)
    failed = sum(r.failed for r in results)
    errors = sum(r.errors for r in results)
    skipped = sum(r.skipped for r in results)
    deselected = sum(r.deselected for r in results)
    known_failed = sum(r.known_failed for r in results)
    total = passed + failed + errors + skipped + deselected + known_failed
    conformant = passed + skipped
    pct = f"{conformant / total * 100:5.1f}%" if total else "    -"
    files_ok = sum(1 for r in results if r.status == "passed")
    notes = []
    if skipped:
        notes.append(f"{skipped} skipped")
    if known_failed:
        notes.append(f"{known_failed} known-failed")
    # Files that died before running any test contribute no test counts.
    dead = [
        r.file for r in results if r.status in ("error", "timeout") and r.passed + r.failed == 0
    ]
    if dead:
        notes.append(f"{len(dead)} file{'s' if len(dead) != 1 else ''} died: {', '.join(dead)}")
    if excluded:
        notes.append(f"{excluded} files excluded")
    note = f"  [{'; '.join(notes)}]" if notes else ""
    return (
        f"tests {conformant:>5}/{total:<5} ({pct})   "
        f"files {files_ok:>2}/{len(results):<3} all-pass{note}"
    )


def prepare_suite(suite: Suite, use_local: bool) -> tuple[list[Path], int]:
    """Clone the checkout, scan test files and warm the deps --target
    install (workers must not race for it)."""
    suite.fetch(use_local)
    files, excluded = suite.test_files()
    manifest_excluded = load_excluded(suite)
    skipped_by_manifest = [
        f for f in files if str(f.relative_to(suite.checkout)) in manifest_excluded
    ]
    files = [f for f in files if str(f.relative_to(suite.checkout)) not in manifest_excluded]
    excluded += len(skipped_by_manifest)
    suite.deps_dir()
    return files, excluded


def expected_costs(suite: Suite) -> dict[str, int]:
    """Rough per-file runtime proxy from the committed scoreboard (test
    counts): the global pool submits expensive files first so the slowest
    ones never start at the tail."""
    path = SCOREBOARD / PLATFORM / f"{suite.name}.json"
    if not path.exists():
        path = SCOREBOARD / "linux" / f"{suite.name}.json"
    if not path.exists():
        return {}
    board = json.loads(path.read_text())
    return {
        entry["file"]: entry.get("passed", 0) + entry.get("failed", 0) + entry.get("errors", 0)
        for entry in board.get("files", [])
    }


def run_suites(
    suites: list[Suite], use_local: bool, jobs: int
) -> list[tuple[Suite, list[FileResult], str]]:
    """Run every suite's files through ONE pool (suites used to run
    sequentially: each suite's slowest files idled the other workers).
    Results keep the per-suite deterministic file order."""
    # Clone + deps installs are network-bound; warm them in parallel.
    with ThreadPoolExecutor(max_workers=min(8, max(len(suites), 1))) as pool:
        prepared = list(pool.map(lambda suite: prepare_suite(suite, use_local), suites))

    work: list[tuple[int, int, Suite, Path]] = []
    cost_of: dict[tuple[int, int], int] = {}
    for suite_index, (suite, (files, _)) in enumerate(zip(suites, prepared)):
        costs = expected_costs(suite)
        for file_index, path in enumerate(files):
            work.append((suite_index, file_index, suite, path))
            rel = str(path.relative_to(suite.checkout))
            # Declared-slow files outrank every test-count estimate.
            cost = 10_000_000 if rel in suite.slow_files else costs.get(rel, 0)
            cost_of[(suite_index, file_index)] = cost
    work.sort(key=lambda item: cost_of[(item[0], item[1])], reverse=True)

    slots: list[list[FileResult | None]] = [
        [None] * len(files) for _, (files, _) in zip(suites, prepared)
    ]

    out_by_index: list[tuple[Suite, list[FileResult], str] | None] = [None] * len(suites)
    remaining = [len(files) for _, (files, _) in zip(suites, prepared)]

    def finish_suite(suite_index: int) -> None:
        """Emit (and persist) one suite's results the moment its last file
        lands, so progress streams instead of dumping only at the very end."""
        suite, (_, excluded) = suites[suite_index], prepared[suite_index]
        results = [r for r in slots[suite_index] if r is not None]
        summary = suite_summary(results, excluded)
        print(f"{suite.name} @ {suite.tag}", flush=True)
        print(f"  {summary}", flush=True)

        for result in results:
            if result.status == "error" and (result.stdout or result.stderr):
                print(f"\n  --- error dump: {result.file} (exit {result.exit_code}) ---")
                if result.stdout:
                    print(result.stdout.rstrip())
                if result.stderr:
                    print("  [stderr]")
                    print(result.stderr.rstrip())
                print(f"  --- end: {result.file} ---", flush=True)

        (SCOREBOARD / PLATFORM).mkdir(parents=True, exist_ok=True)
        (SCOREBOARD / PLATFORM / f"{suite.name}.json").write_text(
            json.dumps(
                {
                    "suite": suite.name,
                    "tag": suite.tag,
                    "category": suite.category,
                    "excluded_files": excluded,
                    "deselected": sum(r.deselected for r in results),
                    "known_failed": sum(r.known_failed for r in results),
                    "files": [
                        {k: v for k, v in result.__dict__.items() if k not in ("stdout", "stderr")}
                        for result in results
                    ],
                },
                indent=2,
            )
            + "\n"
        )
        out_by_index[suite_index] = (suite, results, summary)

    with ThreadPoolExecutor(max_workers=jobs) as pool:
        futures = {
            pool.submit(suite.run_file, path): (suite_index, file_index)
            for suite_index, file_index, suite, path in work
        }
        for future in as_completed(futures):
            suite_index, file_index = futures[future]
            slots[suite_index][file_index] = future.result()
            remaining[suite_index] -= 1
            if remaining[suite_index] == 0:
                finish_suite(suite_index)

    # Suites with no collected files never had a future to complete; flush them.
    for suite_index in range(len(suites)):
        if out_by_index[suite_index] is None:
            finish_suite(suite_index)
    return [entry for entry in out_by_index if entry is not None]


def load_scoreboards(suites: list[Suite], platform: str) -> list[dict]:
    boards = []
    for suite in suites:
        path = SCOREBOARD / platform / f"{suite.name}.json"
        if path.exists():
            boards.append(json.loads(path.read_text()))
    return boards


def scoreboard_platforms() -> list[str]:
    """Platforms with committed results, canonical (linux) first."""
    if not SCOREBOARD.is_dir():
        return []
    found = sorted(p.name for p in SCOREBOARD.iterdir() if p.is_dir())
    return sorted(found, key=lambda name: (name != "linux", name))


def cross_suite_tables(boards: list[dict]) -> list[str]:
    """Category-split cross-suite tables (shared by RESULTS.md / README.md):
    the pytest ecosystem (reimplementation targets), then third-party plugins
    loaded as-is through the entry-point shim, then real-world suites run
    wholesale as drop-in evidence."""
    ecosystem = [b for b in boards if b.get("category", "ecosystem") == "ecosystem"]
    third_party = [b for b in boards if b.get("category") == "third-party"]
    real_world = [b for b in boards if b.get("category") == "real-world"]
    lines = ["**pytest & plugin ecosystem** (the APIs pytest-rs reimplements):", ""]
    lines.extend(cross_suite_table(ecosystem))
    if third_party:
        lines.append("")
        lines.append(
            "**Third-party plugins** (not reimplemented — their own upstream test "
            "suites run under pytest-rs, loaded via the `pytest11` entry-point shim):"
        )
        lines.append("")
        lines.extend(cross_suite_table(third_party))
    if real_world:
        lines.append("")
        lines.append("**Real-world projects** (their suites run unchanged, as drop-in evidence):")
        lines.append("")
        lines.extend(cross_suite_table(real_world))
    return lines


def cross_suite_table(boards: list[dict]) -> list[str]:
    """One cross-suite markdown table."""
    lines = [
        "| suite | tag | passed | failed | errors | skipped | deselected | known_failed "
        "| total | conformant % | files all-pass | files run | files excluded |",
        "|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for board in boards:
        files = board["files"]
        passed = sum(f["passed"] for f in files)
        failed = sum(f["failed"] for f in files)
        errors = sum(f["errors"] for f in files)
        skipped = sum(f["skipped"] for f in files)
        deselected = sum(f.get("deselected", 0) for f in files)
        known_failed = sum(f.get("known_failed", 0) for f in files)
        total = passed + failed + errors + skipped + deselected + known_failed
        pct = f"{(passed + skipped) / total * 100:.1f}%" if total else "-"
        files_ok = sum(1 for f in files if f["status"] == "passed")
        lines.append(
            f"| {board['suite']} | {board['tag']} | {passed} | {failed} | {errors} "
            f"| {skipped} | {deselected} | {known_failed} | {total} | {pct} | {files_ok} "
            f"| {len(files)} | {board.get('excluded_files', 0)} |"
        )
    return lines


def render_results_doc(suites: list[Suite]) -> str:
    """RESULTS.md rendered from the committed scoreboard JSONs: per platform,
    the cross-suite table plus per-file detail, with total = passed + failed +
    errors + skipped per file."""
    lines = [
        "# Conformance results",
        "",
        "Auto-generated by `conformance/runner.py` — do not edit by hand. Every run",
        "rewrites the current platform's `conformance/scoreboard/<platform>/*.json`,",
        "this file and the README table. Linux is canonical: CI re-runs the suites",
        "and auto-commits updated results on pushes to main.",
        "",
        "Accounting: `total = passed + failed + errors + skipped + deselected +",
        "known_failed`, summed over the upstream test files of each suite.",
        "`conformant % = (passed + skipped) / total` — reproducing an upstream skip",
        "matches pytest's behaviour, so it counts toward conformance. `deselected`",
        "counts individual tests excluded via `--deselect` before they ever run (see",
        "`deselect` lists in `conformance/suites.toml` — used for tests that are flaky",
        "or environmentally invalid under pytest-rs's execution model). `known_failed`",
        "counts individual tests that DO run but are known, permanent, accepted",
        "non-conformance (see `known_failures` lists in `conformance/suites.toml` —",
        "matched by exact nodeid against the run's own FAILED/ERROR lines, so a test",
        "that starts passing again is simply absorbed into `passed` instead of masking",
        "a real fix). Both count against the total but not toward conformance. `files",
        "excluded` counts whole test files not run at all — these fail under vanilla",
        "pytest too (network access, OS clipboard, etc.) and are out of scope for",
        "pytest-rs; they are not counted in the total.",
        "",
    ]
    for platform in scoreboard_platforms():
        boards = load_scoreboards(suites, platform)
        if not boards:
            continue
        label = " (CI-verified)" if platform == "linux" else " (dev snapshot)"
        lines.append(f"## {platform}{label}")
        lines.append("")
        lines.extend(cross_suite_table(boards))
        lines.append("")
        for board in boards:
            files = board["files"]
            lines.append(f"### {board['suite']} @ {board['tag']}")
            lines.append("")
            lines.append(f"<details><summary>per-file detail ({len(files)} files)</summary>")
            lines.append("")
            lines.append(
                "| file | status | passed | failed | errors | skipped | deselected | known_failed |"
            )
            lines.append("|---|---|---:|---:|---:|---:|---:|---:|")
            for f in files:
                lines.append(
                    f"| {f['file']} | {f['status']} | {f['passed']} | {f['failed']} "
                    f"| {f['errors']} | {f['skipped']} | {f.get('deselected', 0)} "
                    f"| {f.get('known_failed', 0)} |"
                )
            lines.append("")
            lines.append("</details>")
            lines.append("")
    return "\n".join(lines).rstrip() + "\n"


README_DOC = ROOT / "README.md"
README_MARKERS = ("<!-- conformance-results:start -->", "<!-- conformance-results:end -->")


def update_readme_table(suites: list[Suite]) -> None:
    """Splice the canonical platform's cross-suite table between the README's
    marker comments (linux preferred — it is what CI verifies)."""
    platforms = scoreboard_platforms()
    if not platforms:
        return
    platform = platforms[0]
    boards = load_scoreboards(suites, platform)
    start, end = README_MARKERS
    text = README_DOC.read_text()
    if start not in text or end not in text:
        return
    head, rest = text.split(start, 1)
    _, tail = rest.split(end, 1)
    label = "CI-verified" if platform == "linux" else "dev snapshot"
    table = "\n".join([f"_{platform} ({label})_", "", *cross_suite_tables(boards)])
    README_DOC.write_text(f"{head}{start}\n{table}\n{end}{tail}")


def check_suite(suite: Suite, results: list[FileResult]) -> list[str]:
    expected = load_expected(suite)
    violations = []
    by_file = {result.file: result for result in results}
    for file, want in expected.items():
        result = by_file.get(file)
        got = result.status if result is not None else "missing"
        if got != want:
            line = f"{suite.name}: {file}: expected {want}, got {got}"
            # Surface the offending tests so a CI-only regression is
            # actionable without re-running the file by hand (the captured
            # output is kept for "failed" files in run_one). Known, pre-
            # approved failures are excluded so only genuinely new breakage
            # shows up here.
            known = suite._known_failures_for(file)
            failing = [
                row
                for row in (result.stdout if result else "").splitlines()
                for nodeid in [_failed_nodeid(row)]
                if nodeid is not None and not any(suite._nodeid_matches(nodeid, k) for k in known)
            ]
            if failing:
                line += "\n      " + "\n      ".join(failing[:20])
            violations.append(line)
    return violations


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--suite", default=None)
    parser.add_argument("--check", action="store_true")
    parser.add_argument(
        "--local",
        action="store_true",
        help="use sibling checkouts (e.g. ../pytest) instead of cloning pinned tags",
    )
    parser.add_argument(
        "--jobs",
        type=int,
        default=os.cpu_count() or 4,
        help="test files run in parallel (each is its own pytest-rs process)",
    )
    parser.add_argument(
        "--skip-docs",
        action="store_true",
        help="skip RESULTS.md/README regeneration (for parallel CI shards)",
    )
    parser.add_argument(
        "--pinned",
        action="store_true",
        help="only suites that have an expected/<name>.toml pin (the --check gate set)",
    )
    parser.add_argument(
        "--shard",
        default=None,
        metavar="i/n",
        help="run shard i of n: round-robin the selected suites across n shards",
    )
    parser.add_argument(
        "--test",
        default=None,
        metavar="NODEID",
        help="run a single file or node (file::node) under one suite for "
        "diagnosis; requires --suite; prints raw pytest-rs output and skips "
        "the scoreboard/docs",
    )
    args = parser.parse_args()

    if not BINARY.exists():
        sys.exit("build first: cargo build")

    if args.test:
        suites = load_suites(args.suite)
        if len(suites) != 1:
            sys.exit("--test requires exactly one --suite (e.g. --suite pytest-cov)")
        suite = suites[0]
        suite.fetch(args.local)
        file_part, sep, node_part = args.test.partition("::")
        node = node_part if sep else None
        file_path = suite.checkout / file_part
        if not file_path.is_file():
            sys.exit(f"file not found in {suite.name} checkout: {file_part}")
        spec = f"{file_part}::{node}" if node else file_part
        sys.stderr.write(f"[{suite.name}] pytest-rs {spec}\n")
        proc = suite.run_one(file_path, node)
        sys.stdout.write(proc.stdout)
        if proc.stderr:
            sys.stderr.write(proc.stderr)
        return

    suites = load_suites(args.suite)
    if args.pinned:
        suites = [
            s for s in suites if (ROOT / "conformance" / "expected" / f"{s.name}.toml").exists()
        ]
    if args.shard:
        shard_i, shard_n = (int(x) for x in args.shard.split("/"))
        # Greedy bin-pack the suites across n shards by weight (committed test
        # count from the linux scoreboard) so the two heaviest — pytest and anyio
        # — don't land together. Heaviest first, each to the lightest shard.
        # Name-sorted tiebreak keeps the partition deterministic.
        weight = {s.name: max(sum(expected_costs(s).values()), 1) for s in suites}
        buckets: list[list[Suite]] = [[] for _ in range(shard_n)]
        loads = [0] * shard_n
        for s in sorted(suites, key=lambda s: (-weight[s.name], s.name)):
            j = loads.index(min(loads))
            buckets[j].append(s)
            loads[j] += weight[s.name]
        suites = buckets[shard_i - 1]

    violations: list[str] = []
    summaries: list[tuple[str, str]] = []
    for suite, results, summary in run_suites(suites, args.local, args.jobs):
        summaries.append((suite.name, summary))
        if args.check:
            violations.extend(check_suite(suite, results))

    if not args.skip_docs:
        all_suites = load_suites(None)
        RESULTS_DOC.write_text(render_results_doc(all_suites))
        update_readme_table(all_suites)

    if len(summaries) > 1:
        print("\n==== summary " + "=" * 67)
        for name, line in summaries:
            print(f"{name:<17} {line}")

    if violations:
        print("\nconformance regressions:")
        for violation in violations:
            print(f"  {violation}")
        sys.exit(1)
    if args.check:
        print("conformance: OK")


if __name__ == "__main__":
    main()
