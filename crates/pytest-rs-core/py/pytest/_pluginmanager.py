"""Minimal config.pluginmanager: getplugin probes for common conftest
checks, plus a pluggy-lite hook relay so autoloaded plugins can register
custom hookspecs (pytest_addhooks) and dispatch them through
``config.pluginmanager.hook.<name>(**kwargs)`` (e.g. pytest-timeout's
pytest_timeout_set_timer). Core plugin loading stays the Rust engine's job."""

from __future__ import annotations

import inspect
from typing import Any


def _accepted_kwargs(func: Any, kwargs: dict[str, Any]) -> dict[str, Any]:
    """pluggy passes each hookimpl only the arguments its signature names."""
    try:
        params = inspect.signature(func).parameters
    except (TypeError, ValueError):
        return kwargs
    return {name: value for name, value in kwargs.items() if name in params}


class HookCaller:
    """One named hook: calls every registered plugin's same-named function
    (LIFO), honoring firstresult from the registered hookspec."""

    def __init__(self, name: str, pm: PluginManager) -> None:
        self._name = name
        self._pm = pm

    def __call__(self, **kwargs: Any) -> Any:
        firstresult = self._pm._specs.get(self._name, {}).get("firstresult", False)
        results = []
        for plugin in reversed(self._pm._plugins):
            func = getattr(plugin, self._name, None)
            if not callable(func):
                continue
            res = func(**_accepted_kwargs(func, kwargs))
            if res is not None:
                if firstresult:
                    return res
                results.append(res)
        return None if firstresult else results


class HookRelay:
    def __init__(self, pm: PluginManager) -> None:
        self._pm = pm

    def __getattr__(self, name: str) -> HookCaller:
        if name.startswith("_"):
            raise AttributeError(name)
        return HookCaller(name, self._pm)


class PluginManager:
    def __init__(self) -> None:
        self._plugins: list[Any] = []
        self._names: dict[str, Any] = {}
        # Core firstresult hookspecs the relay must honor even though no
        # plugin registers them via pytest_addhooks.
        self._specs: dict[str, dict[str, Any]] = {
            "pytest_report_teststatus": {"firstresult": True},
        }
        self.hook = HookRelay(self)

    def getplugin(self, name: str) -> Any:
        if name in self._names:
            return self._names[name]
        if name in ("logging-plugin", "logging"):
            from pytest import _logging

            return _logging.state
        if name == "capturemanager":
            from pytest import _capture

            return _capture.manager
        return None

    get_plugin = getplugin

    def hasplugin(self, name: str) -> bool:
        return self.getplugin(name) is not None

    has_plugin = hasplugin

    def add_hookspecs(self, module_or_class: Any) -> None:
        """Record hookspec options (firstresult) declared via
        @pytest.hookspec on the spec container's functions."""
        for name in dir(module_or_class):
            func = getattr(module_or_class, name, None)
            opts = getattr(func, "pytest_spec", None)
            if name.startswith("pytest_") and isinstance(opts, dict):
                self._specs[name] = opts

    def register(self, plugin: Any, name: str | None = None) -> Any:
        """Track the plugin for hook-relay dispatch; a plugin defining
        pytest_addhooks gets to register its hookspecs immediately (pluggy
        calls it at registration time)."""
        if plugin is None or plugin in self._plugins:
            return None
        self._plugins.append(plugin)
        if name is not None:
            self._names[name] = plugin
        addhooks = getattr(plugin, "pytest_addhooks", None)
        if callable(addhooks):
            addhooks(**_accepted_kwargs(addhooks, {"pluginmanager": self}))
        return plugin

    def unregister(self, plugin: Any = None, name: str | None = None) -> None:
        if plugin is None and name is not None:
            plugin = self._names.get(name)
        if plugin in self._plugins:
            self._plugins.remove(plugin)
        for key in [k for k, v in self._names.items() if v is plugin]:
            del self._names[key]
        return None

    def is_registered(self, plugin: Any) -> bool:
        return plugin in self._plugins


pluginmanager = PluginManager()
