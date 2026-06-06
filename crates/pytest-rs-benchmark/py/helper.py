"""Benchmark inner loop: one FFI crossing per round.

The round runner is a plain Python for-loop timed with perf_counter, so
per-iteration overhead matches pytest-benchmark's generated runner.
"""

from time import perf_counter


def make_runner(func, args, kwargs):
    if args or kwargs:

        def runner(loops):
            it = range(loops)
            t0 = perf_counter()
            for _ in it:
                func(*args, **kwargs)
            t1 = perf_counter()
            return t1 - t0
    else:

        def runner(loops):
            it = range(loops)
            t0 = perf_counter()
            for _ in it:
                func()
            t1 = perf_counter()
            return t1 - t0

    return runner


def make_result_runner(func, args, kwargs):
    """Like make_runner, but also returns the last call's result
    (pedantic mode must not call the target extra times)."""

    def runner(loops):
        it = range(loops)
        result = None
        t0 = perf_counter()
        for _ in it:
            result = func(*args, **kwargs)
        t1 = perf_counter()
        return t1 - t0, result

    return runner


def timed_call(func, args, kwargs):
    """One timed call, returning (duration, result)."""
    t0 = perf_counter()
    result = func(*args, **kwargs)
    t1 = perf_counter()
    return t1 - t0, result


def resolution():
    """Measured clock resolution (pytest-benchmark's approach)."""
    deltas = []
    for _ in range(10):
        t0 = perf_counter()
        t1 = perf_counter()
        while t1 == t0:
            t1 = perf_counter()
        deltas.append(t1 - t0)
    return min(deltas)
