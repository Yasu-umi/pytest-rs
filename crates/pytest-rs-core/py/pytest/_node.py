"""The `request.node` object: a minimal pytest Item surface."""

import inspect
import pathlib
import traceback
import warnings

# Marks added at runtime (node.add_marker / request.applymarker) for the
# currently running item; the engine re-evaluates xfail against these and
# clears the list per item.
_added_marks: list = []


class _NodeKeywords:
    """Mutable dict-like keyword map for a node (pytest's NodeKeywords).

    Priority (highest wins): own function attributes > own marks/extra > ancestor chain.
    """

    def __init__(self, node):
        self._node = node
        self._extra = {}

    def _own_items(self):
        name = self._node.name
        result = {name: True}
        # parametrize id lives inside the brackets of the nodeid component
        # e.g. "test_func[hello-123]" → add "hello-123" as a keyword too
        if name.endswith("]") and "[" in name:
            param_id = name[name.index("[") + 1 : -1]
            result[param_id] = True
        for m in getattr(self._node, "own_markers", []):
            result[m.name] = m
        result.update(self._extra)
        func = getattr(self._node, "function", None)
        if func is not None:
            for k, v in vars(func).items() if hasattr(func, "__dict__") else []:
                if not k.startswith("_") and k != "pytestmark":
                    result[k] = v
        return result

    def _all_items(self):
        chain = []
        node = self._node
        while node is not None:
            chain.append(node)
            node = getattr(node, "parent", None)
        chain.reverse()
        result = {}
        for n in chain:
            kw = getattr(n, "_keywords", None)
            if kw is not None:
                result.update(kw._own_items())
            else:
                result[n.name] = True
                for m in getattr(n, "own_markers", []):
                    result[m.name] = m
        return result

    def __getitem__(self, key):
        return self._all_items()[key]

    def __setitem__(self, key, value):
        self._extra[key] = value

    def __contains__(self, key):
        return key in self._all_items()

    def __iter__(self):
        return iter(self._all_items())

    def __len__(self):
        return len(self._all_items())

    def keys(self):
        return self._all_items().keys()

    def values(self):
        return self._all_items().values()

    def items(self):
        return self._all_items().items()

    def get(self, key, default=None):
        return self._all_items().get(key, default)

    def __or__(self, other):
        result = dict(self._all_items())
        result.update(other._all_items() if isinstance(other, _NodeKeywords) else other)
        return result

    def __ror__(self, other):
        result = dict(other._all_items() if isinstance(other, _NodeKeywords) else other)
        result.update(self._all_items())
        return result

    def __repr__(self):
        return repr(self._all_items())


def record_added_mark(marker):
    mark = getattr(marker, "mark", marker)
    name = getattr(mark, "name", None)
    if isinstance(name, str):
        _added_marks.append((name, mark))


def added_marks():
    return list(_added_marks)


def clear_added_marks():
    _added_marks.clear()


class _NodeBase:
    """Shared Node implementation. Use Collector or Item, not this directly."""

    def __init__(
        self,
        name=None,
        parent=None,
        config=None,
        session=None,
        fspath=None,
        path=None,
        nodeid=None,
        **kwargs,
    ):
        self.parent = parent
        self.config = config if config is not None else getattr(parent, "config", None)
        self.session = session if session is not None else getattr(parent, "session", None)
        if path is None and fspath is not None:
            import warnings

            from _pytest.deprecated import NODE_CTOR_FSPATH_ARG

            warnings.warn(
                NODE_CTOR_FSPATH_ARG.format(node_type_name=type(self).__name__),
                stacklevel=3,
            )
            path = pathlib.Path(str(fspath))
        if path is None and parent is not None:
            path = getattr(parent, "path", None)
        self.path = pathlib.Path(str(path)) if path is not None else None
        self.name = name if name is not None else (self.path.name if self.path is not None else "")
        self.own_markers = []
        self._nodeid = nodeid if nodeid is not None else self._compute_nodeid()

    @classmethod
    def _create(cls, *k, **kw):
        """Construct cls directly, bypassing any metaclass __call__ override (e.g. NodeMeta).

        Using type.__call__ skips NodeMeta.__call__ (which raises on direct construction)
        while still invoking cls.__new__ + cls.__init__ through the normal slot machinery.
        """
        return type.__call__(cls, *k, **kw)

    @classmethod
    def from_parent(cls, parent, **kwargs):
        """Construct a child node under `parent` (pytest's Node.from_parent)."""
        if "session" in kwargs:
            raise TypeError(
                "session is a keyword-only argument of Node.from_parent; "
                "pass it via parent.session or the config"
            )
        if "config" in kwargs:
            raise TypeError(
                "config is a keyword-only argument of Node.from_parent; pass it via parent.config"
            )
        try:
            return cls._create(parent=parent, **kwargs)
        except TypeError:
            # Non-cooperative constructor: __init__ lacks **kwargs and raised on unknown kw.
            own_init = cls.__dict__.get("__init__")
            if own_init is not None:
                import warnings
                from inspect import Parameter, signature

                from _pytest.warning_types import PytestDeprecationWarning

                sig = signature(own_init)
                has_var_kw = any(p.kind == Parameter.VAR_KEYWORD for p in sig.parameters.values())
                if not has_var_kw:
                    known_kw = {k: v for k, v in kwargs.items() if k in sig.parameters}
                    warnings.warn(
                        PytestDeprecationWarning(
                            f"{cls} is not using a cooperative constructor and only takes {set(known_kw)}.\n"
                            "See https://docs.pytest.org/en/stable/deprecations.html"
                            "#constructors-of-custom-pytest-node-subclasses-should-take-kwargs "
                            "for more details."
                        )
                    )
                    return cls._create(parent=parent, **known_kw)
            raise

    def _compute_nodeid(self):
        parent_id = getattr(self.parent, "nodeid", None)
        if isinstance(self, Item) and parent_id:
            return f"{parent_id}::{self.name}"
        if self.path is not None and self.config is not None:
            root = getattr(self.config, "rootpath", None) or getattr(self.config, "rootdir", None)
            if root is not None:
                try:
                    rel = (
                        pathlib.Path(self.path)
                        .resolve()
                        .relative_to(pathlib.Path(str(root)).resolve())
                    )
                    return str(rel).replace("\\", "/")
                except ValueError:
                    pass
        return self.name

    @property
    def nodeid(self):
        return self._nodeid

    @nodeid.setter
    def nodeid(self, value):
        # Node/Function (engine nodes) subclass this and assign self.nodeid.
        self._nodeid = value

    @property
    def fspath(self):
        """Legacy py.path.local (node.fspath.mtime() etc.)."""
        import py

        return py.path.local(str(self.path))

    @property
    def ihook(self):
        from pytest._pluginmanager import pluginmanager

        return pluginmanager.hook

    def add_marker(self, marker, append=True):
        from pytest._marks import Mark, MarkDecorator

        if isinstance(marker, str):
            marker = Mark(marker)
        elif isinstance(marker, MarkDecorator):
            marker = marker.mark
        else:
            raise ValueError(f"is not a string or pytest.mark.* Marker object: {marker!r}")
        if append:
            self.own_markers.append(marker)
        else:
            self.own_markers.insert(0, marker)

    def get_closest_marker(self, name, default=None):
        for marker in self.own_markers:
            if marker.name == name:
                return marker
        return default

    def iter_markers(self, name=None):
        for marker in self.own_markers:
            if name is None or marker.name == name:
                yield marker

    def getparent(self, cls):
        """Walk up the parent chain and return the first node that is an
        instance of `cls`, or None. Mirrors pytest's Node.getparent."""
        node = self
        while node is not None:
            if isinstance(node, cls):
                return node
            node = getattr(node, "parent", None)
        return None

    def listchain(self):
        """Return the chain from the root down to this node (inclusive)."""
        chain = []
        node = self
        while node is not None:
            chain.append(node)
            node = getattr(node, "parent", None)
        chain.reverse()
        return chain

    def __eq__(self, other):
        if not isinstance(other, _NodeBase):
            return NotImplemented
        return self.nodeid == other.nodeid

    def __ne__(self, other):
        result = self.__eq__(other)
        if result is NotImplemented:
            return result
        return not result

    def __hash__(self):
        return hash(self.nodeid)

    def __repr__(self):
        return f"<{type(self).__name__} {self.nodeid!r}>"

    @property
    def keywords(self):
        if not hasattr(self, "_keywords"):
            self._keywords = _NodeKeywords(self)
        return self._keywords


