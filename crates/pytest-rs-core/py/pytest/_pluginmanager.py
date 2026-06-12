"""Minimal config.pluginmanager: getplugin probes for common conftest
checks, plus a pluggy-lite hook relay so autoloaded plugins can register
custom hookspecs (pytest_addhooks) and dispatch them through
``config.pluginmanager.hook.<name>(**kwargs)`` (e.g. pytest-timeout's
pytest_timeout_set_timer). Core plugin loading stays the Rust engine's job."""

from __future__ import annotations

import inspect
import pathlib
import sys
import types
from typing import Any


def instance_hook_impls(name: str) -> list:
    """Hook impls registered on non-module plugin objects (instances
    registered at configure time, e.g. pytest-run-parallel's runner).
    Module-level impls fire via the engine's py_hooks instead, and the
    'terminalreporter' plugin is driven through its own delegation path;
    both are excluded here to avoid double dispatch."""
    import types

    reporter = pluginmanager.getplugin("terminalreporter")
    impls = []
    for plugin in pluginmanager._plugins:
        if isinstance(plugin, types.ModuleType) or plugin is reporter:
            continue
        func = getattr(plugin, name, None)
        if callable(func):
            impls.append(func)
    return impls


class _Result:
    """pluggy's old-style hookwrapper outcome (get_result/force_result)."""

    def __init__(self, result: Any) -> None:
        self._result = result
        self._exception: BaseException | None = None

    def get_result(self) -> Any:
        if self._exception is not None:
            raise self._exception
        return self._result

    def force_result(self, result: Any) -> None:
        self._result = result
        self._exception = None

    def force_exception(self, exception: BaseException) -> None:
        self._exception = exception

    @property
    def exception(self) -> BaseException | None:
        return self._exception

    @property
    def excinfo(self):
        if self._exception is None:
            return None
        return (type(self._exception), self._exception, self._exception.__traceback__)


def _accepted_kwargs(func: Any, kwargs: dict[str, Any]) -> dict[str, Any]:
    """pluggy passes each hookimpl only the arguments its signature names."""
    try:
        params = inspect.signature(func).parameters
    except (TypeError, ValueError):
        return kwargs
    return {name: value for name, value in kwargs.items() if name in params}


class _HookImpl:
    """pluggy HookImpl shim: function + wrapper/hookwrapper/tryfirst/trylast."""

    def __init__(self, func: Any, opts: dict) -> None:
        self.function = func
        self.wrapper = bool(opts.get("wrapper"))
        self.hookwrapper = bool(opts.get("hookwrapper"))
        self.tryfirst = bool(opts.get("tryfirst"))
        self.trylast = bool(opts.get("trylast"))
        self.specname = opts.get("specname")

    def __repr__(self) -> str:
        return f"<HookImpl {self.function!r}>"


