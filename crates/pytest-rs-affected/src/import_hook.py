"""Bracket the execution of project modules for --affected (stdlib only).

A finder placed first on sys.meta_path asks the other finders for the
spec, and when the module is a project file, swaps the spec's loader for an
instance of a subclass of the loader's own class whose ``exec_module``
reports the start and the end of the module's execution. The instance
shares the original loader's ``__dict__``, so ``isinstance`` checks and
loader state are unchanged. A loader class that cannot be subclassed is
left alone: that module's code then counts toward whatever context imports
it, as without this hook.
"""

import os
import sys

_SUBCLASSES: dict = {}


def _bracketing_class(cls, collector):
    sub = _SUBCLASSES.get(cls)
    if sub is None:
        base_exec = cls.exec_module

        def exec_module(self, module):
            origin = getattr(module.__spec__, "origin", None) or getattr(module, "__file__", "")
            depth = collector.import_start(origin)
            try:
                return base_exec(self, module)
            finally:
                collector.import_finish(depth)

        try:
            sub = type(
                cls.__name__,
                (cls,),
                {
                    "exec_module": exec_module,
                    "__module__": cls.__module__,
                    "__qualname__": cls.__qualname__,
                },
            )
        except TypeError:
            sub = False
        _SUBCLASSES[cls] = sub
    return sub or None


class AffectedImportFinder:
    def __init__(self, collector, rootdir):
        self._collector = collector
        self._rootdir = rootdir
        self._project_tops: dict = {}

    def _may_be_project_module(self, name):
        """Whether `name`'s top-level package lives under the rootdir on sys.path.

        Everything else is left to the other finders directly: asking them
        here first and returning None would make Python ask them again, and
        a missing module (``pytest.importorskip``) would be searched twice.
        """
        top = name.partition(".")[0]
        key = (top, len(sys.path))
        known = self._project_tops.get(key)
        if known is None:
            known = False
            for entry in sys.path:
                if not isinstance(entry, str) or not entry.startswith(self._rootdir):
                    continue
                base = os.path.join(entry, top)
                if os.path.isdir(base) or os.path.isfile(base + ".py"):
                    known = True
                    break
            self._project_tops[key] = known
        return known

    def find_spec(self, name, path=None, target=None):
        if not self._may_be_project_module(name):
            return None
        for finder in sys.meta_path:
            if finder is self:
                continue
            find = getattr(finder, "find_spec", None)
            if find is None:
                continue
            spec = find(name, path, target)
            if spec is None:
                continue
            loader = spec.loader
            if (
                loader is not None
                and spec.origin
                and hasattr(loader, "exec_module")
                and hasattr(loader, "__dict__")
                and self._collector.tracks_module(spec.origin)
            ):
                sub = _bracketing_class(type(loader), self._collector)
                if sub is not None:
                    try:
                        wrapped = object.__new__(sub)
                        wrapped.__dict__ = loader.__dict__
                    except (TypeError, AttributeError):
                        pass
                    else:
                        spec.loader = wrapped
            return spec
        return None

    def invalidate_caches(self):
        pass


def install(collector, rootdir):
    finder = AffectedImportFinder(collector, rootdir)
    sys.meta_path.insert(0, finder)
    return finder


def uninstall(finder):
    try:
        sys.meta_path.remove(finder)
    except ValueError:
        pass
