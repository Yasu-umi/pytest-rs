"""pytest API shim provided by pytest-rs.

Decorators only *record* metadata on functions (exactly like real pytest);
the Rust engine introspects the imported module afterwards. Nothing here
resolves fixtures or runs tests.
"""

import enum as _enum

from pytest._approx import approx as approx
from pytest._fixtures import FixtureFunctionMarker as FixtureFunctionMarker
from pytest._fixtures import fixture as fixture
from pytest._marks import Mark as Mark
from pytest._marks import MarkDecorator as MarkDecorator
from pytest._marks import MarkGenerator as MarkGenerator
from pytest._marks import ParamSpec as ParamSpec
from pytest._marks import mark as mark
from pytest._marks import param as param
from pytest._monkeypatch import MonkeyPatch as MonkeyPatch
from pytest._monkeypatch import monkeypatch as monkeypatch
from pytest._outcomes import Failed as Failed
from pytest._outcomes import OutcomeException as OutcomeException
from pytest._outcomes import Skipped as Skipped
from pytest._outcomes import XFailed as XFailed
from pytest._outcomes import fail as fail
from pytest._outcomes import importorskip as importorskip
from pytest._outcomes import skip as skip
from pytest._outcomes import xfail as xfail
from pytest._pytester import LineMatcher as LineMatcher
from pytest._pytester import Pytester as Pytester
from pytest._pytester import RunResult as RunResult
from pytest._pytester import pytester as pytester
from pytest._raises import ExceptionInfo as ExceptionInfo
from pytest._raises import RaisesContext as RaisesContext
from pytest._raises import raises as raises
from pytest._tmp_path import tmp_path as tmp_path
from pytest._warns import WarningsRecorder as WarningsRecorder
from pytest._warns import deprecated_call as deprecated_call
from pytest._warns import recwarn as recwarn
from pytest._warns import warns as warns

__version__ = "9.0.3"  # pytest API version this shim tracks
version_tuple = (9, 0, 3)
__pytest_rs__ = True


class UsageError(Exception):
    """Errors in pytest usage or invocation."""


class ExitCode(_enum.IntEnum):
    OK = 0
    TESTS_FAILED = 1
    INTERRUPTED = 2
    INTERNAL_ERROR = 3
    USAGE_ERROR = 4
    NO_TESTS_COLLECTED = 5


def hookimpl(function=None, **kwargs):
    """Record hook implementation options on the function (inert for now:
    conftest hook functions are not yet called by the runner)."""

    def decorator(func):
        func.pytest_impl = dict(kwargs)
        return func

    if function is not None:
        return decorator(function)
    return decorator


def hookspec(function=None, **kwargs):
    def decorator(func):
        func.pytest_spec = dict(kwargs)
        return func

    if function is not None:
        return decorator(function)
    return decorator
