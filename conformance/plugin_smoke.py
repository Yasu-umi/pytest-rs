"""Plugin smoke gate: third-party plugins that run through the `pytest`
shim, verified end-to-end on every CI run.

Two kinds of checks over the demo suite in conformance/plugin-smoke/:

- reporter replacement (pytest-sugar, pytest-pretty): the full terminal
  output must match real pytest byte-for-byte after normalizing timings,
  versions and paths;
- fixture providers (Faker, time-machine, requests-mock), test-order
  control (pytest-randomly), snapshot assertions (inline-snapshot) and
  threaded repeat runs (pytest-run-parallel): functional demos that
  exercise the plugin's fixture/CLI so a silently-broken autoload fails
  instead of passing vacuously.

Usage:
    python conformance/plugin_smoke.py
"""

from __future__ import annotations

import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BINARY = ROOT / "target" / "debug" / "pytest-rs-bin"
DEMO = ROOT / "conformance" / "plugin-smoke"
CACHE = ROOT / ".tmp" / "conformance"
DEPS = [
    # The reporter byte-diff compares against this exact pytest (the shim
    # reports the same version string).
    "pytest==9.0.3",
    "pytest-sugar",
    "pytest-pretty",
    "Faker",
    "time-machine",
    "requests-mock",
    "pytest-randomly",
    "inline-snapshot",
    "pytest-run-parallel",
    "requests",
]
TIMEOUT_S = 120

ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")
NORMALIZERS = [
    (re.compile(r"\d+\.\d+s"), "Xs"),
    (re.compile(r"Python \d+\.\d+\.\d+"), "Python X"),
    (re.compile(r"pluggy-\d+\.\d+(\.\d+)?"), "pluggy-X"),
    (re.compile(r"^plugins: .*$", re.MULTILINE), "plugins: X"),
    (re.compile(r"^rootdir: .*$", re.MULTILINE), "rootdir: X"),
]


def deps_dir() -> Path:
    """Install the smoke deps into a --target dir (PYTHONPATH for both the
    pytest-rs run and the real-pytest reference run)."""
    target = CACHE / "deps" / "plugin-smoke"
    marker = target / ".deps.txt"
    wanted = "\n".join(sorted(DEPS))
    if marker.exists() and marker.read_text() == wanted:
        return target
    subprocess.run(
        ["uv", "pip", "install", "--target", str(target), *DEPS],
        check=True,
        capture_output=True,
    )
    marker.write_text(wanted)
    return target


def smoke_env(deps: Path) -> dict[str, str]:
    env = os.environ.copy()
    env["PYTHONPATH"] = str(deps)
    # Reporter output is width- and color-sensitive: pin both so the
    # byte-diff is stable across terminals and CI.
    env["COLUMNS"] = "80"
    env["NO_COLOR"] = "1"
    env.pop("PY_COLORS", None)
    env.pop("FORCE_COLOR", None)
    env.pop("PYTEST_DISABLE_PLUGIN_AUTOLOAD", None)
    return env


def run(cmd: list[str], cwd: Path, env: dict[str, str]) -> tuple[int, str]:
    proc = subprocess.run(
        cmd,
        cwd=cwd,
        env=env,
        capture_output=True,
        text=True,
        timeout=TIMEOUT_S,
    )
    return proc.returncode, proc.stdout + proc.stderr


def normalize(output: str) -> str:
    output = ANSI_RE.sub("", output)
    for pattern, replacement in NORMALIZERS:
        output = pattern.sub(replacement, output)
    # sugar rewrites its progress line with \r; keep only the final state
    # of each physical line so flush timing differences cannot drift.
    lines = []
    for line in output.split("\n"):
        lines.append(line.rsplit("\r", 1)[-1].rstrip())
    return "\n".join(lines)


