"""Conformance harness: run upstream test suites (pytest, pytest-asyncio, ...)
under pytest-rs, file by file, and score the results.

Usage:
    uv run python conformance/runner.py                 # all enabled suites
    uv run python conformance/runner.py --suite pytest  # one suite
    uv run python conformance/runner.py --check         # gate on expected/*.toml
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import tomllib
from concurrent.futures import ThreadPoolExecutor
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
    r"(?:(?P<errors>\d+) errors?)?"
)
ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")


@dataclass
class FileResult:
    file: str
    status: str  # "passed" | "failed" | "error" | "timeout" | "no-tests"
    exit_code: int | None
    passed: int = 0
    failed: int = 0
    skipped: int = 0
    errors: int = 0
    stdout: str = ""
    stderr: str = ""


class Suite:
    def __init__(self, name: str, config: dict):
        self.name = name
        self.repo = config["repo"]
        self.tag = config["tag"]
        self.testpaths = config["testpaths"]
        self.enabled = config.get("enabled", False)
        self.local = config.get("local")
        # Some src-layout packages generate their version module at build time
        # (absent from a git checkout); use_src = false skips the checkout's
        # src/ shadow so the installed dist (with the generated file) is used.
        self.use_src = config.get("use_src", True)
        self.deps: list[str] = config.get("deps", [])
        self.exclude: list[str] = config.get("exclude", [])
        # Node ids never run (flaky under load; they would destabilize the
        # committed results the release gate compares against).
        self.deselect: list[str] = config.get("deselect", [])
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
        self.checkout = CACHE / f"{self.name}-{self.tag}"
        self.src_dir: Path | None = None

    def deps_dir(self) -> Path | None:
        """Install the suite's extra runtime deps into a --target dir, used
        as PYTHONPATH so upstream tests can import them."""
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

    def fetch(self, use_local: bool) -> None:
        if use_local and self.local is not None:
            local = (ROOT / self.local).resolve()
            if local.exists():
                self.checkout = local
                self._detect_src()
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

    def _detect_src(self) -> None:
        """src-layout checkouts import the package from src/."""
        if not self.use_src:
            return
        src = self.checkout / "src"
        if src.is_dir():
            self.src_dir = src

    def test_files(self) -> tuple[list[Path], int]:
        """(files to run, number excluded by configuration)."""
        files: list[Path] = []
        for testpath in self.testpaths:
            base = self.checkout / testpath
            # Flat-layout suites point straight at a test file.
            if base.is_file():
                files.append(base)
                continue
            files.extend(sorted(base.rglob("test_*.py")))
            files.extend(sorted(p for p in base.rglob("*_test.py") if p not in files))
        kept = [f for f in files if not any(part in self.exclude for part in f.parts)]
        return kept, len(files) - len(kept)

    def run_file(self, path: Path) -> FileResult:
        rel = str(path.relative_to(self.checkout))
        deselects = [
            arg
            for nodeid in self.deselect
            if nodeid.split("::")[0] == rel
            for arg in ("--deselect", nodeid)
        ]
        env = dict(os.environ)
        deps_dir = self.deps_dir()
        extra_paths = [str(p) for p in [self.src_dir, deps_dir] if p is not None]
        extra_paths.extend(str((self.checkout / entry).resolve()) for entry in self.pythonpath)
        if extra_paths:
            env["PYTHONPATH"] = ":".join(extra_paths)
        timeout = TIMEOUT_S * 3 if rel in self.slow_files else TIMEOUT_S
        try:
            proc = subprocess.run(
                [str(BINARY), rel, *deselects],
                cwd=self.checkout,
                capture_output=True,
                text=True,
                timeout=timeout,
                env=env,
            )
        except subprocess.TimeoutExpired:
            return FileResult(file=rel, status="timeout", exit_code=None)

        out = ANSI_RE.sub("", proc.stdout)
        counts = self._parse_summary(out)
        if proc.returncode == 0 and counts.get("passed", 0) > 0:
            status = "passed"
        elif proc.returncode == 0:
            status = "no-tests"
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
            stdout=out if is_unexpected else "",
            stderr=proc.stderr if is_unexpected else "",
        )

    @staticmethod
    def _parse_summary(out: str) -> dict[str, int]:
        for line in reversed(out.splitlines()):
            if line.startswith("====") and (" in " in line or "no tests ran" in line):
                body = line.strip("= ")
                match = SUMMARY_RE.match(body)
                if match:
                    return {k: int(v) for k, v in match.groupdict().items() if v is not None}
        return {}


def load_suites(only: str | None) -> list[Suite]:
    config = tomllib.loads((ROOT / "conformance" / "suites.toml").read_text())
    suites = [Suite(name, c) for name, c in config.items()]
    if only is not None:
        suites = [s for s in suites if s.name == only]
        if not suites:
            sys.exit(f"unknown suite: {only}")
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
    """Aligned stats: tests passed/total (%), file tally, oddity notes.

    "tests" counts individual test outcomes summed across every file, with
    total = passed + failed + errors + skipped; "files" counts per-file runs
    (a file is all-pass only when its whole run exited cleanly)."""
    passed = sum(r.passed for r in results)
    failed = sum(r.failed for r in results)
    errors = sum(r.errors for r in results)
    skipped = sum(r.skipped for r in results)
    total = passed + failed + errors + skipped
    pct = f"{passed / total * 100:5.1f}%" if total else "    -"
    files_ok = sum(1 for r in results if r.status == "passed")
    notes = []
    if skipped:
        notes.append(f"{skipped} skipped")
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
        f"tests {passed:>5}/{total:<5} ({pct})   "
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
    with ThreadPoolExecutor(max_workers=jobs) as pool:
        futures = {
            pool.submit(suite.run_file, path): (suite_index, file_index)
            for suite_index, file_index, suite, path in work
        }
        for future in futures:
            suite_index, file_index = futures[future]
            slots[suite_index][file_index] = future.result()

    out: list[tuple[Suite, list[FileResult], str]] = []
    for suite_index, (suite, (_, excluded)) in enumerate(zip(suites, prepared)):
        results = [r for r in slots[suite_index] if r is not None]
        summary = suite_summary(results, excluded)
        print(f"{suite.name} @ {suite.tag}")
        print(f"  {summary}")

        for result in results:
            if result.status == "error" and (result.stdout or result.stderr):
                print(f"\n  --- error dump: {result.file} (exit {result.exit_code}) ---")
                if result.stdout:
                    print(result.stdout.rstrip())
                if result.stderr:
                    print("  [stderr]")
                    print(result.stderr.rstrip())
                print(f"  --- end: {result.file} ---")

        (SCOREBOARD / PLATFORM).mkdir(parents=True, exist_ok=True)
        (SCOREBOARD / PLATFORM / f"{suite.name}.json").write_text(
            json.dumps(
                {
                    "suite": suite.name,
                    "tag": suite.tag,
                    "category": suite.category,
                    "excluded_files": excluded,
                    "files": [
                        {k: v for k, v in result.__dict__.items() if k not in ("stdout", "stderr")}
                        for result in results
                    ],
                },
                indent=2,
            )
            + "\n"
        )
        out.append((suite, results, summary))
    return out


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
    the pytest ecosystem (reimplementation targets), then real-world suites
    run wholesale as drop-in evidence."""
    ecosystem = [b for b in boards if b.get("category", "ecosystem") == "ecosystem"]
    real_world = [b for b in boards if b.get("category") == "real-world"]
    lines = ["**pytest & plugin ecosystem** (the APIs pytest-rs reimplements):", ""]
    lines.extend(cross_suite_table(ecosystem))
    if real_world:
        lines.append("")
        lines.append("**Real-world projects** (their suites run unchanged, as drop-in evidence):")
        lines.append("")
        lines.extend(cross_suite_table(real_world))
    return lines


