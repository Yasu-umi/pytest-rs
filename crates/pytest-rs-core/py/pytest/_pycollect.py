"""Collection-time warnings for uncollectable members.

Mirrors the PytestCollectionWarning paths in _pytest.python: a Test-named
class that defines __init__/__new__ cannot be instantiated for collection,
and a test-named module member that is callable but not a function (an
instance with __call__) is skipped with a warning.
"""

from __future__ import annotations

import inspect
import types
import warnings
from typing import Any

from _pytest.compat import get_real_func

from pytest._warning_types import PytestCollectionWarning


class _EmptyClass:
    pass


# Builtin attribute names pre-ignored during collection (mirrors
# _pytest.python.IGNORED_ATTRIBUTES): the pycollect/makeitem path is never
# consulted for them, so `python_functions=*` / `python_classes=*` don't try
# to collect (or warn about) dunders on modules, classes and instances.
IGNORED_ATTRIBUTES = frozenset(
    set(dir(types.ModuleType("empty_module")))
    | {"__builtins__", "__file__", "__cached__"}
    | set(dir(_EmptyClass))
    | set(dir(_EmptyClass()))
)


def ignored_attributes() -> list[str]:
    """The IGNORED_ATTRIBUTES set as a list (the Rust collector seeds a set
    from it to skip builtin members before name-pattern matching)."""
    return list(IGNORED_ATTRIBUTES)


def _hasinit(obj: Any) -> bool:
    init = getattr(obj, "__init__", None)
    return bool(init) and init != object.__init__


def _hasnew(obj: Any) -> bool:
    new = getattr(obj, "__new__", None)
    return bool(new) and new != object.__new__


def warn_uncollectable_class(obj: Any, parent_nodeid: str) -> bool:
    """Mirror _pytest.python.Class.collect: warn and return True (skip) when a
    Test-named class defines a custom __init__ or __new__ constructor.

    Classes marked `__test__ = False` (e.g. an imported `_pytest.reports.
    TestReport`) are skipped silently first — pytest never turns them into a
    Class node, so they must not produce a (filterwarnings=error) warning."""
    if not getattr(obj, "__test__", True):
        return True
    name = getattr(obj, "__name__", "")
    if _hasinit(obj):
        warnings.warn(
            PytestCollectionWarning(
                f"cannot collect test class {name!r} because it has a "
                f"__init__ constructor (from: {parent_nodeid})"
            ),
            stacklevel=2,
        )
        return True
    if _hasnew(obj):
        warnings.warn(
            PytestCollectionWarning(
                f"cannot collect test class {name!r} because it has a "
                f"__new__ constructor (from: {parent_nodeid})"
            ),
            stacklevel=2,
        )
        return True
    return False


def warn_uncollectable_function(name: str, obj: Any, filename: str) -> bool:
    """Mirror _pytest.python.pytest_pycollect_makeitem: when a test-named
    member is callable but not a function (an instance with __call__), warn at
    the object's source location and return True (skip)."""
    real = getattr(obj, "__func__", obj)
    if inspect.isfunction(real) or inspect.isfunction(get_real_func(real)):
        return False
    # getfslineno resolves a non-function callable to its __call__ code; the
    # warning's 1-based line equals that code's co_firstlineno.
    call = getattr(type(get_real_func(obj)), "__call__", None)  # noqa: B004
    code = getattr(call, "__code__", None)
    lineno = getattr(code, "co_firstlineno", 1)
    warnings.warn_explicit(
        message=PytestCollectionWarning(
            f"cannot collect {name!r} because it is not a function."
        ),
        category=None,
        filename=str(filename),
        lineno=lineno,
    )
    return True
