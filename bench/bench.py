#!/usr/bin/env python3
"""Wall-clock benchmark harness for pytest-rs vs CPython pytest.

Measures where collection/startup time goes on a real test suite, with
enough config variants to *attribute* the cost rather than just total it:

  rs-collect       pytest-rs --collect-only             (import + rewrite + collect)
  rs-collect-plain pytest-rs --collect-only --assert=plain  (assertion rewrite OFF)
      rs-collect - rs-collect-plain  ==  assertion-rewrite/compile cost
  py-collect-cold  real pytest --collect-only, __pycache__ cleared first
  py-collect-warm  real pytest --collect-only, pyc now cached
      py-cold - py-warm  ==  the rewritten-.pyc cache value pytest-rs forgoes
      py-warm vs rs-collect  ==  warm head-to-head

Each variant runs --reps times; we report the median and min wall-clock plus
the item count parsed from the summary line (a sanity check that both tools
collected the same suite).

Examples:
  # auto-detect the debug binary, measure the whole `tests` tree
  python bench/bench.py tests --py /path/to/.venv/bin/python --venv /path/to/.venv

  # a py3.14 release binary against an external suite in its own venv
  python bench/bench.py tests \\
    --rs-bin target-py314/release/pytest-rs-bin \\
    --cwd /path/to/suite \\
    --venv /path/to/suite/.venv \\
    --py /path/to/suite/.venv/bin/python
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import statistics
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
_COUNT_RE = re.compile(r"(\d+)\s+(?:tests?\s+)?(?:collected|passed|deselected)")


def _detect_rs_bin() -> str | None:
    for candidate in ("target/debug/pytest-rs-bin", "target/release/pytest-rs-bin"):
        path = ROOT / candidate
        if path.exists():
            return str(path)
    return None


def _parse_count(output: str) -> int | None:
    """Pull the collected/passed item count out of pytest's summary line."""
    best = None
    for match in _COUNT_RE.finditer(output):
        best = int(match.group(1))
    return best


def _clear_pycache(root: Path) -> None:
    for cache in root.rglob("__pycache__"):
        shutil.rmtree(cache, ignore_errors=True)


def _run_once(cmd: list[str], cwd: Path, env: dict[str, str]) -> tuple[float, int | None, int]:
    start = time.perf_counter()
    proc = subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True)
    elapsed = time.perf_counter() - start
    out = proc.stdout + proc.stderr
    return elapsed, _parse_count(out), proc.returncode


def _measure(
    label: str,
    cmd: list[str],
    cwd: Path,
    env: dict[str, str],
    reps: int,
    cold_root: Path | None = None,
) -> dict:
    """Run `cmd` `reps` times; if cold_root is set, clear its pyc before each run."""
    times: list[float] = []
    count = None
    rc = 0
    for _ in range(reps):
        if cold_root is not None:
            _clear_pycache(cold_root)
        elapsed, count, rc = _run_once(cmd, cwd, env)
        times.append(elapsed)
    return {
        "label": label,
        "median": statistics.median(times),
        "min": min(times),
        "count": count,
        "rc": rc,
        "raw": times,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("target", nargs="+", help="paths to collect (e.g. tests tests/battery)")
    parser.add_argument("--rs-bin", default=_detect_rs_bin(), help="pytest-rs binary (auto-detected)")
    parser.add_argument("--py", default=sys.executable, help="python that has pytest installed (real pytest)")
    parser.add_argument("--venv", default=None, help="VIRTUAL_ENV for pytest-rs (so it sees site-packages)")
    parser.add_argument("--cwd", default=".", help="working directory to run from")
    parser.add_argument("--reps", type=int, default=5, help="runs per variant (median reported)")
    parser.add_argument("--extra", nargs=argparse.REMAINDER, default=[], help="extra args appended to every command")
    parser.add_argument("--no-pytest", action="store_true", help="skip the real-pytest variants")
    args = parser.parse_args()

    if not args.rs_bin or not Path(args.rs_bin).exists():
        sys.exit(f"pytest-rs binary not found: {args.rs_bin!r} (build it or pass --rs-bin)")
    args.rs_bin = str(Path(args.rs_bin).resolve())  # survives the cwd change below
    cwd = Path(args.cwd).expanduser().resolve()
    cold_root = cwd / args.target[0] if (cwd / args.target[0]).is_dir() else cwd

    base_env = dict(os.environ)
    rs_env = dict(base_env)
    if args.venv:
        rs_env["VIRTUAL_ENV"] = str(Path(args.venv).expanduser().resolve())
        rs_env.pop("PYTHONHOME", None)

    co = ["--collect-only", "-q", *args.extra]
    rs = [args.rs_bin, *args.target]
    py = [args.py, "-m", "pytest", *args.target]

    results = []
    print(f"# target={args.target} cwd={cwd} reps={args.reps}\n", flush=True)
    results.append(_measure("rs-collect", rs + co, cwd, rs_env, args.reps))
    results.append(_measure("rs-collect-plain", rs + co + ["--assert=plain"], cwd, rs_env, args.reps))
    if not args.no_pytest:
        results.append(_measure("py-collect-cold", py + co, cwd, base_env, args.reps, cold_root=cold_root))
        results.append(_measure("py-collect-warm", py + co, cwd, base_env, args.reps))

    by = {r["label"]: r for r in results}
    width = max(len(r["label"]) for r in results)
    print(f"{'variant':<{width}}  {'median':>9}  {'min':>9}  {'items':>7}  rc")
    print("-" * (width + 32))
    for r in results:
        print(f"{r['label']:<{width}}  {r['median']*1000:>7.1f}ms  {r['min']*1000:>7.1f}ms  {str(r['count']):>7}  {r['rc']}")

    print("\n# derived")
    if "rs-collect" in by and "rs-collect-plain" in by:
        delta = by["rs-collect"]["median"] - by["rs-collect-plain"]["median"]
        pct = 100 * delta / by["rs-collect"]["median"] if by["rs-collect"]["median"] else 0
        print(f"assertion-rewrite cost   = {delta*1000:>7.1f}ms  ({pct:.0f}% of rs collection)")
    if "py-collect-cold" in by and "py-collect-warm" in by:
        delta = by["py-collect-cold"]["median"] - by["py-collect-warm"]["median"]
        print(f"real-pytest pyc value     = {delta*1000:>7.1f}ms  (cold - warm; what pytest-rs forgoes)")
    if "py-collect-warm" in by and "rs-collect" in by:
        ratio = by["py-collect-warm"]["median"] / by["rs-collect"]["median"] if by["rs-collect"]["median"] else 0
        faster = "pytest-rs faster" if ratio > 1 else "real pytest faster"
        print(f"head-to-head (warm)       = py-warm/rs-collect = {ratio:.2f}x  ({faster})")


if __name__ == "__main__":
    main()