class Collector(_NodeBase):
    """Base for collection nodes (pytest.File, pytest.Class, etc.)."""

    class CollectError(Exception):
        """An error during collection, shown without a traceback."""


def _custom_item_xfail_mark(item):
    """The last @pytest.mark.xfail on a custom item, or None. pytest-mypy adds
    one dynamically in runtest() (via add_marker) before raising MypyError when
    --mypy-xfail is set."""
    for marker in reversed(getattr(item, "own_markers", []) or []):
        if getattr(marker, "name", None) == "xfail":
            return marker
    return None


def _custom_item_runxfail(item):
    config = getattr(item, "config", None) or getattr(
        getattr(item, "session", None), "config", None
    )
    try:
        return bool(config.getoption("runxfail"))
    except Exception:
        return False


def _custom_item_xfail(item, exc):
    """Return the xfail reason if `exc` from a custom item's runtest is expected
    by an xfail marker (matching its `raises`), else None."""
    marker = _custom_item_xfail_mark(item)
    if marker is None or _custom_item_runxfail(item):
        return None
    raises = marker.kwargs.get("raises")
    if raises is not None and not isinstance(exc, raises):
        return None
    return marker.kwargs.get("reason") or (marker.args[0] if marker.args else "")


def _custom_item_pass_report(item):
    """A custom item that ran clean: XPASS if an xfail marker fired anyway
    (strict-aware), else a plain pass."""
    marker = _custom_item_xfail_mark(item)
    if marker is None or _custom_item_runxfail(item):
        return ("call", "passed", None)
    reason = marker.kwargs.get("reason") or (marker.args[0] if marker.args else "")
    config = getattr(item, "config", None) or getattr(
        getattr(item, "session", None), "config", None
    )
    strict = marker.kwargs.get("strict")
    if strict is None:
        try:
            strict = bool(config.getini("xfail_strict"))
        except Exception:
            strict = False
    if strict:
        return ("call", "failed", f"[XPASS(strict)] {reason}")
    return ("call", "xpassed", reason)


def run_custom_item(item):
    """Run a custom collector Item (setup/runtest/teardown) and return a list
    of (when, outcome, longrepr) tuples for the engine. Failures get their
    longrepr from Item.repr_failure plus any pytest_exception_interact hook
    (pytest-ruff sets the ruff error message there)."""
    from pytest._outcomes import Skipped, XFailed

    def _excinfo(exc):
        class _ExcInfo:
            def __init__(self):
                self.value = exc
                self.type = type(exc)
                self.typename = type(exc).__name__

            def exconly(self, tryshort=False):
                return f"{type(exc).__name__}: {exc}"

            def errisinstance(self, cls):
                return isinstance(exc, cls)

        return _ExcInfo()

    def _failrepr(when, exc):
        excinfo = _excinfo(exc)
        try:
            longrepr = item.repr_failure(excinfo)
        except Exception:
            longrepr = None
        if not longrepr:
            longrepr = "".join(traceback.format_exception(type(exc), exc, exc.__traceback__))
        # pytest_exception_interact may replace report.longrepr.
        report = type("_Report", (), {})()
        report.when = when
        report.nodeid = item.nodeid
        report.outcome = "failed"
        report.longrepr = longrepr
        report.failed = True

        class _Call:
            def __init__(self):
                self.when = when
                self.excinfo = excinfo

        try:
            item.ihook.pytest_exception_interact(node=item, call=_Call(), report=report)
        except Exception:
            pass
        return str(report.longrepr)

    reports = []

    # Walk ancestor collectors and run their setup() before the item's own
    # setup(). Per-collector exceptions are cached on the collector object so
    # repeated failures across items produce a traceback of the same length
    # (the "BTW" / #12204 case). Internal shim nodes (module pytest._node,
    # _pytest.*, and _CollectorProxy instances) are skipped.
    for col in item.listchain()[:-1]:
        mod = type(col).__module__
        if mod == "pytest._node" or mod.startswith("_pytest.") or isinstance(col, _CollectorProxy):
            continue
        if not hasattr(col, "setup"):
            continue
        cached = getattr(col, "_pytest_rs_col_setup_exc", None)
        if cached is not None:
            cached_exc, cached_tb = cached
            reports.append(
                ("setup", "failed", _failrepr("setup", cached_exc.with_traceback(cached_tb)))
            )
            try:
                item.teardown()
                reports.append(("teardown", "passed", None))
            except BaseException as texc:  # noqa: BLE001
                reports.append(("teardown", "failed", _failrepr("teardown", texc)))
            return reports
        if not getattr(col, "_pytest_rs_col_setup_done", False):
            col._pytest_rs_col_setup_done = True
            try:
                col.setup()
            except BaseException as exc:  # noqa: BLE001 - protocol boundary
                col._pytest_rs_col_setup_exc = (exc, exc.__traceback__)
                reports.append(("setup", "failed", _failrepr("setup", exc)))
                try:
                    item.teardown()
                    reports.append(("teardown", "passed", None))
                except BaseException as texc:  # noqa: BLE001
                    reports.append(("teardown", "failed", _failrepr("teardown", texc)))
                return reports

    try:
        item.setup()
    except Skipped as exc:
        reports.append(("setup", "skipped", str(getattr(exc, "msg", exc) or exc)))
        reports.append(("teardown", "passed", None))
        return reports
    except XFailed as exc:
        reports.append(("call", "xfailed", str(getattr(exc, "msg", exc) or exc)))
        reports.append(("teardown", "passed", None))
        return reports
    except BaseException as exc:  # noqa: BLE001 - protocol boundary
        reports.append(("setup", "failed", _failrepr("setup", exc)))
        try:
            item.teardown()
            reports.append(("teardown", "passed", None))
        except BaseException as texc:  # noqa: BLE001
            reports.append(("teardown", "failed", _failrepr("teardown", texc)))
        return reports

    try:
        item.runtest()
        reports.append(_custom_item_pass_report(item))
    except Skipped as exc:
        reports.append(("call", "skipped", str(getattr(exc, "msg", exc) or exc)))
    except XFailed as exc:
        reports.append(("call", "xfailed", str(getattr(exc, "msg", exc) or exc)))
    except BaseException as exc:  # noqa: BLE001
        xfail = _custom_item_xfail(item, exc)
        if xfail is not None:
            reports.append(("call", "xfailed", xfail))
        else:
            reports.append(("call", "failed", _failrepr("call", exc)))

    try:
        item.teardown()
        reports.append(("teardown", "passed", None))
    except BaseException as texc:  # noqa: BLE001
        reports.append(("teardown", "failed", _failrepr("teardown", texc)))
    return reports


