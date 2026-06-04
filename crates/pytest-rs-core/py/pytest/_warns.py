"""pytest.warns / recwarn: warning capture and assertion."""

import re as _re
import warnings as _warnings

from pytest._fixtures import fixture
from pytest._outcomes import fail


class WarningsRecorder:
    def __init__(self, *, _ispytest=False):
        self._catch = _warnings.catch_warnings(record=True)
        self._entered = False
        self._list = []

    @property
    def list(self):
        return self._list

    def __enter__(self):
        if self._entered:
            raise RuntimeError(f"Cannot enter {self!r} twice")
        self._list = self._catch.__enter__()
        self._entered = True
        _warnings.simplefilter("always")
        return self

    def __exit__(self, exc_type, exc_value, tb):
        if not self._entered:
            raise RuntimeError(f"Cannot exit {self!r} without entering first")
        result = self._catch.__exit__(exc_type, exc_value, tb)
        self._entered = False
        return result

    def __len__(self):
        return len(self._list)

    def __getitem__(self, index):
        return self._list[index]

    def __iter__(self):
        return iter(self._list)

    def pop(self, cls=Warning):
        """Pop the first warning of exact category `cls`, or the first
        subclass match when there is no exact match (pytest behavior)."""
        best_index = None
        for index, w in enumerate(self._list):
            if w.category is cls:
                best_index = index
                break
            if best_index is None and issubclass(w.category, cls):
                best_index = index
        if best_index is not None:
            return self._list.pop(best_index)
        __tracebackhide__ = True
        raise AssertionError(f"{cls!r} not found in warning list")

    def clear(self):
        self._list[:] = []


class WarningsChecker(WarningsRecorder):
    def __init__(self, expected_warning=Warning, match_expr=None, *, _ispytest=False):
        super().__init__(_ispytest=_ispytest)
        self.expected_warning = expected_warning
        self.match_expr = match_expr

    def __exit__(self, exc_type, exc_value, tb):
        __tracebackhide__ = True
        suppressed = super().__exit__(exc_type, exc_value, tb)
        if exc_type is not None:
            return suppressed
        matched = [w for w in self._list if issubclass(w.category, self.expected_warning)]
        if not matched:
            fail(
                f"DID NOT WARN. No warnings of type {self.expected_warning} were emitted.\n"
                f" Emitted warnings: {[w.category.__name__ for w in self._list]}."
            )
        if self.match_expr is not None and not any(
            _re.search(self.match_expr, str(w.message)) for w in matched
        ):
            fail(
                f"DID NOT WARN. No warnings of type {self.expected_warning} "
                f"matching the regex {self.match_expr!r} were emitted.\n"
                f" Emitted warnings: {[str(w.message) for w in matched]}."
            )
        return suppressed


def warns(expected_warning=Warning, *args, match=None, **kwargs):
    if args:
        func, *fargs = args
        with WarningsChecker(expected_warning, match_expr=match):
            return func(*fargs, **kwargs)
    return WarningsChecker(expected_warning, match_expr=match)


def deprecated_call(func=None, *args, **kwargs):
    __tracebackhide__ = True
    expected = (DeprecationWarning, PendingDeprecationWarning, FutureWarning)
    if func is not None:
        with WarningsChecker(expected):
            return func(*args, **kwargs)
    return WarningsChecker(expected, match_expr=kwargs.get("match"))


@fixture
def recwarn():
    with WarningsRecorder(_ispytest=True) as recorder:
        yield recorder
