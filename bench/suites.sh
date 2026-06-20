#!/usr/bin/env bash
# End-to-end suite benchmarks behind the README "Performance" table.
#
# Clones a public test suite at a pinned tag into a throwaway directory, sets up
# a venv with the right deps, and times real pytest vs pytest-rs for `--cov` and
# the suite's parallel mode (median of N, real/rs interleaved for a load-robust
# ratio). The checkout is disposable — delete the work dir when done.
#
# Usage:
#   bench/suites.sh <pytest-rs-bin> <work-dir> [suite ...]
#     <pytest-rs-bin>  release binary built against the venv's Python minor
#     <work-dir>       where suites are cloned (rm -rf it afterwards)
#     [suite ...]      subset of: marshmallow click pydantic  (default: all)
#
# Pinned versions match the conformance submodules: the *latest* releases of
# marshmallow/click don't run clean (missing test dep `simplejson`; collection
# errors), so we pin the versions known to pass under pytest-rs at 100%.
set -euo pipefail

RS_BIN="${1:?usage: suites.sh <pytest-rs-bin> <work-dir> [suite ...]}"
WORK="${2:?usage: suites.sh <pytest-rs-bin> <work-dir> [suite ...]}"
shift 2
SUITES=("$@"); [ ${#SUITES[@]} -eq 0 ] && SUITES=(marshmallow click pydantic)
REPS="${REPS:-5}"
PYVER="${PYVER:-3.13}"   # venv Python — MUST match the minor the pytest-rs binary was built against
RS_BIN="$(cd "$(dirname "$RS_BIN")" && pwd)/$(basename "$RS_BIN")"   # absolutize
mkdir -p "$WORK"

# suite spec: repo | tag | cov-target | parallel-flag | extra pip deps
spec() { case "$1" in
  marshmallow) echo "https://github.com/marshmallow-code/marshmallow|4.1.1|marshmallow|-n 3|simplejson==4.1.1" ;;
  click)       echo "https://github.com/pallets/click|8.3.1|click|-n 3|" ;;
  # pydantic runs its suite under pytest-run-parallel (Makefile: --parallel-threads),
  # NOT xdist — plain `-n 3` fails on pydantic with a "different tests collected"
  # error, so we compare both runners under --parallel-threads instead.
  pydantic)    echo "https://github.com/pydantic/pydantic|v2.13.4|pydantic|--parallel-threads 3|hypothesis pytest-run-parallel" ;;
  *) echo "unknown suite: $1" >&2; return 1 ;;
esac; }

median() { printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END{print a[int((NR+1)/2)]}'; }
timed()  { /usr/bin/time -p "$@" >/dev/null 2>/tmp/_st; grep '^real' /tmp/_st | awk '{print $2}'; }

setup() { # name -> prints "dir|cov|pflag"
  local name="$1" dir repo tag cov pflag extra
  IFS='|' read -r repo tag cov pflag extra <<<"$(spec "$name")"
  dir="$WORK/$name"
  if [ ! -d "$dir/.git" ]; then
    git clone --depth 1 --branch "$tag" "$repo" "$dir" >/dev/null 2>&1
  fi
  uv venv "$dir/.venv" --python "$PYVER" >/dev/null 2>&1
  if [ "$name" = "pydantic" ]; then
    # pydantic ships its own pinned pytest + needs the package itself installed
    ( cd "$dir" && uv sync --group dev >/dev/null 2>&1 ) || \
      uv pip install -q --python "$dir/.venv/bin/python" -e "$dir" pytest pytest-cov pytest-xdist $extra >/dev/null 2>&1
  else
    # real baseline = pytest 9.0.3 (the version pytest-rs reproduces)
    uv pip install -q --python "$dir/.venv/bin/python" "pytest==9.0.3" pytest-cov pytest-xdist -e "$dir" $extra >/dev/null 2>&1
  fi
  echo "$dir|$cov|$pflag"
}

echo "suite | mode | pytest | pytest-rs | speedup"
echo "------|------|-------:|---------:|-------"
for name in "${SUITES[@]}"; do
  IFS='|' read -r dir cov pflag <<<"$(setup "$name")"
  PY="$dir/.venv/bin/python"
  cnt=$("$PY" -m pytest "$dir/tests" --co -q -p no:cacheprovider 2>/dev/null | grep -oE '[0-9]+ tests collected' | grep -oE '^[0-9]+' || echo '?')
  for mode in "--cov=$cov" "$pflag --cov=$cov"; do
    "$PY" -m pytest "$dir/tests" $mode -q -p no:cacheprovider -p no:randomly >/dev/null 2>&1 || true   # warmup
    if ! env VIRTUAL_ENV="$dir/.venv" "$RS_BIN" "$dir/tests" $mode -q -p no:cacheprovider -p no:randomly >/dev/null 2>&1; then
      echo "!! pytest-rs failed on $name $mode. Common causes: the binary's embedded Python can't initialize in this venv (rebuild pytest-rs against the same Python the venv's base uses), a Python-minor mismatch (binary vs PYVER=$PYVER), or a missing dep. Skipping." >&2
      continue
    fi
    rp=(); rs=()
    for _ in $(seq "$REPS"); do
      rp+=("$(timed "$PY" -m pytest "$dir/tests" $mode -q -p no:cacheprovider -p no:randomly)")
      rs+=("$(timed env VIRTUAL_ENV="$dir/.venv" "$RS_BIN" "$dir/tests" $mode -q -p no:cacheprovider -p no:randomly)")
    done
    R=$(median "${rp[@]}"); S=$(median "${rs[@]}")
    printf '%s (%s) | `%s` | %ss | %ss | %.1fx\n' "$name" "$cnt" "$mode" "$R" "$S" "$(awk "BEGIN{print $R/$S}")"
  done
done
echo
echo "Done. Delete the checkouts when finished:  rm -rf $WORK"