class Session(Collector):
    """Stub session collector (annotations/isinstance upstream, e.g.
    pytest-run-parallel's pytest_runtestloop signature)."""

    class Failed(Exception):
        """Signals a stop as failed test run (upstream)."""

    class Interrupted(KeyboardInterrupt):
        """Signals an interrupted test run (upstream)."""

    @classmethod
    def from_config(cls, config):
        rootdir = getattr(config, "rootpath", None) or getattr(config, "rootdir", None)
        rootdir = pathlib.Path(str(rootdir)) if rootdir is not None else pathlib.Path.cwd()
        session = cls(name=rootdir.name or ".", config=config, path=rootdir, nodeid="")
        session.parent = None
        return session

    def perform_collect(self, args=None, genitems=False):
        if not args or args == [self.nodeid] or args == [""]:
            native = take_native_collection()
            if native is not None:
                items, fixturemanager = native
                self.items = items
                self._fixturemanager = fixturemanager
                return list(items)
            return [self]
        results = []
        for arg in args:
            arg_str = str(arg)
            file_part_str = arg_str.split("::")[0] if "::" in arg_str else arg_str
            file_part = pathlib.Path(file_part_str)
            if not file_part.is_absolute():
                file_part = self.path / file_part
            if not file_part.exists():
                continue
            if file_part.is_dir():
                try:
                    rel = str(file_part.resolve().relative_to(self.path.resolve())).replace(
                        "\\", "/"
                    )
                except ValueError:
                    rel = file_part.name
                dir_node = Dir(
                    name=file_part.name,
                    config=self.config,
                    path=file_part,
                    nodeid=rel,
                    session=self,
                )
                dir_node.parent = self
                results.append(dir_node)
            elif file_part.is_file():
                parent_dir = file_part.parent
                is_pkg = (parent_dir / "__init__.py").is_file()
                try:
                    parent_rel = str(parent_dir.resolve().relative_to(self.path.resolve())).replace(
                        "\\", "/"
                    )
                except ValueError:
                    parent_rel = parent_dir.name
                if is_pkg:
                    mid_node = Package(
                        name=parent_dir.name,
                        config=self.config,
                        path=parent_dir,
                        nodeid=parent_rel,
                        session=self,
                    )
                    mid_node.parent = self
                else:
                    mid_node = Dir(
                        name=parent_dir.name,
                        config=self.config,
                        path=parent_dir,
                        nodeid=parent_rel,
                        session=self,
                    )
                    mid_node.parent = self
                try:
                    file_rel = str(file_part.resolve().relative_to(self.path.resolve())).replace(
                        "\\", "/"
                    )
                except ValueError:
                    file_rel = file_part.name
                mod_node = File(
                    name=file_part.name,
                    config=self.config,
                    path=file_part,
                    nodeid=file_rel,
                    session=self,
                )
                mod_node.parent = mid_node
                results.append(mod_node)
        return results


class Class(Collector):
    """A class collector stand-in for pytest_collectstart: carries .obj (the
    test class) and collects markers via add_marker, propagated to its items."""

    def __init__(self, name=None, parent=None, *, obj=None, **kwargs):
        if name is not None:
            kwargs.setdefault("name", name)
        if parent is not None:
            kwargs.setdefault("parent", parent)
        super().__init__(**kwargs)
        self.obj = obj

    def reportinfo(self):
        """(path, 0-based lineno, class name) — mirrors pytest's Class.reportinfo.
        inspect.getsourcelines returns the 1-based def line, so subtract 1."""
        import inspect

        obj = self.obj
        try:
            lineno0 = inspect.getsourcelines(obj)[1] - 1
        except (OSError, TypeError):
            lineno0 = 0
        return (self.path, lineno0, self.name)


class Item(_NodeBase):
    """Base test item for custom collectors. Subclasses override runtest()
    (and optionally setup()/teardown()/repr_failure()/reportinfo())."""

    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self._check_item_and_collector_diamond_inheritance()

    def _check_item_and_collector_diamond_inheritance(self):
        cls = type(self)
        attr_name = "_pytest_diamond_inheritance_warning_shown"
        if getattr(cls, attr_name, False):
            return
        setattr(cls, attr_name, True)
        problems = ", ".join(base.__name__ for base in cls.__bases__ if issubclass(base, Collector))
        if problems:
            import warnings

            from _pytest.warning_types import PytestWarning

            warnings.warn(
                f"{cls.__name__} is an Item subclass and should not be a collector, "
                f"however its bases {problems} are collectors.\n"
                "Please split the Collectors and the Item into separate node types.\n"
                "Pytest Doc example: https://docs.pytest.org/en/latest/example/nonpython.html\n"
                "example pull request on a plugin: https://github.com/asmeurer/pytest-flakes/pull/40/",
                PytestWarning,
            )

    def setup(self):
        pass

    def runtest(self):
        raise NotImplementedError("custom Item must implement runtest()")

    def teardown(self):
        pass

    def reportinfo(self):
        return (self.path, None, "")

    def repr_failure(self, excinfo, style=None):
        """Failure text (subclasses may override); default is the exception."""
        return str(getattr(excinfo, "value", excinfo))


