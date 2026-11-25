"""pytest_collectstart / pytest_make_collect_report / pytest_collectreport.

pytest-rs collects natively. Once collection settles, the engine hands the
collected tree here and every collector is replayed through the shim
pluginmanager in upstream's order: the Session first (its report lists the
nodes the command-line arguments matched, after the collectors on the way to
them were collected), then a depth-first walk of each matched node
(``Session.genitems``): collectstart, make_collect_report, the children,
collectreport. Conftests, plugin modules and instances (a replacement
terminal reporter) and a pytester HookRecorder all receive these calls from
this one path, each exactly once.

``pytest_make_collect_report`` only observes: its default implementation
returns the report built from the native collection, and a report a hookimpl
returns does not change what was collected.
"""

from __future__ import annotations

import os
import pathlib
import unittest
from typing import Any

from pytest._pluginmanager import pluginmanager


class CollectEntry:
    """One collector of the replayed tree (a Dir, Package, Module, Class,
    doctest file or a custom collector a ``pytest_collect_file`` returned)."""

    def __init__(
        self,
        kind: str,
        nodeid: str,
        name: str,
        path: pathlib.Path | None,
        parent: CollectEntry | None,
    ) -> None:
        self.kind = kind
        self.nodeid = nodeid
        self.name = name
        self.path = path
        self.parent = parent
        # CollectEntry, or an int: an index into the engine's item list.
        self.children: list[Any] = []
        self.outcome = "passed"
        self.longrepr: Any = None
        self.obj: Any = None
        self.collector: Any = None
        self.initial_marks = 0

    def chain(self) -> list[CollectEntry]:
        """This entry and its ancestors, outermost first."""
        entry: CollectEntry | None = self
        out = []
        while entry is not None:
            out.append(entry)
            entry = entry.parent
        return out[::-1]

    def item_indices(self) -> list[int]:
        return [
            index
            for child in self.children
            for index in (child.item_indices() if isinstance(child, CollectEntry) else [child])
        ]

    def owner_of(self, index: int) -> CollectEntry | None:
        """The entry whose children hold item `index`."""
        if index in self.children:
            return self
        for child in self.children:
            if isinstance(child, CollectEntry):
                owner = child.owner_of(index)
                if owner is not None:
                    return owner
        return None


