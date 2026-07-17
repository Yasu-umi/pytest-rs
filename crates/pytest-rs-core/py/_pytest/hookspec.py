"""Stub fallback for _pytest.hookspec, but accurate about *which* names are
real pytest hookspecs: some tests (e.g. pytest-xdist's
test_warning_captured_deprecated_in_pytest_6) hasattr()-probe this module to
detect whether a since-removed hookspec still exists in the running pytest
version, and the permissive _stub.py fallback (which answers every attribute
access) would always say yes."""

from pytest._pluginmanager import PluginManager as _PluginManager

from _pytest._stub import _Unsupported


def __getattr__(name):
    if name.startswith("__") or name not in _PluginManager._CORE_HOOKSPEC_NAMES:
        raise AttributeError(name)
    return _Unsupported(name)
