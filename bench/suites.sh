#!/usr/bin/env bash
# End-to-end suite benchmarks behind the README "Performance" table.
#
# Clones a public test suite at a pinned tag into a throwaway directory, sets up
# a venv, INSTALLS pytest-rs into it (the way a real user runs it — no external
# binary, no PYTHONHOME, portable across machines/CI), and times real pytest vs
# pytest-rs for `--cov` and the suite's parallel mode (median of N, real/rs
# interleaved for a load-robust ratio). The checkout is disposable.
#
# Usage:
#   bench/suites.sh <work-dir> [suite ...]
#     <work-dir>   where suites are cloned (rm -rf it afterwards)
#     [suite ...]  subset of: marshmallow click networkx  (default: all)
#
# Env:
#   RS_SPEC   what to `uv pip install` for pytest-rs (default: "pytest-rs" from
#             PyPI). For a local dev build, point at the repo: RS_SPEC=/path/to/pytest-rs
#   PYVER     Python for the venv (default: 3.13). Any CPython with a pytest-rs wheel.
#   REPS      timed repetitions per cell (default: 5).
#   WRITE_README  path to a README.md; rewrites its perf table in place between
#             <!-- perf-results:start --> and <!-- perf-results:end -->. Opt-in,
#             because perf is machine-specific (unlike the CI-regenerated
#             conformance table).
#
# Two things that make the numbers valid (learned the hard way):
#   * Pin the versions. The *latest* marshmallow/click don't run clean
#     (marshmallow's suite imports `simplejson`; click's latest has collection
#     errors), so a naive run measures an error/near-empty run. These tags match
#     the conformance submodules and pass under pytest-rs at 100%.
#   * Give `--cov` a SOURCE PATH, not a package name. pytest-rs resolves the
#     `--cov` argument as a path; `--cov=click` (a src-layout package living at
#     src/click) would measure nothing, while real pytest resolves it by import.
#     Passing the in-tree source path (src/click, src/marshmallow, pydantic) makes
#     BOTH runners measure the same files — the apples-to-apples coverage case a
#     developer hits when covering their own repo.
set -euo pipefail

WORK="${1:?usage: suites.sh <work-dir> [suite ...]}"
shift
SUITES=("$@"); [ ${#SUITES[@]} -eq 0 ] && SUITES=(marshmallow click networkx)
RS_SPEC="${RS_SPEC:-pytest-rs}"
PYVER="${PYVER:-3.13}"
REPS="${REPS:-5}"
mkdir -p "$WORK"
PERF_ROWS=""   # accumulated README-format rows (for WRITE_README)

# suite spec: repo | tag | cov-source-path (repo-relative) | parallel-flag | extra deps | testpath (repo-relative, default "tests")
spec() { case "$1" in
  marshmallow) echo "https://github.com/marshmallow-code/marshmallow|4.1.1|src/marshmallow|-n 3|simplejson==4.1.1|" ;;
  click)       echo "https://github.com/pallets/click|8.3.1|src/click|-n 3||" ;;
  networkx)    echo "https://github.com/networkx/networkx|networkx-3.6.1|networkx|-n 3|numpy==2.4.6 scipy==1.18.0 pandas==3.0.3|networkx" ;;
  *) echo "unknown suite: $1" >&2; return 1 ;;
esac; }

median() { printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END{print a[int((NR+1)/2)]}'; }
timed()  { /usr/bin/time -p "$@" >/dev/null 2>/tmp/_st; grep '^real' /tmp/_st | awk '{print $2}'; }

echo "RS_SPEC=$RS_SPEC  PYVER=$PYVER  REPS=$REPS"
echo
echo "suite (tests) | mode | pytest | pytest-rs | speedup"
echo "------|------|-------:|---------:|-------"
for name in "${SUITES[@]}"; do
  IFS='|' read -r repo tag cov pflag extra tp <<<"$(spec "$name")"
  tp="${tp:-tests}"
  dir="$WORK/$name"
  [ -d "$dir/.git" ] || git clone --depth 1 --branch "$tag" "$repo" "$dir" >/dev/null 2>&1
  uv venv "$dir/.venv" --python "$PYVER" >/dev/null 2>&1
  # Install pytest-rs the same way a user does, into the venv, so it self-locates.
  # Editable install keeps the package source in-tree (so the cov path resolves).
  # pytest 9.0.3 = the version pytest-rs reproduces; xdist for real's `-n` baseline.
  uv pip install -q --python "$dir/.venv/bin/python" "pytest==9.0.3" pytest-cov pytest-xdist "$RS_SPEC" -e "$dir" $extra >/dev/null 2>&1
  PY="$dir/.venv/bin/python"; RS="$dir/.venv/bin/pytest-rs"
  CFLAGS="-q -p no:cacheprovider -p no:randomly"
  cnt=$( { "$PY" -m pytest "$dir/$tp" --co -q -p no:cacheprovider 2>/dev/null || true; } | grep -oE '[0-9]+ tests collected' | head -1 | grep -oE '^[0-9]+' )
  [ -n "$cnt" ] || cnt='?'

  # median real/rs for one mode (interleaved), printed as a table row.
  measure() { # $1 = pytest args, $2 = display label
    local args="$1" label="$2"
    ( cd "$dir" && "$PY" -m pytest $tp $args $CFLAGS >/dev/null 2>&1 ) || true  # warmup
    ( cd "$dir" && "$RS" $tp $args $CFLAGS >/dev/null 2>&1 ) || true
    local rp=() rs=() _ R S
    for _ in $(seq "$REPS"); do
      rp+=("$(cd "$dir" && timed "$PY" -m pytest $tp $args $CFLAGS)")
      rs+=("$(cd "$dir" && timed "$RS" $tp $args $CFLAGS)")
    done
    R=$(median "${rp[@]}"); S=$(median "${rs[@]}")
    local sp; sp=$(awk "BEGIN{printf \"%.1f\", $R/$S}")
    printf '%s (%s) | `%s` | %ss | %ss | %sx\n' "$name" "$cnt" "$label" "$R" "$S" "$sp"
    PERF_ROWS+="| $name ($cnt) | \`$label\` | $R s | $S s | **${sp}x** |"$'\n'
  }

  # plain run (no coverage, no parallel) — test bodies dominate, so this is the
  # floor where both runners are closest.
  measure "" "(plain)"

  # coverage modes — only if pytest-rs actually resolves & measures the source.
  # `|| true`: a failing test (non-zero exit) must not abort the sanity probe.
  stmts=$( { cd "$dir" && "$RS" $tp --cov="$cov" $CFLAGS 2>/dev/null | awk '/^TOTAL/{print $2}'; } || true )
  if [ -z "${stmts:-}" ] || [ "${stmts:-0}" = 0 ]; then
    echo "$name ($cnt) | -- | -- | -- | pytest-rs measured 0 statements for cov path '$cov' — skipping cov modes (check layout)" >&2
    continue
  fi
  measure "--cov=$cov" "--cov"
  measure "$pflag --cov=$cov" "$pflag --cov"
done
echo

# Optionally rewrite the README perf table in place, between the markers
#   <!-- perf-results:start --> ... <!-- perf-results:end -->
# (perf is machine-specific, so this is opt-in — not run in CI like the
# conformance table.) Set WRITE_README=/path/to/README.md.
if [ -n "${WRITE_README:-}" ]; then
  if ! grep -q '<!-- perf-results:start -->' "$WRITE_README" 2>/dev/null; then
    echo "!! $WRITE_README has no <!-- perf-results:start --> marker — not writing." >&2
  else
    # Build the replacement block in a file (awk -v can't carry newlines).
    block="$WORK/.perf-block"
    { echo "| suite (tests) | mode | pytest | pytest-rs | speedup |"
      echo "|---|---|---:|---:|---|"
      printf '%s' "$PERF_ROWS"; } > "$block"
    awk -v bf="$block" '
      /<!-- perf-results:start -->/{print; while ((getline l < bf) > 0) print l; close(bf); skip=1; next}
      /<!-- perf-results:end -->/{skip=0}
      !skip{print}
    ' "$WRITE_README" > "$WRITE_README.tmp" && mv "$WRITE_README.tmp" "$WRITE_README"
    rm -f "$block"
    echo "Updated perf table in $WRITE_README"
  fi
fi

echo "Done. Delete the checkouts when finished:  rm -rf $WORK"
