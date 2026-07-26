"""The warning category pytest-benchmark emits under.

Upstream's `Logger` is not reimplemented here: pytest-rs drives the benchmark
plugin natively (crates/pytest-rs-benchmark), which writes its own terminal
output and raises this category through `warnings.warn_explicit`. What the
module must provide is the *class*, because a `filterwarnings` entry that names
a category by import path — `ignore::pytest_benchmark.logger.PytestBenchmarkWarning`
— is resolved to a class object and matched by identity, so the warning pytest-rs
emits has to be an instance of the very class such a filter imports.
"""

import pytest


class PytestBenchmarkWarning(pytest.PytestWarning):
    """Warning emitted by pytest-benchmark."""


__all__ = ["PytestBenchmarkWarning"]
