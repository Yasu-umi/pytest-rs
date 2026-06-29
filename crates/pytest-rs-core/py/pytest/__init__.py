"""pytest API shim provided by pytest-rs.

Decorators only *record* metadata on functions (exactly like real pytest);
the Rust engine introspects the imported module afterwards. Nothing here
resolves fixtures or runs tests.
"""

import enum as _enum

from pytest._approx import approx as approx
from pytest._cache import Cache as Cache
from pytest._cache import cache as cache
from pytest._capture import CaptureFixture as CaptureFixture
from pytest._capture import capfd as capfd
from pytest._capture import capfdbinary as capfdbinary
from pytest._capture import capsys as capsys
from pytest._capture import capsysbinary as capsysbinary
from pytest._capture import capteesys as capteesys
from pytest._fixtures import FixtureFunctionMarker as FixtureFunctionMarker
from pytest._fixtures import FixtureLookupError as FixtureLookupError
from pytest._fixtures import fixture as fixture
from pytest._fixtures import yield_fixture as yield_fixture
from pytest._junitxml import record_property as record_property
from pytest._junitxml import record_testsuite_property as record_testsuite_property
from pytest._junitxml import record_xml_attribute as record_xml_attribute
from pytest._logging import LogCaptureFixture as LogCaptureFixture
from pytest._logging import caplog as caplog
from pytest._marks import HIDDEN_PARAM as HIDDEN_PARAM
from pytest._marks import Mark as Mark
from pytest._marks import MarkDecorator as MarkDecorator
from pytest._marks import MarkGenerator as MarkGenerator
from pytest._marks import ParamSpec as ParamSpec
from pytest._marks import mark as mark
from pytest._marks import param as param
from pytest._monkeypatch import MonkeyPatch as MonkeyPatch
from pytest._monkeypatch import monkeypatch as monkeypatch
from pytest._node import Class as Class
from pytest._node import Collector as Collector
from pytest._node import Dir as Dir
from pytest._node import Dir as Directory  # noqa: F401  # pytest.Directory alias
from pytest._node import File as File
from pytest._node import (
    File as Module,  # noqa: F401  # Module is the base class for file collectors
)
from pytest._node import Function as Function
from pytest._node import Item as Item
from pytest._node import Package as Package
from pytest._node import Session as Session
from pytest._outcomes import Exit as Exit
from pytest._outcomes import Failed as Failed
from pytest._outcomes import OutcomeException as OutcomeException
from pytest._outcomes import Skipped as Skipped
from pytest._outcomes import XFailed as XFailed
from pytest._outcomes import exit as exit
from pytest._outcomes import fail as fail
from pytest._outcomes import importorskip as importorskip
from pytest._outcomes import skip as skip
from pytest._outcomes import xfail as xfail
from pytest._parser import Parser as Parser
from pytest._pluginmanager import PluginManager as PluginManager
from pytest._pytester import LineComp as LineComp
from pytest._pytester import LineMatcher as LineMatcher
from pytest._pytester import Pytester as Pytester
from pytest._pytester import RunResult as RunResult
from pytest._pytester import Testdir as Testdir
from pytest._pytester import _config_for_test as _config_for_test
from pytest._pytester import _sys_snapshot as _sys_snapshot
from pytest._pytester import linecomp as linecomp
from pytest._pytester import pytester as pytester
from pytest._pytester import testdir as testdir
from pytest._raises import ExceptionInfo as ExceptionInfo
from pytest._raises import RaisesContext as RaisesContext
from pytest._raises import raises as raises
from pytest._raises_group import RaisesExc as RaisesExc
from pytest._raises_group import RaisesGroup as RaisesGroup
from pytest._rewrite import register_assert_rewrite as register_assert_rewrite
from pytest._stash import Stash as Stash
from pytest._stash import StashKey as StashKey
from pytest._subtests import Subtests as Subtests
from pytest._subtests import subtests as subtests
from pytest._tmp_path import TempPathFactory as TempPathFactory
from pytest._tmp_path import tmp_path as tmp_path
from pytest._tmp_path import tmp_path_factory as tmp_path_factory
from pytest._tmp_path import tmpdir as tmpdir
from pytest._tmp_path import tmpdir_factory as tmpdir_factory
from pytest._warning_types import PytestAssertRewriteWarning as PytestAssertRewriteWarning
from pytest._warning_types import PytestCacheWarning as PytestCacheWarning
from pytest._warning_types import PytestCollectionWarning as PytestCollectionWarning
from pytest._warning_types import PytestConfigWarning as PytestConfigWarning
from pytest._warning_types import PytestDeprecationWarning as PytestDeprecationWarning
from pytest._warning_types import PytestExperimentalApiWarning as PytestExperimentalApiWarning
from pytest._warning_types import PytestFDWarning as PytestFDWarning
from pytest._warning_types import PytestRemovedIn9Warning as PytestRemovedIn9Warning
from pytest._warning_types import PytestRemovedIn10Warning as PytestRemovedIn10Warning
from pytest._warning_types import PytestReturnNotNoneWarning as PytestReturnNotNoneWarning
from pytest._warning_types import (
    PytestUnhandledThreadExceptionWarning as PytestUnhandledThreadExceptionWarning,
)
from pytest._warning_types import PytestUnknownMarkWarning as PytestUnknownMarkWarning
from pytest._warning_types import (
    PytestUnraisableExceptionWarning as PytestUnraisableExceptionWarning,
)
from pytest._warning_types import PytestWarning as PytestWarning
from pytest._warns import WarningsRecorder as WarningsRecorder
from pytest._warns import deprecated_call as deprecated_call
from pytest._warns import recwarn as recwarn
from pytest._warns import warns as warns
from pytest._xdist_fixtures import testrun_uid as testrun_uid
from pytest._xdist_fixtures import worker_id as worker_id

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


