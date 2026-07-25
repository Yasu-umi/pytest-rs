"""pytest-benchmark shim provided by pytest-rs.

The plugin itself is implemented natively (crates/pytest-rs-benchmark); this
package exists so `from pytest_benchmark.fixture import BenchmarkFixture` — the
annotation a benchmark test writes for its fixture parameter — resolves under
mypy and at run time.
"""

from pytest_benchmark.fixture import BenchmarkFixture as BenchmarkFixture
from pytest_benchmark.fixture import FixtureAlreadyUsed as FixtureAlreadyUsed
from pytest_benchmark.stats import Metadata as Metadata
from pytest_benchmark.stats import Stats as Stats

__all__ = ["BenchmarkFixture", "FixtureAlreadyUsed", "Metadata", "Stats"]