class CollectTree:
    """The collector tree rebuilt from the native collection: items (in
    collection order), failed and skipped files, and skipped directories."""

    def __init__(self, rootdir: pathlib.Path) -> None:
        self.rootdir = rootdir
        self.root = CollectEntry(self._dir_kind(rootdir), ".", rootdir.name, rootdir, None)
        self.dirs: dict[str, CollectEntry] = {".": self.root}
        # nodeid -> the collectors with that nodeid (a Module and a
        # DoctestModule share one).
        self.by_nodeid: dict[str, list[CollectEntry]] = {".": [self.root]}
        self.files: dict[tuple[str, Any], CollectEntry] = {}
        # Collection errors no file or directory owns (a hook raising).
        self.orphan_failures: list[tuple[str, Any]] = []

    @staticmethod
    def _dir_kind(path: pathlib.Path) -> str:
        return "Package" if (path / "__init__.py").is_file() else "Dir"

    @staticmethod
    def split_nodeid(nodeid: str) -> tuple[str, list[str]]:
        """(file part, [class names..., item name]) of an item nodeid; the
        parametrization id may itself contain '::'."""
        head, bracket, _ = nodeid.partition("[")
        file_part, _, rest = head.partition("::")
        names = rest.split("::") if rest else []
        if bracket and names:
            names[-1] = names[-1] + nodeid[len(head) :]
        return file_part, names

    def _add(self, parent: CollectEntry, entry: CollectEntry) -> CollectEntry:
        parent.children.append(entry)
        self.by_nodeid.setdefault(entry.nodeid, []).append(entry)
        return entry

    def dir_entry(self, dir_nodeid: str) -> CollectEntry:
        entry = self.dirs.get(dir_nodeid)
        if entry is not None:
            return entry
        parent_nodeid, _, name = dir_nodeid.rpartition("/")
        parent = self.dir_entry(parent_nodeid or ".")
        path = self.rootdir / dir_nodeid
        entry = CollectEntry(self._dir_kind(path), dir_nodeid, name, path, parent)
        self.dirs[dir_nodeid] = entry
        return self._add(parent, entry)

    def parent_dir_of(self, file_nodeid: str) -> CollectEntry:
        if file_nodeid.startswith(("/", "..")) or "/" not in file_nodeid:
            return self.root
        return self.dir_entry(file_nodeid.rpartition("/")[0])

    def file_entry(self, file_nodeid: str, kind: str, key: Any = None) -> CollectEntry:
        cache_key = (file_nodeid, kind if key is None else key)
        entry = self.files.get(cache_key)
        if entry is None:
            parent = self.parent_dir_of(file_nodeid)
            path = self.rootdir / file_nodeid if file_nodeid else self.rootdir
            entry = CollectEntry(kind, file_nodeid, path.name, path, parent)
            self.files[cache_key] = entry
            self._add(parent, entry)
        return entry

    def add_item(self, index: int, nodeid: str, node: Any, kind: str) -> None:
        file_part, names = self.split_nodeid(nodeid)
        if kind == "custom":
            self._add_custom_item(index, file_part, node)
            return
        if kind == "doctest":
            entry_kind = "DoctestModule" if file_part.endswith(".py") else "DoctestTextfile"
            self.file_entry(file_part, entry_kind).children.append(index)
            return
        module = self.file_entry(file_part, "Module")
        if module.obj is None:
            module.obj = getattr(node, "module", None)
        parent = module
        for depth, class_name in enumerate(names[:-1]):
            class_nodeid = "::".join([file_part, *names[: depth + 1]])
            found = next(
                (
                    child
                    for child in parent.children
                    if isinstance(child, CollectEntry) and child.nodeid == class_nodeid
                ),
                None,
            )
            if found is None:
                found = self._add(
                    parent, CollectEntry("Class", class_nodeid, class_name, module.path, parent)
                )
                found.obj = self._class_obj(parent, class_name, node, depth == len(names) - 2)
            parent = found
        parent.children.append(index)

    @staticmethod
    def _class_obj(parent: CollectEntry, name: str, node: Any, innermost: bool) -> Any:
        if innermost:
            cls = getattr(node, "cls", None)
            if cls is not None and getattr(cls, "__name__", None) == name:
                return cls
        owner = parent.obj
        return getattr(owner, name, None) if owner is not None else None

    def _add_custom_item(self, index: int, file_part: str, node: Any) -> None:
        """A custom collector's item: its real collectors (the ``pytest.File``
        a ``pytest_collect_file`` hookimpl returned, and any collector below
        it) are reported as themselves."""
        from pytest._node import Collector, Dir, Package, Session

        collectors = []
        parent = getattr(node, "parent", None)
        while (
            isinstance(parent, Collector)
            and not isinstance(parent, Session | Dir | Package)
            and getattr(parent, "nodeid", "") not in ("", ".")
        ):
            collectors.append(parent)
            parent = getattr(parent, "parent", None)
        if not collectors:
            self.file_entry(file_part, "Module").children.append(index)
            return
        collectors.reverse()
        top = collectors[0]
        entry = self.file_entry(getattr(top, "nodeid", None) or file_part, "custom", id(top))
        entry.collector = top
        for collector in collectors[1:]:
            found = next(
                (
                    child
                    for child in entry.children
                    if isinstance(child, CollectEntry) and child.collector is collector
                ),
                None,
            )
            if found is None:
                found = CollectEntry(
                    "custom", collector.nodeid, collector.name, collector.path, entry
                )
                found.collector = collector
                self._add(entry, found)
            entry = found
        entry.children.append(index)

    def add_failure(self, nodeid: str, longrepr: Any) -> None:
        if not nodeid or nodeid == ".":
            self.orphan_failures.append((nodeid, longrepr))
            return
        if (self.rootdir / nodeid).is_dir():
            entry = self.dir_entry(nodeid)
        else:
            entry = self.file_entry(nodeid, "Module")
        entry.outcome = "failed"
        entry.longrepr = longrepr

    def add_skip(self, nodeid: str, longrepr: Any, is_dir: bool, kind: str = "Module") -> None:
        if is_dir or nodeid in ("", "."):
            entry = self.dir_entry(nodeid or ".")
        else:
            entry = self.file_entry(nodeid, kind)
        entry.outcome = "skipped"
        entry.longrepr = longrepr

    def add_file_skip(self, file_nodeid: str, longrepr: Any) -> None:
        """``pytest.skip()`` inside ``pytest_collect_file``: the directory
        collecting the file skips, when nothing else in it collected."""
        entry = self.parent_dir_of(file_nodeid)
        if not entry.children:
            entry.outcome = "skipped"
            entry.longrepr = longrepr

    def sort_dirs(self, entry: CollectEntry) -> None:
        """Directory entries in upstream's order: sorted by name."""
        if entry.kind in ("Dir", "Package"):
            entry.children.sort(
                key=lambda child: child.name if isinstance(child, CollectEntry) else ""
            )
        for child in entry.children:
            if isinstance(child, CollectEntry):
                self.sort_dirs(child)


