"""The `request.node` object: a minimal pytest Item surface."""

# Marks added at runtime (node.add_marker / request.applymarker) for the
# currently running item; the engine re-evaluates xfail against these and
# clears the list per item.
_added_marks: list = []


def record_added_mark(marker):
    mark = getattr(marker, "mark", marker)
    name = getattr(mark, "name", None)
    if isinstance(name, str):
        _added_marks.append((name, mark))


def added_marks():
    return list(_added_marks)


def clear_added_marks():
    _added_marks.clear()


class Collector:
    """Base for custom collectors (pytest.File subclasses returned from
    pytest_collect_file) and items. Enough of pytest's Node API for plugins
    like pytest-ruff / pytest-mypy: from_parent, config/path/nodeid, markers."""

    class CollectError(Exception):
        """An error during collection, shown without a traceback."""

    def __init__(
        self,
        *,
        name=None,
        parent=None,
        config=None,
        path=None,
        fspath=None,
        nodeid=None,
        session=None,
        **kwargs,
    ):
        import pathlib

        self.parent = parent
        self.config = config if config is not None else getattr(parent, "config", None)
        self.session = session if session is not None else getattr(parent, "session", None)
        if path is None and fspath is not None:
            path = pathlib.Path(str(fspath))
        if path is None and parent is not None:
            path = getattr(parent, "path", None)
        self.path = pathlib.Path(str(path)) if path is not None else None
        self.name = name if name is not None else (self.path.name if self.path is not None else "")
        self.own_markers = []
        self._nodeid = nodeid if nodeid is not None else self._compute_nodeid()

    @classmethod
    def from_parent(cls, parent, **kwargs):
        """Construct a child node under `parent` (pytest's Node.from_parent)."""
        return cls(parent=parent, **kwargs)

    def _compute_nodeid(self):
        parent_id = getattr(self.parent, "nodeid", None)
        if isinstance(self, Item) and parent_id:
            return f"{parent_id}::{self.name}"
        if self.path is not None and self.config is not None:
            import pathlib

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
            raise ValueError(
                f"is not a string or pytest.mark.* Marker object: {marker!r}"
            )
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
        if not isinstance(other, Collector):
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


def run_custom_item(item):
    """Run a custom collector Item (setup/runtest/teardown) and return a list
    of (when, outcome, longrepr) tuples for the engine. Failures get their
    longrepr from Item.repr_failure plus any pytest_exception_interact hook
    (pytest-ruff sets the ruff error message there)."""
    import traceback

    from pytest._outcomes import Skipped

    def _excinfo(exc):
        class _ExcInfo:
            def __init__(self):
                self.value = exc
                self.type = type(exc)
                self.typename = type(exc).__name__

            def exconly(self, tryshort=False):
                return f"{type(exc).__name__}: {exc}"

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
    try:
        item.setup()
    except Skipped as exc:
        reports.append(("setup", "skipped", str(getattr(exc, "msg", exc) or exc)))
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
        reports.append(("call", "passed", None))
    except Skipped as exc:
        reports.append(("call", "skipped", str(getattr(exc, "msg", exc) or exc)))
    except BaseException as exc:  # noqa: BLE001
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
        import pathlib

        rootdir = getattr(config, "rootpath", None) or getattr(config, "rootdir", None)
        rootdir = pathlib.Path(str(rootdir)) if rootdir is not None else pathlib.Path.cwd()
        session = cls(name=rootdir.name or ".", config=config, path=rootdir, nodeid="")
        session.parent = None
        return session

    def perform_collect(self, args=None, genitems=False):
        import pathlib

        if not args or args == [self.nodeid] or args == [""]:
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
                    rel = str(file_part.resolve().relative_to(self.path.resolve())).replace("\\", "/")
                except ValueError:
                    rel = file_part.name
                dir_node = Dir(name=file_part.name, config=self.config, path=file_part, nodeid=rel)
                dir_node.parent = self
                results.append(dir_node)
            elif file_part.is_file():
                parent_dir = file_part.parent
                is_pkg = (parent_dir / "__init__.py").is_file()
                try:
                    parent_rel = str(parent_dir.resolve().relative_to(self.path.resolve())).replace("\\", "/")
                except ValueError:
                    parent_rel = parent_dir.name
                if is_pkg:
                    mid_node = Package(name=parent_dir.name, config=self.config, path=parent_dir, nodeid=parent_rel)
                    mid_node.parent = self
                else:
                    mid_node = Dir(name=parent_dir.name, config=self.config, path=parent_dir, nodeid=parent_rel)
                    mid_node.parent = self
                try:
                    file_rel = str(file_part.resolve().relative_to(self.path.resolve())).replace("\\", "/")
                except ValueError:
                    file_rel = file_part.name
                mod_node = File(name=file_part.name, config=self.config, path=file_part, nodeid=file_rel)
                mod_node.parent = mid_node
                results.append(mod_node)
        return results


class Class(Collector):
    """A class collector stand-in for pytest_collectstart: carries .obj (the
    test class) and collects markers via add_marker, propagated to its items."""

    def __init__(self, *, obj=None, **kwargs):
        super().__init__(**kwargs)
        self.obj = obj


