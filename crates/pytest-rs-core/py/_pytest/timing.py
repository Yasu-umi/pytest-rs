from datetime import datetime as _dt
from time import perf_counter, sleep, time  # noqa: F401


class Duration:
    """Elapsed time, exposing ``.seconds`` (monotonic) and ``.wall``."""

    def __init__(self, seconds, wall):
        self.seconds = seconds
        self.wall = wall


class Instant:
    """A starting point for measuring elapsed time (upstream _pytest.timing)."""

    def __init__(self):
        self._perf = perf_counter()
        self._time = time()

    def elapsed(self):
        return Duration(perf_counter() - self._perf, time() - self._time)


class MockTiming:
    """Deterministic time mock for timing tests (mirrors real _pytest.timing.MockTiming)."""

    _current_time: float = _dt(2020, 5, 22, 14, 20, 50).timestamp()

    def sleep(self, seconds: float) -> None:
        self._current_time += seconds

    def time(self) -> float:
        return self._current_time

    def perf_counter(self) -> float:
        return self._current_time

    def patch(self, monkeypatch) -> None:
        import _pytest.timing as _timing
        monkeypatch.setattr(_timing, "sleep", self.sleep)
        monkeypatch.setattr(_timing, "time", self.time)
        monkeypatch.setattr(_timing, "perf_counter", self.perf_counter)


del _dt

from _pytest._stub import __getattr__  # noqa: E402, F401
