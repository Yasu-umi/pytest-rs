#!/usr/bin/env bash
# Smoke test: run pytest-rs against the sample suite and assert the expected
# outcome counts (the suite intentionally contains failures).
set -uo pipefail

cd "$(dirname "$0")/../sample"

out=$(../target/debug/pytest-rs tests)
code=$?
echo "$out"

if [ "$code" -ne 1 ]; then
    echo "smoke: expected exit code 1, got $code" >&2
    exit 1
fi
if ! echo "$out" | grep -q "2 failed, 2 passed, 1 skipped"; then
    echo "smoke: expected '2 failed, 2 passed, 1 skipped' in summary" >&2
    exit 1
fi
echo "smoke: OK"
