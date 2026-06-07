"""Benchmark inner loop: one FFI crossing per round.

The round runner is a plain Python for-loop timed with the benchmark
timer (perf_counter unless a test injects `benchmark._timer`), so
per-iteration overhead matches pytest-benchmark's generated runner.
"""

from time import perf_counter


def make_runner(func, args, kwargs, timer=None):
    timer = timer or perf_counter
    if args or kwargs:

        def runner(loops):
            it = range(loops)
            t0 = timer()
            for _ in it:
                func(*args, **kwargs)
            t1 = timer()
            return t1 - t0
    else:

        def runner(loops):
            it = range(loops)
            t0 = timer()
            for _ in it:
                func()
            t1 = timer()
            return t1 - t0

    return runner


def make_result_runner(func, args, kwargs, timer=None):
    """Like make_runner, but also returns the last call's result
    (pedantic mode must not call the target extra times)."""
    timer = timer or perf_counter

    def runner(loops):
        it = range(loops)
        result = None
        t0 = timer()
        for _ in it:
            result = func(*args, **kwargs)
        t1 = timer()
        return t1 - t0, result

    return runner


def timed_call(func, args, kwargs, timer=None):
    """One timed call, returning (duration, result)."""
    timer = timer or perf_counter
    t0 = timer()
    result = func(*args, **kwargs)
    t1 = timer()
    return t1 - t0, result


def resolution(timer=None):
    """Measured clock resolution (pytest-benchmark's approach)."""
    timer = timer or perf_counter
    deltas = []
    for _ in range(10):
        t0 = timer()
        t1 = timer()
        while t1 == t0:
            t1 = timer()
        deltas.append(t1 - t0)
    return min(deltas)


def wall_clock():
    """Real elapsed-time probe for the calibration warmup budget
    (upstream uses time.time even with an injected benchmark timer)."""
    import time

    return time.time()


def cprofile_call(func, args, kwargs, loops=1):
    """Invocations under cProfile (upstream profiles loops_range calls
    after the timed rounds); returns the last call's result."""
    import cProfile

    profile = cProfile.Profile()
    result = None
    for _ in range(max(loops, 1)):
        result = profile.runcall(func, *args, **kwargs)
    return result
