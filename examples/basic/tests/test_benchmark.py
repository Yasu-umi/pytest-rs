"""Benchmarking: pytest-benchmark is bundled — no extra install needed.

Run with: pytest-rs --benchmark-only
"""


def test_sort_benchmark(benchmark):
    data = list(range(1000, 0, -1))
    result = benchmark(sorted, data)
    assert result == list(range(1, 1001))