def check_reporter_diff(
    name: str, extra_args: list[str], workdir: Path, env: dict[str, str]
) -> list[str]:
    """pytest-rs vs real pytest on the mixed-outcome demo: normalized
    output and exit code must match exactly."""
    args = ["test_outcomes.py", "-p", "no:randomly", *extra_args]
    rs_code, rs_out = run([str(BINARY), *args], workdir, env)
    py_code, py_out = run([sys.executable, "-m", "pytest", *args], workdir, env)
    errors = []
    if rs_code != py_code:
        errors.append(f"{name}: exit code {rs_code} != pytest's {py_code}")
    rs_norm, py_norm = normalize(rs_out), normalize(py_out)
    if rs_norm != py_norm:
        import difflib

        diff = "\n".join(
            difflib.unified_diff(
                py_norm.splitlines(),
                rs_norm.splitlines(),
                fromfile=f"pytest ({name})",
                tofile=f"pytest-rs ({name})",
                lineterm="",
            )
        )
        errors.append(f"{name}: output differs from real pytest\n{diff}")
    return errors


def check_fixture_plugins(workdir: Path, env: dict[str, str]) -> list[str]:
    code, out = run([str(BINARY), "test_fixture_plugins.py", "-p", "no:randomly"], workdir, env)
    if code != 0 or "3 passed" not in out:
        return [f"fixture plugins: expected 3 passed (exit 0), got exit {code}\n{out}"]
    return []


def check_snapshot_parallel(workdir: Path, env: dict[str, str]) -> list[str]:
    """inline-snapshot (snapshot() comparison + its --inline-snapshot flag)
    and pytest-run-parallel (--parallel-threads=2 really runs each test on
    two threads — the demo records thread idents)."""
    code, out = run(
        [
            str(BINARY),
            "test_snapshot_parallel.py",
            "-p",
            "no:randomly",
            "--inline-snapshot=disable",
            "--parallel-threads=2",
        ],
        workdir,
        env,
    )
    if code != 0 or "3 passed" not in out:
        return [f"snapshot/parallel: expected 3 passed (exit 0), got exit {code}\n{out}"]
    return []


def check_randomly(workdir: Path, env: dict[str, str]) -> list[str]:
    """pytest-randomly: the seed header prints, and the same seed yields
    the same (shuffled) collection order on a rerun."""
    errors = []
    code, out = run([str(BINARY), "test_fixture_plugins.py", "--randomly-seed=1234"], workdir, env)
    if code != 0:
        errors.append(f"randomly: run failed (exit {code})\n{out}")
    if "Using --randomly-seed=1234" not in out:
        errors.append(f"randomly: seed header missing\n{out}")
    orders = []
    for _ in range(2):
        code, out = run(
            [str(BINARY), "test_outcomes.py", "--collect-only", "-q", "--randomly-seed=1234"],
            workdir,
            env,
        )
        if code != 0:
            errors.append(f"randomly: collect-only failed (exit {code})\n{out}")
        orders.append([line for line in out.splitlines() if "::" in line])
    if orders[0] != orders[1]:
        errors.append(f"randomly: same seed produced different orders\n{orders[0]}\n{orders[1]}")
    return errors


def main() -> int:
    if not BINARY.exists():
        print(f"missing {BINARY}; run `cargo build` first", file=sys.stderr)
        return 2
    deps = deps_dir()
    env = smoke_env(deps)
    failures: list[str] = []
    with tempfile.TemporaryDirectory(prefix="plugin-smoke-") as tmp:
        workdir = Path(tmp) / "demo"
        shutil.copytree(DEMO, workdir)
        checks = [
            (
                "fixture plugins (Faker, time-machine, requests-mock)",
                lambda: check_fixture_plugins(workdir, env),
            ),
            ("pytest-randomly", lambda: check_randomly(workdir, env)),
            (
                "inline-snapshot + pytest-run-parallel",
                lambda: check_snapshot_parallel(workdir, env),
            ),
            (
                "pytest-pretty (reporter byte-diff)",
                lambda: check_reporter_diff("pretty", ["-p", "no:sugar"], workdir, env),
            ),
            (
                "pytest-sugar (reporter byte-diff)",
                lambda: check_reporter_diff(
                    "sugar", ["--force-sugar", "-p", "no:pytest_pretty"], workdir, env
                ),
            ),
        ]
        for label, check in checks:
            errors = check()
            status = "ok" if not errors else "FAIL"
            print(f"  {label:55} {status}")
            failures.extend(errors)
    if failures:
        print()
        for failure in failures:
            print(failure)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
