"""Functional demos for inline-snapshot and pytest-run-parallel: each test
exercises the plugin's actual machinery (snapshot comparison; threaded
repeat runs), so a silently-broken autoload fails the smoke run instead of
passing vacuously."""

import threading

from inline_snapshot import snapshot


def test_inline_snapshot_value():
    assert {"x": 1, "y": [2, 3]} == snapshot({"x": 1, "y": [2, 3]})


_hits: set[int] = set()
_lock = threading.Lock()


def test_run_parallel_threads():
    # Under --parallel-threads=2 this body runs once per thread; recording
    # distinct thread idents proves the plugin actually parallelized it.
    with _lock:
        _hits.add(threading.get_ident())


def test_run_parallel_saw_two_threads():
    # Runs after test_run_parallel_threads (collection order); under
    # --parallel-threads=2 two distinct idents must have been recorded.
    assert len(_hits) >= 2
