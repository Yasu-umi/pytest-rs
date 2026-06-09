#!/usr/bin/env bash
# Record a MERGED Python + Rust flame graph of a pytest-rs run with py-spy.
#
# pytest-rs is a Rust binary with an embedded CPython (the shim layer), so a
# single profiler that sees both stacks is what we want: py-spy --native walks
# the Python frames AND the native Rust/C frames and folds them into one graph.
#
# Build a symbol-bearing binary first (release is stripped):
#   CARGO_TARGET_DIR=target-prof PYO3_PYTHON=/opt/homebrew/opt/python@3.14/bin/python3.14 \
#     RUSTFLAGS="-C force-frame-pointers=yes" cargo build --profile profiling
#   -> target-prof/profiling/pytest-rs-bin
#
# Usage:
#   bench/flame.sh OUT.svg [VIRTUAL_ENV] -- <binary> <args...>
# Example:
#   bench/flame.sh /tmp/rs.svg /path/to/suite/.venv -- \
#     target-prof/profiling/pytest-rs-bin tests --collect-only -q
#
# macOS note: py-spy must sample another process, which needs root. This script
# re-execs the recording under sudo and preserves VIRTUAL_ENV/PYTHONPATH so the
# embedded interpreter still finds the venv's site-packages.
set -euo pipefail

OUT="${1:?usage: flame.sh OUT.svg [VIRTUAL_ENV] -- <cmd...>}"
shift
VENV=""
if [[ "${1:-}" != "--" ]]; then
  VENV="$1"; shift
fi
[[ "${1:-}" == "--" ]] && shift || { echo "expected -- before the command" >&2; exit 2; }

PYSPY="$(command -v py-spy || true)"
[[ -n "$PYSPY" ]] || { echo "py-spy not found (install: uv tool install py-spy)" >&2; exit 1; }

ENVPREFIX=(env)
[[ -n "$VENV" ]] && ENVPREFIX+=("VIRTUAL_ENV=$VENV")
[[ -n "${PYTHONPATH:-}" ]] && ENVPREFIX+=("PYTHONPATH=$PYTHONPATH")

echo "recording -> $OUT (sudo needed for py-spy on macOS)" >&2
sudo "$PYSPY" record --native --rate 500 -o "$OUT" -- "${ENVPREFIX[@]}" "$@"
echo "wrote $OUT" >&2
