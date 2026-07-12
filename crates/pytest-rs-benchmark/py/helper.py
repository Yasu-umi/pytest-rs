"""Benchmark inner loop: one FFI crossing per round.

The round runner is a plain Python for-loop timed with the benchmark
timer (perf_counter unless a test injects `benchmark._timer`), so
per-iteration overhead matches pytest-benchmark's generated runner.
"""

import cProfile
import gc
import importlib
import pstats
import time
from time import perf_counter

import pytest


class PytestBenchmarkWarning(pytest.PytestWarning):
    """Warning emitted by pytest-benchmark."""


def make_runner(func, args, kwargs, timer=None, disable_gc=False):
    timer = timer or perf_counter
    if args or kwargs:

        def timed(loops):
            it = range(loops)
            t0 = timer()
            for _ in it:
                func(*args, **kwargs)
            t1 = timer()
            return t1 - t0
    else:

        def timed(loops):
            it = range(loops)
            t0 = timer()
            for _ in it:
                func()
            t1 = timer()
            return t1 - t0

    if not disable_gc:
        return timed

    def runner(loops):
        gc_enabled = gc.isenabled()
        gc.disable()
        try:
            return timed(loops)
        finally:
            if gc_enabled:
                gc.enable()

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
    return time.time()


def _stderr_writeorg(msg):
    """Write MSG to the real process stderr, bypassing pytest's fd capture.

    During test execution pytest redirects fd 2 to a capture buffer. The
    saved original fd is stored in the FDCapture object; writing there
    produces output that reaches the subprocess's outer stderr pipe (and
    therefore `result.stderr` in pytester).
    """
    import os

    fd = None
    try:
        from pytest._capture import state as _cap_state

        cap = _cap_state._capture
        if cap is not None and cap.err is not None and hasattr(cap.err, "targetfd_save"):
            fd = cap.err.targetfd_save
    except Exception:
        pass
    data = (msg + "\n").encode("utf-8")
    if fd is not None:
        os.write(fd, data)
    else:
        import sys

        sys.stderr.buffer.write(data)
        sys.stderr.flush()


def cprofile_call(func, args, kwargs, loops=1):
    """Invocations under cProfile (upstream profiles loops_range calls
    after the timed rounds); returns (the last call's result, per-function
    profile rows for the --benchmark-cprofile report section)."""
    profile = cProfile.Profile()
    result = None
    for _ in range(max(loops, 1)):
        result = profile.runcall(func, *args, **kwargs)
    return result, _cprofile_functions(profile)


def _cprofile_functions(profile):
    """Port of pytest_benchmark.utils.get_cprofile_functions: pstats -> a
    list of per-function dicts (ncalls_recursion/ncalls/tottime/tottime_per/
    cumtime/cumtime_per/function_name), matching the --benchmark-cprofile
    sort column choices."""
    import os

    stats = pstats.Stats(profile)
    project_dir_parent = os.path.dirname(os.getcwd())
    result = []
    for (file_path, lineno, func_name), run_info in stats.stats.items():
        if file_path.startswith(project_dir_parent):
            file_path = file_path[len(project_dir_parent) :].lstrip("/")
        pcalls, ncalls, tottime, cumtime = run_info[:4]
        ncalls_recursion = str(pcalls) if pcalls == ncalls else f"{ncalls}/{pcalls}"
        result.append(
            {
                "ncalls_recursion": ncalls_recursion,
                "ncalls": ncalls,
                "tottime": tottime,
                "tottime_per": tottime / pcalls if pcalls else 0.0,
                "cumtime": cumtime,
                "cumtime_per": cumtime / pcalls if pcalls else 0.0,
                "function_name": f"{file_path}:{lineno}({func_name})",
            }
        )
    return result


class FixtureAlreadyUsed(Exception):
    """The benchmark fixture ran already in this test (upstream's
    pytest_benchmark.fixture.FixtureAlreadyUsed)."""


def weave(benchmark, target, kwargs):
    """benchmark.weave/patch (aspect mode): weaves a call through
    `benchmark(function, ...)` into `target` via aspectlib. Returns the
    rollback callable (upstream's `aspectlib.weave(...).rollback`)."""
    try:
        import aspectlib
    except ImportError as exc:
        raise ImportError(exc.args, "Please install aspectlib or pytest-benchmark[aspect]") from exc

    def aspect(function):
        def wrapper(*args, **kwargs):
            return benchmark(function, *args, **kwargs)

        return wrapper

    return aspectlib.weave(target, aspect, **kwargs).rollback


def _load_timer(spec):
    """Resolve --benchmark-timer's dotted spec into a callable, honoring
    the historical `pep418` module alias for `time` (PEP 418 added the
    perf_counter/monotonic clocks to it; upstream's load_timer special-cases
    it since `time` predates that PEP)."""
    module_name, _, attr = spec.rpartition(".")
    if not module_name:
        raise ValueError(f"Value for --benchmark-timer must be in dotted form. Got: {spec!r}")
    if module_name == "pep418":
        module_name = "time"
    return getattr(importlib.import_module(module_name), attr)


def resolve_timer(spec):
    """--benchmark-timer=module.attr (upstream's NameWrapper-ed dotted
    lookup, e.g. time.time or time.perf_counter)."""
    return _load_timer(spec)


def timer_display_name(spec=None):
    """The `timer=...` value shown in the benchmark report header
    (upstream's `str(NameWrapper(timer))`: `<target.__module__>.<target.__name__>`)."""
    target = _load_timer(spec) if spec is not None else perf_counter
    module = getattr(target, "__module__", "")
    name = getattr(target, "__name__", repr(target))
    return f"{module}.{name}" if module else name