class File(Collector):
    """Base file collector. Subclasses override collect() to yield Items."""

    @classmethod
    def from_parent(cls, parent, *, fspath=None, path=None, **kw):
        """Inject path=None (mirrors FSCollector.from_parent) so non-cooperative
        constructors that lack **kwargs receive a TypeError and the
        cooperative-constructor deprecation warning."""
        return super().from_parent(parent=parent, fspath=fspath, path=path, **kw)

    def collect(self):
        return []


def is_bare_file_collector(collector):
    """True when `collector` is a File/Module instance relying on the base
    stub's `collect()` (no real subclass override — e.g. a conftest's
    `pytest_collect_file` returning a plain `Module.from_parent(...)`
    unmodified). The engine falls back to native module scanning instead of
    trusting the stub's unconditional empty list; a genuine custom collector
    (pytest-mypy's MypyFile, pytest-ruff's RuffFile) overrides `collect()`
    and is left alone."""
    return getattr(type(collector), "collect", None) is File.collect


class Package(File):
    """Directory-level collector for Python packages (has __init__.py).
    Alias used by upstream isinstance checks (e.g. pytest.Package in test_collection)."""


class Dir(Collector):
    """Directory-level collector for plain directories (no __init__.py)."""


def make_default_directory_node(path, parent):
    """The plain `Dir`/`Package` node for one directory level, used both as
    the built-in `pytest_collect_directory` fallback and directly by the
    Rust-side directory walker (`walk_collect_directories`, hooks.rs) when no
    `pytest_collect_directory` hookimpl is registered at all."""
    is_pkg = (path / "__init__.py").is_file()
    cls = Package if is_pkg else Dir
    return cls(name=path.name, path=path, nodeid="", parent=parent)


def _parent_chain_cache():
    """Per-run cache of the File/Class/Session nodes built by
    attach_parent_chain, keyed in _session_state so nested in-process runs
    (run_inprocess's fresh-dict swap) get an isolated cache for free."""
    return _session_state.setdefault("parent_chain", {"session": None, "files": {}, "classes": {}})


def _parent_chain_session(config):
    cache = _parent_chain_cache()
    if cache["session"] is None:
        cache["session"] = Session.from_config(config)
    return cache["session"]


def _parent_chain_file(key, path, config, module):
    files = _parent_chain_cache()["files"]
    node = files.get(key)
    if node is None:
        node = File(name=path.name if path is not None else key, path=path, config=config)
        node.parent = _parent_chain_session(config)
        node.obj = module
        if module is not None:
            from pytest._marks import get_unpacked_marks

            node.own_markers = list(get_unpacked_marks(module))
        files[key] = node
    return node


def _parent_chain_class(cls, config, file_node):
    classes = _parent_chain_cache()["classes"]
    node = classes.get(id(cls))
    if node is None:
        from pytest._marks import get_unpacked_marks

        node = Class(
            name=cls.__name__,
            parent=file_node,
            obj=cls,
            nodeid=f"{file_node.nodeid}::{cls.__name__}",
        )
        node.own_markers = list(get_unpacked_marks(cls))
        classes[id(cls)] = node
    return node


def attach_parent_chain(node):
    """Populate node.parent with the (cached, run-scoped) enclosing
    Class/File/Session chain so Node.getparent works for plugins that need it
    (e.g. pytest-dependency's scope='class'/'module'/'session' lookups,
    pytest-order's class-mark relative-ordering). No-op if a parent is
    already set (custom pytest_pycollect_makeitem nodes keep their own) or
    the node has no config to build a chain from."""
    if getattr(node, "parent", None) is not None:
        return
    config = getattr(node, "config", None)
    if config is None:
        return
    module = getattr(node, "module", None)
    path = getattr(node, "path", None)
    key = module.__name__ if module is not None else str(path)
    file_node = _parent_chain_file(key, path, config, module)
    cls = getattr(node, "cls", None)
    node.parent = _parent_chain_class(cls, config, file_node) if cls is not None else file_node


class _DefaultCollectDirectory:
    """Permanent low-priority (trylast) `pytest_collect_directory` participant.

    pytest-rs has no built-in default for this hook otherwise — unlike
    upstream, where `_pytest.main`'s own hookimpl always participates in the
    same firstresult chain. Without a default participant here, a conftest
    hookimpl that merely *declines* (returns `None`, not via a hookwrapper
    forcing it) would make the overall hook_relay.call() result `None` too,
    which Rust cannot distinguish from a real forced skip
    (`test_directory_ignored_if_none`'s `@hookimpl(wrapper=True)` pattern) —
    trylast ensures conftest-registered hookimpls (LIFO-prioritized ahead of
    this) are tried first, so this only ever fires when nothing else claimed
    the directory. Idempotently registered by
    ensure_default_collect_directory_registered() (called from hooks.rs
    before firing the hook) rather than at import time, to keep `_node.py`
    free of a module-level pluginmanager dependency."""

    @staticmethod
    def pytest_collect_directory(path, parent):
        return make_default_directory_node(path, parent)


def ensure_default_collect_directory_registered():
    from pytest._pluginmanager import pluginmanager

    name = "_default_collect_directory"
    if pluginmanager.getplugin(name) is None:
        plugin = _DefaultCollectDirectory()
        plugin.pytest_collect_directory.pytest_impl = {"trylast": True}
        pluginmanager.register(plugin, name)


def collect_custom_directory(collector):
    """Call a custom `pytest.Directory` subclass's `.collect()` (the
    documented customdirectory.rst pattern: a `pytest_collect_directory`
    hookimpl returns e.g. a `ManifestDirectory` whose `collect()` reads its
    own file list and delegates each one to `self.ihook.pytest_collect_file`).

    pytest-rs has no default `pytest_collect_file` hookimpl registered
    normally — standard file collection happens via native Rust scanning
    instead, bypassing the hook entirely — so a custom `.collect()` that
    delegates through the hook like the example above would otherwise get
    nothing back. Temporarily register a minimal one (any `.py` file becomes
    a bare `File`) for the duration of this one call.

    Returns the file paths (str) the custom collector approved, in
    call order. The caller intersects this against its own independently
    (natively) discovered file list rather than trusting it to introduce new
    files outright — matching upstream's full lazy, hook-aware directory walk
    is out of scope; this only supports a custom collector *narrowing* which
    already-discovered files get collected."""
    from pytest._pluginmanager import pluginmanager

    class _DefaultCollectFile:
        @staticmethod
        def pytest_collect_file(file_path, parent):
            if file_path.suffix == ".py":
                return File.from_parent(parent=parent, path=file_path)
            return None

    plugin = _DefaultCollectFile()
    pluginmanager.register(plugin)
    try:
        paths = []
        for node in collector.collect():
            path = getattr(node, "path", None)
            if path is not None:
                paths.append(str(path))
        return paths
    finally:
        pluginmanager.unregister(plugin)