class HookCaller:
    """One named hook: calls every registered plugin's same-named function
    (LIFO), honoring firstresult from the registered hookspec."""

    def __init__(self, name: str, pm: PluginManager) -> None:
        self._name = name
        self._pm = pm

    def get_hookimpls(self) -> list[_HookImpl]:
        """Return HookImpl objects for all registered implementations (pluggy API)."""
        impls = []
        for plugin in reversed(self._pm._plugins):
            func = getattr(plugin, self._name, None)
            if callable(func):
                opts = getattr(func, "pytest_impl", None) or {}
                impls.append(_HookImpl(func, opts))
        return impls

    def __call__(self, **kwargs: Any) -> Any:
        kwargs = self._fix_path_args(kwargs)
        firstresult = self._pm._specs.get(self._name, {}).get("firstresult", False)
        impls = []
        for plugin in reversed(self._pm._plugins):
            func = getattr(plugin, self._name, None)
            if callable(func):
                impls.append(func)
        monitors = self._pm._call_monitors
        if monitors:
            return self._call_monitored(monitors, impls, firstresult, kwargs)
        return self._call_impls(impls, firstresult, kwargs)

    def _call_monitored(self, monitors, impls, firstresult, kwargs):
        # before/after wrap the call so HookRecorder sees every hook (even
        # ones with no registered impl, e.g. a freshly-specced hook).
        for before, _after in monitors:
            before(self._name, impls, kwargs)
        outcome_exc = None
        result = None
        try:
            result = self._call_impls(impls, firstresult, kwargs)
        except BaseException as exc:  # noqa: BLE001 - reraised after after()
            outcome_exc = exc
        outcome = _Result(result)
        if outcome_exc is not None:
            outcome.force_exception(outcome_exc)
        for _before, after in monitors:
            after(outcome, self._name, impls, kwargs)
        if outcome_exc is not None:
            raise outcome_exc
        return result

    def _call_impls(self, impls, firstresult, kwargs):
        # pluggy wrapper semantics: wrapper/hookwrapper impls surround the
        # plain impls (run-parallel wraps pytest_report_teststatus this way).
        wrappers = []
        plain_first = []
        plain_normal = []
        plain_last = []
        for func in impls:
            opts = getattr(func, "pytest_impl", None) or {}
            if opts.get("wrapper") or opts.get("hookwrapper"):
                wrappers.append((func, bool(opts.get("hookwrapper"))))
            elif opts.get("tryfirst"):
                plain_first.append(func)
            elif opts.get("trylast"):
                plain_last.append(func)
            else:
                plain_normal.append(func)
        # pluggy order: tryfirst impls run first, trylast last, others between
        # (stable within each group — `impls` is already reverse-registration).
        plain = plain_first + plain_normal + plain_last

        started = []
        for func, old_style in wrappers:
            gen = func(**_accepted_kwargs(func, kwargs))
            if not inspect.isgenerator(gen):
                # A non-generator "wrapper" already ran to completion.
                continue
            try:
                next(gen)
            except StopIteration:
                continue
            started.append((gen, old_style))

        result: Any = None
        results = []
        for func in plain:
            res = func(**_accepted_kwargs(func, kwargs))
            if res is not None:
                if firstresult:
                    results = res
                    break
                results.append(res)
        if firstresult:
            result = results if results else None
        else:
            result = results

        # Unwind innermost-first. New-style wrappers receive the result at
        # their yield and their return value replaces it; old-style
        # hookwrappers receive a Result outcome object.
        for gen, old_style in reversed(started):
            if old_style:
                outcome = _Result(result)
                try:
                    gen.send(outcome)
                    gen.close()
                except StopIteration:
                    pass
                except Exception:
                    raise
                result = outcome.get_result()
            else:
                try:
                    gen.send(result)
                    gen.close()
                except StopIteration as stop:
                    result = stop.value
        return result

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
        if path_value is None and fspath_value is None:
            # Explicit Nones: nothing to translate.
            kwargs[path_var] = None
            kwargs[fspath_var] = None
            return kwargs
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
            "pytest_runtest_makereport": {"firstresult": True},
        }
        # (before, after) callbacks fired around every hook call (HookRecorder
        # registers itself here to record calls; see add_hookcall_monitoring).
        self._call_monitors: list[tuple[Any, Any]] = []
        self.hook = HookRelay(self)
        # Conftest loading state
        self._dirpath2confmods: dict[pathlib.Path, list[types.ModuleType]] = {}
        self._conftest_plugins: set[types.ModuleType] = set()
        self._noconftest: bool = False
        self._confcutdir: pathlib.Path | None = None
        self._using_pyargs: bool = False
        self._configured: bool = False
        self._blocked_plugins: set[str] = set()

    def add_hookcall_monitoring(self, before, after):
        """Register before(name, hook_impls, kwargs) / after(outcome, name,
        hook_impls, kwargs) callbacks fired around every hook call. Returns an
        undo callable that removes them (pluggy API used by HookRecorder)."""
        entry = (before, after)
        self._call_monitors.append(entry)

        def undo():
            try:
                self._call_monitors.remove(entry)
            except ValueError:
                pass

        return undo

    def record_hook(self, name, kwargs):
        """Notify the registered call monitors of a hook invocation without
        executing any implementations. The native engine dispatches conftest
        and plugin hooks directly (not through HookCaller), so during an
        in-process nested run it calls this so a HookRecorder's getcalls sees
        the live call objects (including custom hooks)."""
        monitors = self._call_monitors
        if not monitors:
            return
        for before, _after in monitors:
            before(name, [], kwargs)
        outcome = _Result(None)
        for _before, after in monitors:
            after(outcome, name, [], kwargs)

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

    def list_plugin_distinfo(self):
        """(plugin, dist) pairs for registered plugins backed by a
        distribution. The native engine tracks plugins out-of-band, so this
        is empty here (the session header's "plugins:" line is omitted)."""
        return []

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

    def parse_hookimpl_opts(self, plugin: Any, name: str) -> dict | None:
        """Return hookimpl opts dict if name is a hook implementation, else None (pluggy API)."""
        method = getattr(plugin, name, None)
        if not callable(method):
            return None
        opts = getattr(method, "pytest_impl", None)
        if isinstance(opts, dict):
            return opts
        for attr in self._LEGACY_HOOK_ATTRS:
            if hasattr(method, attr):
                return {attr: getattr(method, attr)}
        return None

    def get_plugins(self) -> list[Any]:
        return list(self._plugins)

    def is_blocked(self, name: str) -> bool:
        return name in self._blocked_plugins

    def set_blocked(self, name: str) -> None:
        self._blocked_plugins.add(name)

    def unblock(self, name: str) -> None:
        self._blocked_plugins.discard(name)

    def trace(self, msg: str) -> None:
        pass

    def _get_directory(self, path: pathlib.Path) -> pathlib.Path:
        return path.parent if path.is_file() else path

    def _is_in_confcutdir(self, path: pathlib.Path) -> bool:
        if self._confcutdir is None:
            return True
        return path not in self._confcutdir.parents

    def _try_load_conftest(
        self,
        anchor: pathlib.Path,
        importmode: str,
        rootpath: pathlib.Path,
        *,
        consider_namespace_packages: bool,
    ) -> None:
        self._loadconftestmodules(
            anchor, importmode, rootpath, consider_namespace_packages=consider_namespace_packages
        )
        if anchor.is_dir():
            for x in anchor.glob("test*"):
                if x.is_dir():
                    self._loadconftestmodules(
                        x,
                        importmode,
                        rootpath,
                        consider_namespace_packages=consider_namespace_packages,
                    )

    def _loadconftestmodules(
        self,
        path: pathlib.Path,
        importmode: str,
        rootpath: pathlib.Path,
        *,
        consider_namespace_packages: bool,
    ) -> None:
        if self._noconftest:
            return
        directory = self._get_directory(path)
        if directory in self._dirpath2confmods:
            return
        clist: list[types.ModuleType] = []
        for parent in reversed((directory, *directory.parents)):
            if self._is_in_confcutdir(parent):
                conftestpath = parent / "conftest.py"
                if conftestpath.is_file():
                    mod = self._importconftest(
                        conftestpath,
                        importmode,
                        rootpath,
                        consider_namespace_packages=consider_namespace_packages,
                    )
                    clist.append(mod)
        self._dirpath2confmods[directory] = clist

    def _getconftestmodules(self, path: pathlib.Path) -> list[types.ModuleType]:
        directory = self._get_directory(path)
        return self._dirpath2confmods.get(directory, [])

    def _rget_with_confmod(self, name: str, path: pathlib.Path) -> tuple[types.ModuleType, Any]:
        modules = self._getconftestmodules(path)
        for mod in reversed(modules):
            try:
                return mod, getattr(mod, name)
            except AttributeError:
                continue
        raise KeyError(name)

    def _importconftest(
        self,
        conftestpath: pathlib.Path,
        importmode: str,
        rootpath: pathlib.Path,
        *,
        consider_namespace_packages: bool,
    ) -> types.ModuleType:
        conftestpath = pathlib.Path(conftestpath)
        conftestpath_plugin_name = str(conftestpath)
        existing = self.getplugin(conftestpath_plugin_name)
        if existing is not None:
            return existing  # type: ignore[return-value]

        # Non-package conftest.py files all have module name "conftest"; clear
        # the cache entry so a fresh load doesn't return the previous one.
        try:
            del sys.modules[conftestpath.stem]
        except KeyError:
            pass

        try:
            from _pytest.pathlib import import_path

            mod = import_path(
                conftestpath,
                mode=importmode,
                root=rootpath,
                consider_namespace_packages=consider_namespace_packages,
            )
        except Exception as e:
            from _pytest.config import ConftestImportFailure

            raise ConftestImportFailure(conftestpath, cause=e) from e

        self._conftest_plugins.add(mod)
        dirpath = conftestpath.parent
        if dirpath in self._dirpath2confmods:
            for p, mods in self._dirpath2confmods.items():
                if dirpath in p.parents or p == dirpath:
                    if mod not in mods:
                        mods.append(mod)
        self.trace(f"loading conftestmodule {mod!r}")
        self.consider_conftest(mod, registration_name=conftestpath_plugin_name)
        return mod

    def _set_initial_conftests(
        self,
        args: Any,
        pyargs: bool,
        noconftest: bool,
        rootpath: pathlib.Path,
        confcutdir: pathlib.Path | None,
        invocation_dir: pathlib.Path,
        importmode: str,
        *,
        consider_namespace_packages: bool,
    ) -> None:
        from _pytest.pathlib import absolutepath, safe_exists

        self._confcutdir = absolutepath(invocation_dir / confcutdir) if confcutdir else None
        self._noconftest = noconftest
        self._using_pyargs = pyargs
        foundanchor = False
        for initial_path in args:
            path = str(initial_path)
            i = path.find("::")
            if i != -1:
                path = path[:i]
            anchor = absolutepath(invocation_dir / path)
            if safe_exists(anchor):
                self._try_load_conftest(
                    anchor,
                    importmode,
                    rootpath,
                    consider_namespace_packages=consider_namespace_packages,
                )
                foundanchor = True
        if not foundanchor:
            self._try_load_conftest(
                invocation_dir,
                importmode,
                rootpath,
                consider_namespace_packages=consider_namespace_packages,
            )

    def consider_conftest(self, conftestmodule: types.ModuleType, registration_name: str) -> None:
        self.register(conftestmodule, name=registration_name)

    def import_plugin(self, modname: str, consider_entry_points: bool = False) -> None:
        assert isinstance(modname, str), f"module name as text required, got {modname!r}"
        if self.is_blocked(modname) or self.getplugin(modname) is not None:
            return
        try:
            __import__(modname)
        except ImportError as e:
            raise ImportError(f'Error importing plugin "{modname}": {e.args[0]}').with_traceback(
                e.__traceback__
            ) from e
        else:
            mod = sys.modules[modname]
            self.register(mod, modname)


pluginmanager = PluginManager()
