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


from _pytest._stub import __getattr__  # noqa: E402, F401