def cross_suite_table(boards: list[dict]) -> list[str]:
    """One cross-suite markdown table."""
    lines = [
        "| suite | tag | passed | failed | errors | skipped | total | pass % "
        "| files all-pass | files run | files excluded |",
        "|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for board in boards:
        files = board["files"]
        passed = sum(f["passed"] for f in files)
        failed = sum(f["failed"] for f in files)
        errors = sum(f["errors"] for f in files)
        skipped = sum(f["skipped"] for f in files)
        total = passed + failed + errors + skipped
        pct = f"{passed / total * 100:.1f}%" if total else "-"
        files_ok = sum(1 for f in files if f["status"] == "passed")
        lines.append(
            f"| {board['suite']} | {board['tag']} | {passed} | {failed} | {errors} "
            f"| {skipped} | {total} | {pct} | {files_ok} | {len(files)} "
            f"| {board['excluded_files']} |"
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
        "Accounting: `total = passed + failed + errors + skipped`, summed over the",
        "upstream test files of each suite. `skipped` counts tests the upstream",
        "suites explicitly skip when run under pytest-rs. Excluded files (the",
        "per-suite lists in `conformance/expected/*.toml` plus path patterns in",
        "`conformance/suites.toml`) are not run at all and only show up in the",
        '"files excluded" column.',
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
            lines.append("| file | status | passed | failed | errors | skipped |")
            lines.append("|---|---|---:|---:|---:|---:|")
            for f in files:
                lines.append(
                    f"| {f['file']} | {f['status']} | {f['passed']} | {f['failed']} "
                    f"| {f['errors']} | {f['skipped']} |"
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
    actual = {result.file: result.status for result in results}
    for file, want in expected.items():
        got = actual.get(file, "missing")
        if got != want:
            violations.append(f"{suite.name}: {file}: expected {want}, got {got}")
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
    args = parser.parse_args()

    if not BINARY.exists():
        sys.exit("build first: cargo build")

    violations: list[str] = []
    summaries: list[tuple[str, str]] = []
    for suite, results, summary in run_suites(load_suites(args.suite), args.local, args.jobs):
        summaries.append((suite.name, summary))
        if args.check:
            violations.extend(check_suite(suite, results))

    # Rewrite the human-readable results doc and README table from the
    # committed scoreboards plus this run's update. CI auto-commits the
    # refreshed linux results on main; regressions below still hard-fail.
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
