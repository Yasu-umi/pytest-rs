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
        kwargs = self._fix_path_args(kwargs)
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

    def _fix_path_args(self, kwargs: dict[str, Any]) -> dict[str, Any]:
        """Upstream PathAwareHookProxy: hooks with py.path arguments accept
        either form, deprecation-warn on the legacy one, and require both
        to agree when given together."""
        pair = PluginManager._LEGACY_PATH_HOOK_ARGS.get(self._name)
        if pair is None:
            return kwargs
        fspath_var, path_var = pair
        if fspath_var not in kwargs and path_var not in kwargs:
            return kwargs
        import pathlib
        import warnings

        from _pytest.deprecated import HOOK_LEGACY_PATH_ARG

        kwargs = dict(kwargs)
        path_value = kwargs.pop(path_var, None)
        fspath_value = kwargs.pop(fspath_var, None)
        if fspath_value is not None:
            warnings.warn(
                HOOK_LEGACY_PATH_ARG.format(pylib_path_arg=fspath_var, pathlib_path_arg=path_var),
                stacklevel=3,
            )
        if path_value is not None:
            if fspath_value is not None and pathlib.Path(fspath_value) != path_value:
                raise ValueError(
                    f"Path({fspath_value!r}) != {path_value!r}\n"
                    "path and fspath args need to be equal"
                )
            from pytest._tmp_path import LocalPath

            fspath_value = LocalPath(path_value)
        else:
            path_value = pathlib.Path(fspath_value)
        kwargs[path_var] = path_value
        kwargs[fspath_var] = fspath_value
        return kwargs


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

    _LEGACY_HOOK_ATTRS = (
        "tryfirst",
        "trylast",
        "optionalhook",
        "hookwrapper",
        "wrapper",
        "firstresult",
        "historic",
    )

    @classmethod
    def _warn_legacy_marking(cls, func, name, kind):
        """Attribute-style hook configuration is deprecated (upstream
        HOOK_LEGACY_MARKING, warned at the hook's definition site)."""
        import warnings

        from _pytest.deprecated import HOOK_LEGACY_MARKING

        opts = [
            f"{attr}={getattr(func, attr)}"
            for attr in cls._LEGACY_HOOK_ATTRS
            if hasattr(func, attr)
        ]
        if not opts:
            return
        message = HOOK_LEGACY_MARKING.format(type=kind, fullname=name, hook_opts=", ".join(opts))
        code = getattr(func, "__code__", None)
        if code is not None:
            warnings.warn_explicit(message, type(message), code.co_filename, code.co_firstlineno)
        else:
            warnings.warn(message, stacklevel=3)

    #: Hookimpl parameters carrying py.path.local values, replaced by
    #: pathlib counterparts (upstream HOOK_LEGACY_PATH_ARG).
    _LEGACY_PATH_HOOK_ARGS = {
        "pytest_ignore_collect": ("path", "collection_path"),
        "pytest_collect_file": ("path", "file_path"),
        "pytest_pycollect_makemodule": ("path", "module_path"),
        "pytest_report_header": ("startdir", "start_path"),
        "pytest_report_collectionfinish": ("startdir", "start_path"),
    }

    @classmethod
    def _warn_legacy_path_args(cls, func, name):
        import inspect
        import warnings

        from _pytest.deprecated import HOOK_LEGACY_PATH_ARG

        legacy = cls._LEGACY_PATH_HOOK_ARGS.get(name)
        if legacy is None:
            return
        try:
            params = inspect.signature(func).parameters
        except (TypeError, ValueError):
            return
        if legacy[0] in params:
            warnings.warn(
                HOOK_LEGACY_PATH_ARG.format(pylib_path_arg=legacy[0], pathlib_path_arg=legacy[1]),
                stacklevel=4,
            )

    def add_hookspecs(self, module_or_class: Any) -> None:
        """Record hookspec options (firstresult) declared via
        @pytest.hookspec on the spec container's functions."""
        for name in dir(module_or_class):
            func = getattr(module_or_class, name, None)
            if not name.startswith("pytest_") or not callable(func):
                continue
            opts = getattr(func, "pytest_spec", None)
            if isinstance(opts, dict):
                self._specs[name] = opts
            else:
                self._warn_legacy_marking(func, name, "spec")

    def register(self, plugin: Any, name: str | None = None) -> Any:
        """Track the plugin for hook-relay dispatch; a plugin defining
        pytest_addhooks gets to register its hookspecs immediately (pluggy
        calls it at registration time)."""
        if plugin is None or plugin in self._plugins:
            return None
        for attr in dir(plugin):
            if not attr.startswith("pytest_"):
                continue
            method = getattr(plugin, attr, None)
            if not callable(method):
                continue
            if getattr(method, "pytest_impl", None) is None:
                self._warn_legacy_marking(method, attr, "impl")
            self._warn_legacy_path_args(method, attr)
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