class DoctestNode:
    """Node subtype for doctest items; recognized by _pytest.doctest.DoctestItem."""

    _pytest_doctest_item = True

    def __init__(
        self, nodeid, name, marks, fixturenames=None, function=None, path=None, lineno=None
    ):
        self.nodeid = nodeid
        self.name = name
        self.own_markers = list(marks)
        self.fixturenames = list(fixturenames or [])
        self.function = function
        self.obj = function
        # Normalize to pathlib.Path like _NodeBase/File so item.path matches a
        # collector's .path (upstream Node.path is always a Path).
        self.path = pathlib.Path(str(path)) if path is not None else None
        self.lineno = lineno
        # The Python module/class this item was collected from. Reordering
        # plugins (pytest-randomly, pytest-order) shuffle by
        # item.module.__name__ and item.cls; the engine fills these in
        # make_node from the already-imported module and the collected class.
        self.module = None
        self.cls = None
        self.instance = None

    @property
    def fspath(self):
        """Legacy py.path.local of this node's file (upstream Node.fspath);
        some plugins still use node.fspath.dirpath()."""
        import py

        return py.path.local(self.path)

    @property
    def ihook(self):
        """The shim pluginmanager's hook relay (upstream: the node's
        fspath-sensitive HookProxy)."""
        from pytest._pluginmanager import pluginmanager

        return pluginmanager.hook

    @property
    def keywords(self):
        if not hasattr(self, "_keywords"):
            self._keywords = _NodeKeywords(self)
        return self._keywords

    def warn(self, warning):
        """Issue a warning attributed to this item's definition site
        (pytest's Node.warn: warn_explicit with the item location)."""
        if not isinstance(warning, Warning):
            raise ValueError(
                f"warning must be an instance of Warning or subclass, got {warning!r} instead"
            )
        warnings.warn_explicit(
            warning,
            category=type(warning),
            filename=str(self.path) if self.path else "<unknown>",
            lineno=self.lineno or 0,
        )

    def get_closest_marker(self, name, default=None):
        for marker in self.own_markers:
            if marker.name == name:
                return marker
        return default

    def iter_markers(self, name=None):
        for marker in self.own_markers:
            if name is None or marker.name == name:
                yield marker

    def iter_markers_with_node(self, name=None):
        for marker in self.own_markers:
            if name is None or marker.name == name:
                yield self, marker

    def add_marker(self, marker, append=True):
        from pytest._marks import Mark, MarkDecorator

        if isinstance(marker, str):
            marker = Mark(marker)
        elif isinstance(marker, MarkDecorator):
            marker = marker.mark
        if append:
            self.own_markers.append(marker)
        else:
            self.own_markers.insert(0, marker)
        record_added_mark(marker)


# Session.shouldfail / shouldstop set by plugins (pytest-timeout's session
# deadline) or the engine (--maxfail / --stepwise): the runner polls these
# between items and aborts with the message banner.
_session_state: dict = {
    "shouldfail": None,
    "shouldstop": None,
    "items": [],
    "session_markers": [],
    "session_keywords": {},
    "skipped_modules": [],
}


def session_shouldfail():
    return _session_state["shouldfail"]


def session_shouldstop():
    return _session_state["shouldstop"]


def set_session_shouldfail(value):
    """Engine-side set (--maxfail): bypasses the sticky setter so the
    conftest's pytest_sessionfinish sees the truthy value."""
    _session_state["shouldfail"] = value


def set_session_shouldstop(value):
    """Engine-side set (--stepwise)."""
    _session_state["shouldstop"] = value


def set_session_items(items):
    """Collected item proxies, published once collection finishes (the
    engine fires pytest_collection_finish with them on the session)."""
    _session_state["items"] = list(items)


def set_session_testscollected(n):
    """Override testscollected count (used by xdist: workers collect,
    so session.items is empty but testscollected must reflect the total)."""
    _session_state["testscollected"] = n


def set_session_skipped_modules(modules):
    """Skipped-module records [(nodeid, reason, location), ...], published
    before pytest_collection_finish so the relay can serialize them."""
    _session_state["skipped_modules"] = list(modules)


def reset_collection_items():
    """Start a fresh custom-collection pass with an empty session.items so a
    prior in-process run's items don't leak into the isinstance checks below."""
    _session_state["items"] = []
    _session_state["skipped_modules"] = []


def publish_collection_item(item):
    """Append a custom-collected item to session.items mid-collection. Real
    pytest's `self.items.extend(self.genitems(node))` appends each yielded item
    as the generator produces it, so a later collector's collect() sees its
    siblings (pytest-mypy's MypyFile.collect skips adding a second
    MypyStatusItem once one is already in session.items)."""
    _session_state["items"].append(item)


def get_session_keywords() -> dict:
    """Return the session-level keywords dict (mutable, persists across items).
    Used by session-scoped fixtures' request.keywords."""
    return _session_state["session_keywords"]


def session_obj_overrides():
    """(nodeid, obj) for items whose `obj` a plugin swapped after they were
    published (pytest-run-parallel wraps test functions for threaded
    repeats); the engine writes these back into its own items."""
    result = []
    for node in _session_state["items"]:
        try:
            obj = node.obj
            fn = node.function
        except AttributeError:
            continue
        if obj is not fn:
            result.append((node.nodeid, obj))
    return result


class _CallSpec:
    """item.callspec for parametrized items: the param values and the id (the
    "[a-1]" bracket of the nodeid). pytest's CallSpec2 subset plugins read."""

    def __init__(self, params, id):
        self.params = params
        self.id = id


def _call_optional_arg(func, arg):
    """Call an xunit function with the node arg if it accepts one, else
    with no arguments (pytest's _call_with_optional_argument)."""
    try:
        nparams = len(inspect.signature(func).parameters)
    except (TypeError, ValueError):
        nparams = 1
    if nparams:
        func(arg)
    else:
        func()


