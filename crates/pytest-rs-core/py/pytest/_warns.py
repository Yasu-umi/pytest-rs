"""pytest.warns / recwarn: warning capture and assertion."""

import re as _re
import warnings as _warnings

from pytest._fixtures import fixture
from pytest._outcomes import fail


class WarningsRecorder:
    def __init__(self):
        self._catch = _warnings.catch_warnings(record=True)
        self._list = []

    @property
    def list(self):
        return self._list

    def __enter__(self):
        self._list = self._catch.__enter__()
        _warnings.simplefilter("always")
        return self

    def __exit__(self, exc_type, exc_value, tb):
        return self._catch.__exit__(exc_type, exc_value, tb)

    def __len__(self):
        return len(self._list)

    def __getitem__(self, index):
        return self._list[index]

    def __iter__(self):
        return iter(self._list)

    def pop(self, category=Warning):
        for index, w in enumerate(self._list):
            if issubclass(w.category, category):
                return self._list.pop(index)
        raise AssertionError(f"{category!r} not found in warning list")

    def clear(self):
        self._list.clear()


class WarningsChecker(WarningsRecorder):
    def __init__(self, expected_warning=Warning, match=None):
        super().__init__()
        self.expected_warning = expected_warning
        self.match_expr = match

    def __exit__(self, exc_type, exc_value, tb):
        suppressed = super().__exit__(exc_type, exc_value, tb)
        if exc_type is not None:
            return suppressed
        matched = [w for w in self._list if issubclass(w.category, self.expected_warning)]
        if not matched:
            fail(f"DID NOT WARN. No warnings of type {self.expected_warning} were emitted.")
        if self.match_expr is not None and not any(
            _re.search(self.match_expr, str(w.message)) for w in matched
        ):
            fail(
                f"DID NOT WARN. No warnings of type {self.expected_warning} "
                f"matching the regex {self.match_expr!r} were emitted."
            )
        return suppressed


def warns(expected_warning=Warning, *args, match=None, **kwargs):
    if args:
        func, *fargs = args
        with WarningsChecker(expected_warning) as checker:
            func(*fargs, **kwargs)
        return checker
    return WarningsChecker(expected_warning, match=match)


def deprecated_call(*args, **kwargs):
    return warns((DeprecationWarning, PendingDeprecationWarning), *args, **kwargs)


@fixture
def recwarn():
    with WarningsRecorder() as recorder:
        yield recorder
