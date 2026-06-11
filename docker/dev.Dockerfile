# CI-parity dev image: linux + Python 3.13 + Rust stable + uv.
#
# Lets you reproduce the linux/py3.13-only conformance and plugin-smoke results
# locally (some reporter byte-diffs and pin checks diverge from macOS). Mirrors
# the GitHub Actions `rust (python 3.13)` job: ubuntu-class base, Python 3.13
# with a shared libpython, Rust stable with rustfmt+clippy, and uv for the
# conformance runner's suite-dep installs.
#
# Usage (see docker/test-linux.sh):
#   docker build -t pytest-rs-dev -f docker/dev.Dockerfile .
#   docker run --rm -v "$PWD":/workspace \
#       -v pytest-rs-linux-target:/workspace/target \
#       -v pytest-rs-linux-tmp:/workspace/.tmp \
#       pytest-rs-dev bash -lc "cargo build && python conformance/plugin_smoke.py"
FROM python:3.13-bookworm

RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential curl git ca-certificates pkg-config \
    && rm -rf /var/lib/apt/lists/*

# Rust stable with the components CI runs (cargo fmt --check, clippy).
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --default-toolchain stable --component rustfmt --component clippy
ENV PATH="/root/.cargo/bin:${PATH}"

# uv: the conformance runner installs each suite's deps via `uv pip install`.
RUN curl -LsSf https://astral.sh/uv/install.sh | sh
ENV PATH="/root/.local/bin:${PATH}"

# The pytest-rs binary embeds CPython and links libpython at runtime.
ENV LD_LIBRARY_PATH="/usr/local/lib" \
    PYO3_PYTHON="/usr/local/bin/python3.13" \
    PYTHONUNBUFFERED=1
WORKDIR /workspace