def console_main() -> int:
    """In-process invocation entry point (`python -m pytest`). pytest-rs
    runs as its own binary; re-run via the `pytest-rs` executable instead."""
    raise NotImplementedError("pytest.console_main is not supported by pytest-rs")


def main(args=None, plugins=None):
    """Run pytest, returning an integer exit code.

    args: list of CLI arg strings, or a single path-like object, or None
          (defaults to sys.argv[1:]).
    plugins: list of plugin objects or strings (only string plugin names are
             supported; object plugins are silently ignored).
    """
    import os
    import subprocess
    import sys
    from pathlib import Path

    from pytest._pytester import _RUNNER_LIBPATH

    exe = os.environ.get("PYTEST_RS_EXE")
    if exe is None:
        raise RuntimeError("PYTEST_RS_EXE is not set; pytest.main() cannot find the runner")

    if args is None:
        cli = list(sys.argv[1:])
    elif isinstance(args, (str, bytes)):
        raise TypeError(f"expected to be a list of strings, got: {args!r}")
    elif isinstance(args, Path):
        cli = [str(args)]
    else:
        cli = [str(a) for a in args]

    extra = []
    for p in plugins or []:
        if isinstance(p, str):
            extra += ["-p", p]
        # object plugins are not passable to a subprocess; skip them

    env = os.environ.copy()
    for var, val in _RUNNER_LIBPATH.items():
        env.setdefault(var, val)

    proc = subprocess.run(
        [str(exe), *extra, *cli],
        env=env,
    )
    return proc.returncode


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


# Upstream name for the plugin manager (config.pluginmanager's type).
PytestPluginManager = PluginManager

# Report/terminal classes live in the _pytest shadow package; import them last
# to avoid a circular import while pytest's own package is initializing.
from _pytest.reports import CollectReport as CollectReport  # noqa: E402
from _pytest.reports import TestReport as TestReport  # noqa: E402
from _pytest.terminal import TerminalProgressPlugin as TerminalProgressPlugin  # noqa: E402
from _pytest.terminal import TerminalReporter as TerminalReporter  # noqa: E402

#: Public names (upstream curates this list; the public surface here is
#: exactly the non-underscore module globals).
__all__ = sorted(name for name in globals() if not name.startswith("_"))
