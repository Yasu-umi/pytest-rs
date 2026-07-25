"""The `benchmark` fixture's type, for annotations.

`def test_x(benchmark: BenchmarkFixture)` is how a suite spells the parameter,
so the name has to be importable even though the object a test receives is the
engine's own (a Rust #[pyclass] — see crates/pytest-rs-benchmark/src/fixture.rs).
The running engine replaces the class below with that one, exactly as it does
for `pytest.FixtureRequest`, so `isinstance(benchmark, BenchmarkFixture)` holds
inside a run and the annotation describes the real object rather than a
look-alike.

Only what the engine's fixture actually implements is declared here. Upstream
carries more (`name`, `fullname`, `params`, `cprofile`, ...); those are missing
features, and leaving them undeclared keeps that visible instead of promising a
caller an attribute that would fail at run time.
"""

from collections.abc import Callable, Mapping, Sequence
from typing import Any, NoReturn

from pytest_benchmark.stats import Stats


class FixtureAlreadyUsed(Exception):
    """The benchmark fixture was used twice in one test (upstream's error)."""


class BenchmarkFixture:
    def _outside_run(self, name: str) -> NoReturn:
        raise AttributeError(
            f"{name}: the benchmark fixture is only usable inside a pytest-rs run "
            "(the engine replaces this class with its own implementation at "
            "startup); outside one it exists for annotations and isinstance only"
        )

    def __call__(self, function_to_benchmark: Callable[..., Any], *args: Any, **kwargs: Any) -> Any:
        self._outside_run("__call__")

    def pedantic(
        self,
        target: Callable[..., Any],
        args: Sequence[Any] | None = None,
        kwargs: Mapping[str, Any] | None = None,
        setup: Callable[..., Any] | None = None,
        rounds: int | None = None,
        iterations: int | None = None,
        warmup_rounds: int | None = None,
    ) -> Any:
        self._outside_run("pedantic")

    def weave(self, target: Any, **kwargs: Any) -> None:
        self._outside_run("weave")

    # Upstream's alias for weave.
    patch = weave

    @property
    def stats(self) -> Stats | None:
        self._outside_run("stats")

    @property
    def extra_info(self) -> dict[str, Any]:
        self._outside_run("extra_info")

    @property
    def group(self) -> str | None:
        self._outside_run("group")

    @property
    def enabled(self) -> bool:
        self._outside_run("enabled")

    @property
    def disabled(self) -> bool:
        self._outside_run("disabled")