class _SetupState:
    """pytest's SetupState (item.session._setupstate): a stack of collectors
    set up along the path to an item, with per-node finalizers. The engine
    runs items natively and never drives this, but in-process tests
    (test_runner's TestSetupState) construct items and exercise it directly."""

    def __init__(self):
        # Insertion-ordered: node -> (finalizers, cached setup exception).
        self.stack = {}

    def setup(self, item):
        from pytest._outcomes import OutcomeException

        needed = item.listchain()
        for col, (_fin, exc) in self.stack.items():
            assert col in needed, "previous item was not torn down properly"
            if exc:
                raise exc[0].with_traceback(exc[1])
        for col in needed[len(self.stack) :]:
            self.stack[col] = ([col.teardown], None)
            try:
                col.setup()
            except (Exception, OutcomeException) as exc:
                self.stack[col] = (self.stack[col][0], (exc, exc.__traceback__))
                raise

    def addfinalizer(self, finalizer, node):
        assert node and not isinstance(node, tuple)
        assert callable(finalizer)
        assert node in self.stack, (node, self.stack)
        self.stack[node][0].append(finalizer)

    def teardown_exact(self, nextitem):
        from pytest._outcomes import OutcomeException

        needed = (nextitem and nextitem.listchain()) or []
        exceptions = []
        while self.stack:
            if list(self.stack.keys()) == needed[: len(self.stack)]:
                break
            node, (finalizers, _exc) = self.stack.popitem()
            node_exceptions = []
            while finalizers:
                fin = finalizers.pop()
                try:
                    fin()
                except (Exception, OutcomeException) as e:
                    node_exceptions.append(e)
            if len(node_exceptions) == 1:
                exceptions.extend(node_exceptions)
            elif node_exceptions:
                exceptions.append(
                    BaseExceptionGroup(f"errors while tearing down {node!r}", node_exceptions[::-1])
                )
        if len(exceptions) == 1:
            raise exceptions[0]
        elif exceptions:
            raise BaseExceptionGroup("errors during test teardown", exceptions[::-1])


class _CollectorProxy:
    """Lightweight collector proxy for pytest_collectstart /
    pytest_make_collect_report / pytest_collectreport hooks.

    Mirrors real pytest's Collector with just enough surface
    for hook callers (path, nodeid, name, session, parent,
    own_markers, dynamic __class__.__name__).
    """

    def __init__(self, name, nodeid, path, session, parent=None, class_name="Collector"):
        self.name = name
        self.nodeid = nodeid
        self.path = path
        self.session = session
        self.config = getattr(session, "config", None)
        self.parent = parent
        self.own_markers = []
        self._keywords = {}
        self.fspath = path
        self.__class__ = type(class_name, (type(self),), {})

    def add_marker(self, marker, append=True):
        if append:
            self.own_markers.append(marker)
        else:
            self.own_markers.insert(0, marker)

    def iter_markers(self, name=None):
        for m in self.own_markers:
            if name is None or getattr(m, "name", None) == name:
                yield m

    def get_closest_marker(self, name):
        from itertools import chain

        return next(chain(self.iter_markers(name), iter(())), None)

    def listchain(self):
        node = self
        while node is not None:
            yield node
            node = node.parent

    @property
    def keywords(self):
        return self._keywords

    @property
    def ihook(self):
        from unittest.mock import MagicMock

        return MagicMock()

    def __repr__(self):
        return f"<{type(self).__name__} {self.nodeid!r}>"


class _ModuleCollector:
    """A minimal Module collector node for in-process SetupState tests: its
    setup()/teardown() run the module's xunit setup_module/teardown_module."""

    def __init__(self, module, session, path):
        self.module = module
        self.session = session
        self.path = path
        self.name = getattr(path, "name", str(path))
        self.nodeid = self.name
        self.own_markers = []

    def setup(self):
        fn = getattr(self.module, "setup_module", None)
        if fn is not None:
            _call_optional_arg(fn, self.module)

    def teardown(self):
        fn = getattr(self.module, "teardown_module", None)
        if fn is not None:
            _call_optional_arg(fn, self.module)


class _NodeSession:
    """Minimal stand-in for pytest's Session as seen from item.session."""

    def __init__(self, config):
        self.config = config
        # pytest-rerunfailures clears _setupstate.stack between reruns; each
        # pytest-rs attempt is a fresh run_one_body, so the stack is always
        # empty (an object exposing a mutable .stack is enough).
        self._setupstate = _SetupState()

    @property
    def shouldfail(self):
        return _session_state["shouldfail"]

    @shouldfail.setter
    def shouldfail(self, value):
        # Upstream issue #11706: once set, shouldfail cannot be unset.
        if value is False and _session_state["shouldfail"]:
            from pytest._warning_types import PytestWarning

            warnings.warn(
                PytestWarning(
                    "session.shouldfail cannot be unset after it has been set; ignoring."
                ),
                stacklevel=2,
            )
            return
        _session_state["shouldfail"] = value

    @property
    def shouldstop(self):
        return _session_state["shouldstop"]

    @shouldstop.setter
    def shouldstop(self, value):
        if value is False and _session_state["shouldstop"]:
            from pytest._warning_types import PytestWarning

            warnings.warn(
                PytestWarning(
                    "session.shouldstop cannot be unset after it has been set; ignoring."
                ),
                stacklevel=2,
            )
            return
        _session_state["shouldstop"] = value

    @property
    def path(self):
        return getattr(self.config, "rootpath", None) or getattr(self.config, "rootdir", None)

    @property
    def items(self):
        return _session_state["items"]

    @property
    def _rs_skipped_modules(self):
        return _session_state["skipped_modules"]

    @property
    def testscollected(self):
        if "testscollected" in _session_state:
            return _session_state["testscollected"]
        return len(_session_state["items"])

    def add_marker(self, marker, append=True):
        from pytest._marks import Mark, MarkDecorator

        if isinstance(marker, str):
            marker = Mark(marker)
        elif isinstance(marker, MarkDecorator):
            marker = marker.mark
        else:
            raise ValueError(f"is not a string or pytest.mark.* Marker object: {marker!r}")
        _session_state["session_markers"].append(marker)


def get_session_markers():
    return _session_state["session_markers"]