class CollectReportDispatch:
    """Replays the native collection through the three collection hooks."""

    HOOKS = ("pytest_collectstart", "pytest_make_collect_report", "pytest_collectreport")
    #: Collector classes named like upstream's (UnitTestCase, DoctestModule).
    _CLASSES: dict[str, type] = {}

    @staticmethod
    def excluded_plugins() -> list[Any]:
        """Plugins that never receive these calls: the stand-in
        TerminalReporter while pytest-rs renders the terminal itself (or in
        a -n worker, whose stdout is the IPC pipe)."""
        from pytest import _reporter

        reporter = pluginmanager._names.get("terminalreporter")
        if reporter is not None and _reporter.replacement() is None:
            return [reporter]
        return []

    @classmethod
    def wanted(cls) -> bool:
        """True when anything would receive a call: a HookRecorder, or an
        implementation of one of the hooks besides the excluded ones."""
        if pluginmanager._call_monitors:
            return True
        excluded = cls.excluded_plugins()
        return any(
            not any(impl == getattr(plugin, name, None) for plugin in excluded)
            for name in cls.HOOKS
            for impl in pluginmanager._reversed_impls(name)
        )

    @classmethod
    def _collector_class(cls, name: str, base: type) -> Any:
        klass = cls._CLASSES.get(name)
        if klass is None or klass.__bases__ != (base,):
            klass = cls._CLASSES[name] = type(name, (base,), {"__qualname__": name})
        return klass

    @classmethod
    def build_collector(cls, entry: CollectEntry, session: Any, config: Any) -> Any:
        if entry.collector is not None:
            return entry.collector
        from pytest._marks import get_unpacked_marks
        from pytest._node import Dir, File, Package, PyClassCollector, PyModuleCollector

        parent = (
            cls.build_collector(entry.parent, session, config)
            if entry.parent is not None
            else session
        )
        marks: list = []
        node: Any
        if entry.kind in ("Dir", "Package"):
            klass: Any = Package if entry.kind == "Package" else Dir
            node = klass(name=entry.name, path=entry.path, nodeid=entry.nodeid, parent=parent)
        elif entry.kind == "Module":
            node = PyModuleCollector._create(
                name=entry.name,
                parent=parent,
                config=config,
                session=session,
                path=entry.path,
                nodeid=entry.nodeid,
            )
            node.obj = entry.obj
            if entry.obj is not None:
                marks = list(get_unpacked_marks(entry.obj))
        elif entry.kind == "Class":
            klass = PyClassCollector
            if isinstance(entry.obj, type) and issubclass(entry.obj, unittest.TestCase):
                klass = cls._collector_class("UnitTestCase", PyClassCollector)
            node = klass._create(name=entry.name, parent=parent, nodeid=entry.nodeid, obj=entry.obj)
            if entry.obj is not None:
                marks = list(get_unpacked_marks(entry.obj))
        else:
            node = cls._collector_class(entry.kind, File)._create(
                name=entry.name,
                parent=parent,
                config=config,
                session=session,
                path=entry.path,
                nodeid=entry.nodeid,
            )
        node.own_markers = marks
        entry.initial_marks = len(marks)
        entry.collector = node
        return node

    def __init__(self, config: Any, tree: CollectTree, nodes: list[Any]) -> None:
        from pytest._node import _parent_chain_session

        self.config = config
        self.tree = tree
        self.nodes = nodes
        self.session = _parent_chain_session(config)
        if getattr(self.session, "session", None) is None:
            self.session.session = self.session
        self.excluded = self.excluded_plugins()
        self.numcollected = 0
        # Collectors already collected (upstream's _collection_cache): the
        # ones on the way to an argument are not collected again.
        self.collected: set[int] = set()
        self.conftest_dirs: dict[int, list[pathlib.Path]] = {}
        self.intermediates: list[CollectEntry] = []

    # ---- hook calls -----------------------------------------------------

    def _conftest_dirs(self, mod: Any) -> list[pathlib.Path]:
        dirs = self.conftest_dirs.get(id(mod))
        if dirs is None:
            file = getattr(mod, "__file__", None)
            dirs = []
            if file:
                directory = pathlib.Path(file).parent
                dirs = [directory, pathlib.Path(os.path.realpath(directory))]
            self.conftest_dirs[id(mod)] = dirs
        return dirs

    def _out_of_scope(self, path: pathlib.Path | None, is_dir: bool) -> list[Any]:
        """Conftests not in effect at `path` (upstream's gethookproxy)."""
        conftests = pluginmanager._conftest_plugins
        if not conftests or path is None:
            return []
        directory = path if is_dir else path.parent
        ancestors = set()
        for candidate in (directory, pathlib.Path(os.path.realpath(directory))):
            ancestors.add(candidate)
            ancestors.update(candidate.parents)
        return [
            mod
            for mod in conftests
            if not any(conftest_dir in ancestors for conftest_dir in self._conftest_dirs(mod))
        ]

    def _call(self, name: str, entry: CollectEntry | None, extra: list | None = None, **kw):
        path: pathlib.Path | None = self.tree.rootdir
        is_dir = True
        if entry is not None:
            path, is_dir = entry.path, entry.kind in ("Dir", "Package")
        exclude = [*self.excluded, *self._out_of_scope(path, is_dir)]
        return getattr(pluginmanager.hook, name).call_for_node(exclude, extra or [], **kw)

    # ---- reports ----------------------------------------------------------

    def _child_object(self, child: Any) -> Any:
        if isinstance(child, CollectEntry):
            return self.build_collector(child, self.session, self.config)
        return self.nodes[child]

    @staticmethod
    def report(nodeid: str, outcome: Any, longrepr: Any, result: list) -> Any:
        from _pytest.reports import CollectReport

        fspath = nodeid.split("::")[0]
        return CollectReport(
            nodeid=nodeid,
            outcome=outcome,
            longrepr=longrepr,
            result=result,
            location=(fspath, None, fspath),
        )

    def _make_report(self, entry: CollectEntry | None, collector: Any, report: Any, on_collect):
        """pytest_make_collect_report: the default (trylast) implementation
        "collects" (`on_collect`) and returns the native report."""

        def pytest_make_collect_report(collector: Any) -> Any:
            on_collect()
            return report

        pytest_make_collect_report.pytest_impl = {"trylast": True}  # type: ignore[attr-defined]
        result = self._call(
            "pytest_make_collect_report",
            entry,
            [pytest_make_collect_report],
            collector=collector,
        )
        return report if result is None else result

    def _record_makeitems(self, entry: CollectEntry, collector: Any) -> None:
        """Upstream calls pytest_pycollect_makeitem while a Module/Class
        collects; pytest-rs already called the implementations during
        collection, so only a HookRecorder (or a plugin recording calls,
        through its `_rs_record_hook`) hears of it here, in place."""
        recorders = [
            record
            for record in (getattr(p, "_rs_record_hook", None) for p in pluginmanager._plugins)
            if callable(record)
        ]
        if entry.kind not in ("Module", "Class") or not (pluginmanager._call_monitors or recorders):
            return
        seen = set()
        for child in entry.children:
            if isinstance(child, CollectEntry):
                name = child.name
            else:
                node = self.nodes[child]
                name = (
                    getattr(node, "originalname", None)
                    or str(getattr(node, "name", "")).partition("[")[0]
                )
            if name in seen:
                continue
            seen.add(name)
            kwargs = {"collector": collector, "name": name, "obj": getattr(entry.obj, name, None)}
            pluginmanager.record_hook("pytest_pycollect_makeitem", kwargs)
            for record in recorders:
                record("pytest_pycollect_makeitem", kwargs)

    def _collect_one(self, entry: CollectEntry) -> Any:
        """collectstart + make_collect_report for one collector."""
        collector = self.build_collector(entry, self.session, self.config)
        self._call("pytest_collectstart", entry, collector=collector)
        result = (
            [self._child_object(child) for child in entry.children]
            if entry.outcome == "passed"
            else []
        )
        report = self.report(entry.nodeid, entry.outcome, entry.longrepr, result)
        self.collected.add(id(entry))
        return self._make_report(
            entry, collector, report, lambda: self._record_makeitems(entry, collector)
        )

    def _collectreport(self, report: Any, entry: CollectEntry | None) -> None:
        self._call("pytest_collectreport", entry, report=report)
        self.numcollected += sum(
            1 for x in getattr(report, "result", None) or () if self.is_item(x)
        )

    @staticmethod
    def is_item(obj: Any) -> bool:
        from pytest._node import Item

        return isinstance(obj, Item) or bool(getattr(obj, "_pytest_doctest_item", False))

    def _collect_intermediates(self) -> None:
        """Session.collect: the collectors on the way to each argument."""
        for entry in self.intermediates:
            if id(entry) not in self.collected:
                self._collect_one(entry)

    def genitems(self, child: Any) -> None:
        if not isinstance(child, CollectEntry):
            return
        is_file = child.kind not in ("Dir", "Package", "Class") and child.kind != "custom"
        if id(child) in self.collected and not is_file:
            # Collected already on the way to another argument: upstream's
            # collection cache neither collects nor reports it again.
            for sub in child.children:
                self.genitems(sub)
            return
        report = self._collect_one(child)
        if child.outcome == "passed":
            for sub in child.children:
                self.genitems(sub)
        self._collectreport(report, child)

    def run(self, matched: list[Any], intermediates: list[CollectEntry]) -> None:
        session = self.session
        self.intermediates = intermediates
        self._call("pytest_collectstart", None, collector=session)
        report = self.report("", "passed", None, [self._child_object(c) for c in matched])
        report = self._make_report(None, session, report, self._collect_intermediates)
        self._collectreport(report, None)
        for nodeid, longrepr in self.tree.orphan_failures:
            self._collectreport(self.report(nodeid, "failed", longrepr, []), None)
        for child in matched:
            self.genitems(child)

    def added_marks(self) -> list[tuple[int, list]]:
        """Markers hooks added to collectors (pytest-django's Class tags in
        pytest_collectstart), for the items below each collector."""
        out: dict[int, list] = {}
        pending = [self.tree.root]
        while pending:
            entry = pending.pop()
            collector = entry.collector
            if collector is not None and entry.kind != "custom":
                added = list(getattr(collector, "own_markers", [])[entry.initial_marks :])
                if added:
                    for index in entry.item_indices():
                        out.setdefault(index, []).extend(added)
            pending.extend(child for child in entry.children if isinstance(child, CollectEntry))
        return sorted(out.items())

    # ---- engine entry points --------------------------------------------

    @staticmethod
    def match_args(
        tree: CollectTree, args: list[str], item_nodeids: list[str]
    ) -> tuple[list[Any], list[CollectEntry]]:
        """The nodes the arguments matched (the Session's report result)
        and, in order, the collectors on the way to them. Without arguments
        (or when one matches nothing here) the root directory is the match."""
        if not args:
            return [tree.root], []
        matched: list[Any] = []
        intermediates: list[CollectEntry] = []
        for arg in args:
            found: list[Any] = list(tree.by_nodeid.get(arg, []))
            if found:
                parents = [entry.parent for entry in found]
            else:
                found = [
                    index
                    for index, nodeid in enumerate(item_nodeids)
                    if nodeid == arg or nodeid.startswith(arg + "[")
                ]
                if not found:
                    return [tree.root], []
                parents = [tree.root.owner_of(index) for index in found]
            for parent in parents:
                for entry in parent.chain() if parent is not None else []:
                    if entry not in intermediates:
                        intermediates.append(entry)
            matched.extend(child for child in found if child not in matched)
        return matched, intermediates

    @staticmethod
    def skip_longrepr(nodeid: str, reason: str, location: str) -> tuple:
        loc_file, sep, lineno = location.rpartition(":")
        if not sep or not lineno.isdigit():
            return (location or nodeid, 1, f"Skipped: {reason}")
        return (loc_file, int(lineno), f"Skipped: {reason}")

    @classmethod
    def dispatch(
        cls,
        config: Any,
        rootdir: str,
        args: list[str],
        items: list[tuple[str, Any, str]],
        failures: list[tuple[str, Any]],
        skips: list[tuple[str, str, str, bool]],
        doctest_skips: list[tuple[str, str]],
        file_skips: list[tuple[str, str]],
    ) -> list[tuple[int, list]]:
        """Replay the collection; returns (item index, markers) for the
        markers collection hooks added to collectors. A hookimpl's
        exception propagates, carrying the number of items reported so far
        as ``_rs_numcollected``."""
        tree = CollectTree(pathlib.Path(rootdir))
        nodes = [node for _, node, _ in items]
        for index, (nodeid, node, kind) in enumerate(items):
            tree.add_item(index, nodeid, node, kind)
        for nodeid, longrepr in failures:
            tree.add_failure(nodeid, longrepr)
        for nodeid, reason, location, is_dir in skips:
            tree.add_skip(nodeid, cls.skip_longrepr(nodeid, reason, location), is_dir)
        for nodeid, longrepr in doctest_skips:
            kind = "DoctestModule" if nodeid.endswith(".py") else "DoctestTextfile"
            tree.add_skip(nodeid, longrepr, False, kind)
        for nodeid, reason in file_skips:
            dir_nodeid = nodeid.rpartition("/")[0] or "."
            tree.add_file_skip(nodeid, (dir_nodeid, 1, f"Skipped: {reason}"))
        tree.sort_dirs(tree.root)
        matched, intermediates = cls.match_args(tree, args, [nodeid for nodeid, _, _ in items])
        dispatcher = cls(config, tree, nodes)
        try:
            dispatcher.run(matched, intermediates)
        except BaseException as exc:
            exc._rs_numcollected = dispatcher.numcollected  # type: ignore[attr-defined]
            raise
        return dispatcher.added_marks()

    @classmethod
    def deliver(cls, nodeid: str, outcome: str, longrepr: str, location: str | None) -> None:
        """A -n controller's pytest_collectreport for a worker's failed or
        skipped collection (xdist's DSession.worker_collectreport)."""
        repr_: Any = longrepr
        if location is not None:
            repr_ = cls.skip_longrepr(nodeid, longrepr, location)
        report = cls.report(nodeid, outcome, repr_, [])
        pluginmanager.hook.pytest_collectreport.call_for_node(
            cls.excluded_plugins(), [], report=report
        )
