"""The statistics object `benchmark.stats` returns, for annotations.

Like the fixture itself, the engine substitutes its own implementation (the
`PyStats` #[pyclass]); the fields declared here are exactly the ones it exposes.
"""

from typing import NoReturn


class Stats:
    def _outside_run(self, name: str) -> NoReturn:
        raise AttributeError(
            f"{name}: benchmark stats only exist inside a pytest-rs run "
            "(the engine replaces this class with its own implementation at "
            "startup); outside one it exists for annotations and isinstance only"
        )

    @property
    def min(self) -> float:
        self._outside_run("min")

    @property
    def max(self) -> float:
        self._outside_run("max")

    @property
    def mean(self) -> float:
        self._outside_run("mean")

    @property
    def stddev(self) -> float:
        self._outside_run("stddev")

    @property
    def median(self) -> float:
        self._outside_run("median")

    @property
    def iqr(self) -> float:
        self._outside_run("iqr")

    @property
    def q1(self) -> float:
        self._outside_run("q1")

    @property
    def q3(self) -> float:
        self._outside_run("q3")

    @property
    def ops(self) -> float:
        self._outside_run("ops")

    @property
    def total(self) -> float:
        self._outside_run("total")

    @property
    def rounds(self) -> int:
        self._outside_run("rounds")

    @property
    def iterations(self) -> int:
        self._outside_run("iterations")


# Upstream's `benchmark.stats` is a Metadata wrapping a Stats; pytest-rs folds
# the two into one object, so the name is an alias rather than a second class.
Metadata = Stats