class Node(Item):
    def __init__(
        self,
        nodeid=None,
        name=None,
        marks=None,
        fixturenames=None,
        function=None,
        path=None,
        lineno=None,
        parent=None,
        **_,
    ):
        if marks is None and nodeid is None:
            # Generic node created via Node.from_parent(parent, name=...).
            # Set attributes directly (avoids Collector.__init__'s self.session
            # assignment, which would conflict with our read-only property).
            self.name = name or ""
            self.parent = parent
            self.config = getattr(parent, "config", None)
            self.path = path or getattr(parent, "path", None)
            self.own_markers = list(getattr(parent, "own_markers", []))
            self._nodeid = None
            return
        self.nodeid = nodeid
        self.name = name
        self.parent = parent
        self.own_markers = list(marks or [])
        self.fixturenames = list(fixturenames or [])
        self.function = function
        self.obj = function
        # Normalize to pathlib.Path like _NodeBase/File so item.path matches a
        # collector's .path (upstream Node.path is always a Path).
        self.path = pathlib.Path(str(path)) if path is not None else None
        self.lineno = lineno
        # The Python module/class this item was collected from. Reordering
        # plugins (pytest-randomly, pytest-order) shuffle by
        # item.module.__name__ and item.cls; the engine fills these in
        # make_node from the already-imported module and the collected class.
        self.module = None
        self.cls = None
        self.instance = None

    @property
    def keywords(self):
        if not hasattr(self, "_keywords"):
            self._keywords = _NodeKeywords(self)
        return self._keywords

    @property
    def session(self):
        """item.session shim: enough for plugins reaching
        item.session.config (e.g. pytest-timeout's session deadline).

        In-process collection (pytester) pins a single `_session_obj` per node
        so item.session — and hence item.session._setupstate — is stable across
        accesses; the engine path leaves it unset and gets a fresh stand-in."""
        pinned = getattr(self, "_session_obj", None)
        if pinned is not None:
            return pinned
        return _NodeSession(getattr(self, "config", None))

    @session.setter
    def session(self, value):
        self._session_obj = value

    @property
    def fspath(self):
        """Legacy py.path.local of this node's file (upstream Node.fspath);
        some plugins still use node.fspath.dirpath()."""
        import py

        return py.path.local(self.path)

    @property
    def ihook(self):
        """The shim pluginmanager's hook relay (upstream: the node's
        fspath-sensitive HookProxy)."""
        from pytest._pluginmanager import pluginmanager

        return pluginmanager.hook

    def warn(self, warning):
        """Issue a warning attributed to this item's definition site
        (pytest's Node.warn: warn_explicit with the item location)."""
        if not isinstance(warning, Warning):
            raise ValueError(
                f"warning must be an instance of Warning or subclass, got {warning!r} instead"
            )
        warnings.warn_explicit(
            warning,
            category=type(warning),
            filename=str(self.path) if self.path else "<unknown>",
            lineno=self.lineno or 0,
        )

    def get_closest_marker(self, name, default=None):
        for marker in self.own_markers:
            if marker.name == name:
                return marker
        return default

    def iter_markers(self, name=None):
        for marker in self.own_markers:
            if name is None or marker.name == name:
                yield marker

    def iter_markers_with_node(self, name=None):
        for marker in self.own_markers:
            if name is None or marker.name == name:
                yield self, marker

    def add_marker(self, marker, append=True):
        from pytest._marks import Mark, MarkDecorator

        if isinstance(marker, str):
            marker = Mark(marker)
        elif isinstance(marker, MarkDecorator):
            marker = marker.mark
        else:
            raise ValueError(f"is not a string or pytest.mark.* Marker object: {marker!r}")
        if append:
            self.own_markers.append(marker)
        else:
            self.own_markers.insert(0, marker)
        record_added_mark(marker)


class Function(Node):
    """Test-function node; the engine builds these for collected test items
    (conftest hooks isinstance-check pytest.Function)."""

    def __init__(self, *args, callobj=None, originalname=None, fixtureinfo=None, **kwargs):
        if callobj is not None and kwargs.get("function") is None and len(args) < 5:
            kwargs["function"] = callobj
        super().__init__(*args, **kwargs)
        if callobj is not None:
            self.obj = callobj
            self.function = callobj
        # originalname is the function's name without the parametrization id
        # (pytester.getitems / test_function_originalname): "test_func[1]" ->
        # "test_func".
        if originalname is not None:
            self.originalname = originalname
        else:
            name = getattr(self, "name", None)
            self.originalname = name.split("[")[0] if name else name
        if fixtureinfo is not None:
            self.__dict__["_fixtureinfo"] = fixtureinfo

    @property
    def _fixtureinfo(self):
        """FuncFixtureInfo for this item. Plugins like anyio access this to
        build modified items with additional fixture closures (e.g. anyio_backend).
        Returns a lazily-built default from fixturenames if not explicitly set."""
        try:
            return self.__dict__["_fixtureinfo"]
        except KeyError:
            from _pytest.fixtures import FuncFixtureInfo

            fixturenames = list(getattr(self, "fixturenames", []))
            fi = FuncFixtureInfo(
                argnames=tuple(fixturenames),
                initialnames=tuple(fixturenames),
                names_closure=fixturenames,
                name2fixturedefs={},
            )
            self.__dict__["_fixtureinfo"] = fi
            return fi

    @_fixtureinfo.setter
    def _fixtureinfo(self, value):
        self.__dict__["_fixtureinfo"] = value

    def __eq__(self, other):
        # upstream Node has no __eq__ so comparison is identity-based;
        # _NodeBase overrides it with nodeid which breaks Function since two
        # distinct Function objects for different callobj can share the same
        # nodeid.  Restore identity semantics here.
        return self is other

    def __hash__(self):
        return id(self)

    def __repr__(self):
        return f"<Function {getattr(self, 'name', '')}>"

    @property
    def location(self):
        """(relpath, lineno, domain) tuple — always delegates to reportinfo()
        so custom Function subclasses that override reportinfo() see their
        values here (matches pytest's Item.location behaviour).
        When Rust or plugins write ``node.location = value`` directly the
        setter caches it in ``_location``; ``reportinfo()`` can inspect that
        cache if needed, but the default implementation ignores it."""
        return self.reportinfo()

    @location.setter
    def location(self, value):
        # Allow direct assignment (e.g. pytest-rerunfailures sets
        # node.location before calling pytest_runtest_logstart).
        self.__dict__["_location"] = value

    def listchain(self):
        """The collector chain to this item ([module, item]); pytester.getitems
        attaches `_module_collector` so SetupState can set up module scope."""
        mod = getattr(self, "_module_collector", None)
        return [mod, self] if mod is not None else [self]

    def getmodpath(self, stopatmodule=True):
        """Return the dotted path from the module to this function/method.
        E.g. "TestX.testmethod_one" or "test_func". If stopatmodule=False,
        also prepends the module stem."""

        parts = self.nodeid.split("::")
        if stopatmodule:
            path_parts = parts[1:]
        else:
            mod_stem = pathlib.Path(parts[0]).stem
            path_parts = [mod_stem] + parts[1:]
        return ".".join(path_parts)

    def reportinfo(self):
        """(path, 0-based lineno, modpath) — mirrors pytest's Function.reportinfo.
        self.lineno stores the 1-based co_firstlineno, so the line is lineno-1."""
        lineno = self.lineno
        lineno0 = (lineno - 1) if lineno else 0
        return (self.path, lineno0, self.getmodpath())

    def setup(self):
        if self.module is not None:
            fn = getattr(self.module, "setup_function", None)
            if fn is not None and not hasattr(fn, "_pytestfixturefunction"):
                _call_optional_arg(fn, self.function)
        # Resolve the item's fixtures in-process, like pytest's Function.setup
        # (only the pytester-collected nodes carry a resolving request).
        request = getattr(self, "_request", None)
        fill = getattr(request, "_fillfixtures", None)
        if fill is not None:
            fill()

    def teardown(self):
        if self.module is not None:
            fn = getattr(self.module, "teardown_function", None)
            if fn is not None:
                _call_optional_arg(fn, self.function)