class Item(Collector):
    """Base test item for custom collectors. Subclasses override runtest()
    (and optionally setup()/teardown()/repr_failure()/reportinfo())."""

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

    def collect(self):
        return []


class Package(File):
    """Directory-level collector for Python packages (has __init__.py).
    Alias used by upstream isinstance checks (e.g. pytest.Package in test_collection)."""


class Dir(Collector):
    """Directory-level collector for plain directories (no __init__.py)."""


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
        self.path = path
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
        """Mark names (plus the node name) as a mapping — pytest's
        node.keywords, for the common `"xfail" in item.keywords` probes."""
        keywords = {self.name: True}
        for marker in self.own_markers:
            keywords[marker.name] = marker
        return keywords

    def warn(self, warning):
        """Issue a warning attributed to this item's definition site
        (pytest's Node.warn: warn_explicit with the item location)."""
        import warnings

        warnings.warn_explicit(
            warning,
            category=None,
            filename=self.path or "<unknown>",
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


def get_session_keywords() -> dict:
    """Return the session-level keywords dict (mutable, persists across items).
    Used by session-scoped fixtures' request.keywords."""
    return _session_state["session_keywords"]


def session_obj_overrides():
    """(nodeid, obj) for items whose `obj` a plugin swapped after they were
    published (pytest-run-parallel wraps test functions for threaded
    repeats); the engine writes these back into its own items."""
    return [
        (node.nodeid, node.obj) for node in _session_state["items"] if node.obj is not node.function
    ]


class _CallSpec:
    """item.callspec for parametrized items: the param values and the id (the
    "[a-1]" bracket of the nodeid). pytest's CallSpec2 subset plugins read."""

    def __init__(self, params, id):
        self.params = params
        self.id = id


def _call_optional_arg(func, arg):
    """Call an xunit function with the node arg if it accepts one, else
    with no arguments (pytest's _call_with_optional_argument)."""
    import inspect

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
                    BaseExceptionGroup(
                        f"errors while tearing down {node!r}", node_exceptions[::-1]
                    )
                )
        if len(exceptions) == 1:
            raise exceptions[0]
        elif exceptions:
            raise BaseExceptionGroup("errors during test teardown", exceptions[::-1])


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
            import warnings

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
            import warnings

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
    def items(self):
        return _session_state["items"]

    @property
    def testscollected(self):
        return len(_session_state["items"])

    def add_marker(self, marker, append=True):
        from pytest._marks import Mark, MarkDecorator

        if isinstance(marker, str):
            marker = Mark(marker)
        elif isinstance(marker, MarkDecorator):
            marker = marker.mark
        else:
            raise ValueError(
                f"is not a string or pytest.mark.* Marker object: {marker!r}"
            )
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
        self.own_markers = list(marks or [])
        self.fixturenames = list(fixturenames or [])
        self.function = function
        self.obj = function
        self.path = path
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
        """Mark names (plus the node name) as a mapping — pytest's
        node.keywords, for the common `"xfail" in item.keywords` probes."""
        keywords = {self.name: True}
        for marker in self.own_markers:
            keywords[marker.name] = marker
        return keywords

    @property
    def session(self):
        """item.session shim: enough for plugins reaching
        item.session.config (e.g. pytest-timeout's session deadline)."""
        return _NodeSession(getattr(self, "config", None))

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
        import warnings

        warnings.warn_explicit(
            warning,
            category=None,
            filename=self.path or "<unknown>",
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
            raise ValueError(
                f"is not a string or pytest.mark.* Marker object: {marker!r}"
            )
        if append:
            self.own_markers.append(marker)
        else:
            self.own_markers.insert(0, marker)
        record_added_mark(marker)


class Function(Node):
    """Test-function node; the engine builds these for collected test items
    (conftest hooks isinstance-check pytest.Function)."""

    def listchain(self):
        """The collector chain to this item ([module, item]); pytester.getitems
        attaches `_module_collector` so SetupState can set up module scope."""
        mod = getattr(self, "_module_collector", None)
        return [mod, self] if mod is not None else [self]

    def getmodpath(self, stopatmodule=True):
        """Return the dotted path from the module to this function/method.
        E.g. "TestX.testmethod_one" or "test_func". If stopatmodule=False,
        also prepends the module stem."""
        import pathlib

        parts = self.nodeid.split("::")
        if stopatmodule:
            path_parts = parts[1:]
        else:
            mod_stem = pathlib.Path(parts[0]).stem
            path_parts = [mod_stem] + parts[1:]
        return ".".join(path_parts)

    def setup(self):
        if self.module is not None:
            fn = getattr(self.module, "setup_function", None)
            if fn is not None:
                _call_optional_arg(fn, self.function)

    def teardown(self):
        if self.module is not None:
            fn = getattr(self.module, "teardown_function", None)
            if fn is not None:
                _call_optional_arg(fn, self.function)


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


def fire_makeitem_for_class(name: str) -> set:
    """Fire pytest_pycollect_makeitem hookwrappers for a class, returning
    extra_keyword_matches set that plugins may have populated."""
    import inspect
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
