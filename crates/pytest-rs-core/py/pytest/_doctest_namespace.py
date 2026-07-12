"""The doctest_namespace fixture, kept out of _pytest/doctest.py so a plain
test run never eagerly imports the real stdlib `doctest` module (and its
`pdb` dependency) — that module is only needed when doctests actually run.
"""

from pytest._fixtures import fixture


@fixture(scope="session")
def doctest_namespace() -> dict:
    """Fixture providing a namespace dict injected into all doctests."""
    return {}