class FunctionDefinition(Function):
    """Stop-gap node used by Metafunc; not meant to be run as a test."""

    def runtest(self):
        raise RuntimeError("function definitions are not supposed to be run as tests")

    setup = runtest


# ---------------------------------------------------------------------------
# pytest_pycollect_makeitem hookwrapper support
# ---------------------------------------------------------------------------


class _CollectedClass:
    """Minimal class-node shim passed as the 'item' result to
    pytest_pycollect_makeitem hookwrappers (e.g. for extra_keyword_matches)."""

    def __init__(self, name):
        self.name = name
        self.extra_keyword_matches = set()


_pycollect_makeitem_hooks: list = []


def set_pycollect_hooks(hooks: list) -> None:
    global _pycollect_makeitem_hooks
    _pycollect_makeitem_hooks = list(hooks)


# The already-collected (items, fixturemanager) pair handed to a
# `pytest_cmdline_main` plugin (e.g. pytest-bdd's --generate-missing) via
# Session.perform_collect() — set by the engine right before firing that
# hook, consumed (and cleared) the first time perform_collect() is called.
_native_collection: tuple | None = None


def set_native_collection(items: list, fixturemanager: object) -> None:
    global _native_collection
    _native_collection = (items, fixturemanager)


def take_native_collection() -> tuple | None:
    global _native_collection
    result = _native_collection
    _native_collection = None
    return result


def fire_makeitem_for_function(
    nodeid: str,
    func_name: str,
    callobj: object,
    path_str: str,
    lineno: int,
    is_test_func: bool = True,
) -> object | None:
    """Fire pytest_pycollect_makeitem hookwrappers for a collected member.

    Mirrors what pytest's ``Module.collect()`` does via pluggy:
    - Plain (non-wrapper) firstresult impls may return a custom node.
    - Wrapper impls surround the plain result; if ``is_test_func`` is True
      a default ``Function`` node is used as the plain result (so wrappers
      that iterate the result can attach attributes like ``_some123``).
      If ``is_test_func`` is False the plain result is ``None`` — wrapper
      hooks that check ``if result:`` will skip unrecognised members.

    Returns the node (or list of nodes) if any hook claimed it, else
    ``None`` to fall through to Rust's default collection path."""
    if not _pycollect_makeitem_hooks:
        return None

    func_node_name = nodeid.rsplit("::", 1)[-1]
    import pathlib as _pathlib

    kwargs = {
        "name": func_name,
        "obj": callobj,
        "collector": None,
    }
    wrappers = []
    plain_result = None
    for func in _pycollect_makeitem_hooks:
        opts = getattr(func, "pytest_impl", None) or {}
        is_wrapper = bool(opts.get("wrapper") or opts.get("hookwrapper"))
        old_style = bool(opts.get("hookwrapper"))
        if is_wrapper:
            try:
                sig_params = set(inspect.signature(func).parameters)
                call_kw = {k: v for k, v in kwargs.items() if k in sig_params}
                gen = func(**call_kw)
            except Exception:
                continue
            if not inspect.isgenerator(gen):
                continue
            try:
                next(gen)
            except StopIteration:
                continue
            wrappers.append((gen, old_style))
        else:
            # plain firstresult impl — may return a custom node
            try:
                sig_params = set(inspect.signature(func).parameters)
                call_kw = {k: v for k, v in kwargs.items() if k in sig_params}
                res = func(**call_kw)
                if res is not None:
                    plain_result = res
                    break
            except Exception:
                continue

    if not wrappers and plain_result is None:
        return None

    # Determine the inner "plain" result that wrappers will receive:
    # - if a plain impl returned something, use that;
    # - else if this member looks like a test function, synthesise a default
    #   Function node (so wrapper hooks that iterate the result can attach
    #   extra attributes like _some123);
    # - else pass None — wrappers that check ``if result:`` will skip it.
    if plain_result is not None:
        result = plain_result
    elif is_test_func:
        plain_node = Function(
            nodeid, func_node_name, [], [], callobj, _pathlib.Path(path_str), lineno
        )
        result = [plain_node]
    else:
        result = None

    for gen, old_style in reversed(wrappers):
        try:
            if old_style:
                from pytest._pluginmanager import _Result

                outcome = _Result(result)
                try:
                    gen.send(outcome)
                except StopIteration:
                    pass
                finally:
                    gen.close()
                result = outcome.get_result()
            else:
                try:
                    gen.send(result)
                except StopIteration as stop:
                    if stop.value is not None:
                        result = stop.value
                finally:
                    gen.close()
        except Exception:
            pass

    return result


def fire_makeitem_for_class(name: str) -> set:
    """Fire pytest_pycollect_makeitem hookwrappers for a class, returning
    extra_keyword_matches set that plugins may have populated."""
    node = _CollectedClass(name)
    if not _pycollect_makeitem_hooks:
        return node.extra_keyword_matches

    kwargs = {"name": name, "obj": None, "collector": None}
    started = []
    for func in _pycollect_makeitem_hooks:
        opts = getattr(func, "pytest_impl", None) or {}
        old_style = bool(opts.get("hookwrapper"))
        try:
            sig_params = set(inspect.signature(func).parameters)
            call_kw = {k: v for k, v in kwargs.items() if k in sig_params}
            gen = func(**call_kw)
        except Exception:
            continue
        if not inspect.isgenerator(gen):
            continue
        try:
            next(gen)
        except StopIteration:
            continue
        started.append((gen, old_style))

    result = node
    for gen, old_style in reversed(started):
        try:
            if old_style:
                from pytest._pluginmanager import _Result

                outcome = _Result(result)
                try:
                    gen.send(outcome)
                except StopIteration:
                    pass
                finally:
                    gen.close()
                result = outcome.get_result()
            else:
                try:
                    gen.send(result)
                except StopIteration as stop:
                    if stop.value is not None:
                        result = stop.value
                finally:
                    gen.close()
        except Exception:
            pass

    if hasattr(result, "extra_keyword_matches"):
        return result.extra_keyword_matches
    return set()
