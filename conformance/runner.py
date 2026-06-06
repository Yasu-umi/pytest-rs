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
import re
import subprocess
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CACHE = ROOT / ".tmp" / "conformance"
BINARY = ROOT / "target" / "debug" / "pytest-rs"
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
        self.deps: list[str] = config.get("deps", [])
        self.exclude: list[str] = config.get("exclude", [])
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
                src = local / "src"
                if src.is_dir():
                    self.src_dir = src
                return
        if self.checkout.exists():
            return
        CACHE.mkdir(exist_ok=True)
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

    def test_files(self) -> tuple[list[Path], int]:
        """(files to run, number excluded by configuration)."""
        files: list[Path] = []
        for testpath in self.testpaths:
            base = self.checkout / testpath
            files.extend(sorted(base.rglob("test_*.py")))
            files.extend(sorted(p for p in base.rglob("*_test.py") if p not in files))
        kept = [f for f in files if not any(part in self.exclude for part in f.parts)]
        return kept, len(files) - len(kept)

    def run_file(self, path: Path) -> FileResult:
        import os

        rel = str(path.relative_to(self.checkout))
        env = dict(os.environ)
        deps_dir = self.deps_dir()
        extra_paths = [str(p) for p in [self.src_dir, deps_dir] if p is not None]
        if extra_paths:
            env["PYTHONPATH"] = ":".join(extra_paths)
        try:
            proc = subprocess.run(
                [str(BINARY), rel],
                cwd=self.checkout,
                capture_output=True,
                text=True,
                timeout=TIMEOUT_S,
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
    """Aligned stats: tests passed/graded (%), file tally, oddity notes.

    "tests" counts individual test outcomes summed across every file;
    "files" counts per-file runs (a file is all-pass only when its whole
    run exited cleanly)."""
    passed = sum(r.passed for r in results)
    failed = sum(r.failed for r in results)
    errors = sum(r.errors for r in results)
    skipped = sum(r.skipped for r in results)
    graded = passed + failed + errors
    pct = f"{passed / graded * 100:5.1f}%" if graded else "    -"
    files_ok = sum(1 for r in results if r.status == "passed")
    notes = []
    if skipped:
        notes.append(f"{skipped} skipped")
    # Files that died before running any test contribute no test counts.
    dead = [r.file for r in results if r.status in ("error", "timeout") and r.passed + r.failed == 0]
    if dead:
        notes.append(f"{len(dead)} file{'s' if len(dead) != 1 else ''} died: {', '.join(dead)}")
    if excluded:
        notes.append(f"{excluded} files excluded")
    note = f"  [{'; '.join(notes)}]" if notes else ""
    return (
        f"tests {passed:>5}/{graded:<5} ({pct})   "
        f"files {files_ok:>2}/{len(results):<3} all-pass{note}"
    )


def run_suite(suite: Suite, use_local: bool) -> tuple[list[FileResult], str]:
    suite.fetch(use_local)
    files, excluded = suite.test_files()
    manifest_excluded = load_excluded(suite)
    skipped_by_manifest = [
        f for f in files if str(f.relative_to(suite.checkout)) in manifest_excluded
    ]
    files = [f for f in files if str(f.relative_to(suite.checkout)) not in manifest_excluded]
    excluded += len(skipped_by_manifest)
    results = [suite.run_file(path) for path in files]

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

    scoreboard = ROOT / "conformance" / "scoreboard"
    scoreboard.mkdir(exist_ok=True)
    (scoreboard / f"{suite.name}.json").write_text(
        json.dumps(
            [{k: v for k, v in result.__dict__.items() if k not in ("stdout", "stderr")} for result in results],
            indent=2,
        )
        + "\n"
    )
    return results, summary


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
    args = parser.parse_args()

    if not BINARY.exists():
        sys.exit("build first: cargo build")

    violations: list[str] = []
    summaries: list[tuple[str, str]] = []
    for suite in load_suites(args.suite):
        results, summary = run_suite(suite, args.local)
        summaries.append((suite.name, summary))
        if args.check:
            violations.extend(check_suite(suite, results))

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
