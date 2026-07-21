"""pytest.warns / recwarn: warning capture and assertion (ported from
_pytest/recwarn.py)."""

import re as _re
import warnings as _warnings
from pprint import pformat as _pformat
from typing import TYPE_CHECKING, overload

from pytest._fixtures import fixture
from pytest._outcomes import Exit, fail

if TYPE_CHECKING:
    from collections.abc import Callable
    from typing import Self


class WarningsRecorder(_warnings.catch_warnings):
    """A context manager to record raised warnings (adapted from
    `warnings.catch_warnings`)."""

    def __init__(self, *, _ispytest=False):
        super().__init__(record=True)
        self._entered = False
        self._list = []

    @property
    def list(self):
        return self._list

    def __getitem__(self, i):
        return self._list[i]

    def __iter__(self):
        return iter(self._list)

    def __len__(self):
        return len(self._list)

    def pop(self, cls=Warning):
        """Pop the first recorded warning which is an instance of ``cls``,
        but not an instance of a child class of any other match."""
        best_idx = None
        for i, w in enumerate(self._list):
            if w.category == cls:
                return self._list.pop(i)  # exact match, stop looking
            if issubclass(w.category, cls) and (
                best_idx is None or not issubclass(w.category, self._list[best_idx].category)
            ):
                best_idx = i
        if best_idx is not None:
            return self._list.pop(best_idx)
        __tracebackhide__ = True
        raise AssertionError(f"{cls!r} not found in warning list")

    def clear(self):
        self._list[:] = []

    def __enter__(self) -> "Self":  # type: ignore[override]
        if self._entered:
            __tracebackhide__ = True
            raise RuntimeError(f"Cannot enter {self!r} twice")
        _list = super().__enter__()
        self._list = _list
        _warnings.simplefilter("always")
        return self

    def __exit__(self, exc_type, exc_val, exc_tb):
        if not self._entered:
            __tracebackhide__ = True
            raise RuntimeError(f"Cannot exit {self!r} without entering first")
        super().__exit__(exc_type, exc_val, exc_tb)
        # Reset entered state so the context manager is reusable.
        self._entered = False


class WarningsChecker(WarningsRecorder):
    def __init__(self, expected_warning=Warning, match_expr=None, *, _ispytest=False):
        super().__init__(_ispytest=True)

        msg = "exceptions must be derived from Warning, not %s"
        if isinstance(expected_warning, tuple):
            for exc in expected_warning:
                if not issubclass(exc, Warning):
                    raise TypeError(msg % type(exc))
            expected_warning_tup = expected_warning
        elif isinstance(expected_warning, type) and issubclass(expected_warning, Warning):
            expected_warning_tup = (expected_warning,)
        else:
            raise TypeError(msg % type(expected_warning))

        self.expected_warning = expected_warning_tup
        self.match_expr = match_expr

    def matches(self, warning):
        return issubclass(warning.category, self.expected_warning) and bool(
            self.match_expr is None or _re.search(self.match_expr, str(warning.message))
        )

    def __exit__(self, exc_type, exc_val, exc_tb):
        super().__exit__(exc_type, exc_val, exc_tb)

        __tracebackhide__ = True

        # BaseExceptions like pytest.{skip,fail,xfail,exit} or Ctrl-C within
        # pytest.warns should *not* trigger "DID NOT WARN"; control-flow
        # exceptions always propagate.
        if exc_val is not None and (
            not isinstance(exc_val, Exception) or isinstance(exc_val, Exit)
        ):
            return

        def found_str():
            return _pformat([record.message for record in self], indent=2)

        try:
            if not any(issubclass(w.category, self.expected_warning) for w in self):
                fail(
                    f"DID NOT WARN. No warnings of type {self.expected_warning} were emitted.\n"
                    f" Emitted warnings: {found_str()}."
                )
            elif not any(self.matches(w) for w in self):
                fail(
                    f"DID NOT WARN. No warnings of type {self.expected_warning} matching the regex were emitted.\n"
                    f" Regex: {self.match_expr}\n"
                    f" Emitted warnings: {found_str()}."
                )
        finally:
            # Whether or not any warnings matched, re-emit all unmatched
            # warnings (pytest 8.0 behavior).
            for w in self:
                if not self.matches(w):
                    _warnings.warn_explicit(
                        message=w.message,
                        category=w.category,
                        filename=w.filename,
                        lineno=w.lineno,
                        module=w.__module__,
                        source=w.source,
                    )

            # Guard against non-str warning messages (CPython #103577).
            for w in self:
                if type(w.message) is not UserWarning:
                    continue
                if not w.message.args:
                    continue
                msg = w.message.args[0]
                if isinstance(msg, str):
                    continue
                raise TypeError(
                    f"Warning must be str or Warning, got {msg!r} (type {type(msg).__name__})"
                )


@overload
def warns(
    expected_warning: "type[Warning] | tuple[type[Warning], ...]" = ...,
    *,
    match: "str | _re.Pattern[str] | None" = ...,
) -> WarningsChecker: ...


@overload
def warns[T, **P](
    expected_warning: "type[Warning] | tuple[type[Warning], ...]",
    func: "Callable[P, T]",
    *args: "P.args",
    **kwargs: "P.kwargs",
) -> T: ...


def warns(expected_warning=Warning, *args, match=None, **kwargs):
    __tracebackhide__ = True
    if not args:
        if kwargs:
            argnames = ", ".join(sorted(kwargs))
            raise TypeError(
                f"Unexpected keyword arguments passed to pytest.warns: {argnames}"
                "\nUse context-manager form instead?"
            )
        return WarningsChecker(expected_warning, match_expr=match, _ispytest=True)
    func = args[0]
    if not callable(func):
        raise TypeError(f"{func!r} object (type: {type(func)}) must be callable")
    with WarningsChecker(expected_warning, _ispytest=True):
        return func(*args[1:], **kwargs)


@overload
def deprecated_call(*, match: "str | _re.Pattern[str] | None" = ...) -> WarningsRecorder: ...


@overload
def deprecated_call[T, **P](func: "Callable[P, T]", *args: "P.args", **kwargs: "P.kwargs") -> T: ...


def deprecated_call(func=None, *args, **kwargs):
    __tracebackhide__ = True
    if func is not None:
        args = (func, *args)
    return warns(
        (DeprecationWarning, PendingDeprecationWarning, FutureWarning),
        *args,
        **kwargs,
    )


@fixture
def recwarn():
    """Return a :class:`WarningsRecorder` instance that records all warnings emitted by test functions.

    See :ref:`warnings` for information on warning categories.
    """
    wrec = WarningsRecorder(_ispytest=True)
    with wrec:
        _warnings.simplefilter("default")
        yield wrec
