"""MonkeyPatch and its fixture."""

from pytest._fixtures import fixture


class MonkeyPatch:
    _notset = object()

    def __init__(self):
        self._setattr = []
        self._setitem = []
        self._cwd = None
        self._savesyspath = None

    @classmethod
    def context(cls):
        import contextlib

        @contextlib.contextmanager
        def _context():
            m = cls()
            try:
                yield m
            finally:
                m.undo()

        return _context()

    def setattr(self, target, name, value=_notset, raising=True):
        if isinstance(target, str):
            # setattr("module.path.attr", value) form
            import importlib

            if value is self._notset:
                value = name
            module_path, _, name = target.rpartition(".")
            target = importlib.import_module(module_path)
        elif value is self._notset:
            raise TypeError("setattr requires a value when target is an object")
        old = getattr(target, name, self._notset)
        if raising and old is self._notset:
            raise AttributeError(f"{target!r} has no attribute {name!r}")
        self._setattr.append((target, name, old))
        setattr(target, name, value)

    def delattr(self, target, name=_notset, raising=True):
        if isinstance(target, str):
            import importlib

            module_path, _, attr_name = target.rpartition(".")
            target = importlib.import_module(module_path)
            name = attr_name
        old = getattr(target, name, self._notset)
        if old is self._notset:
            if raising:
                raise AttributeError(name)
            return
        self._setattr.append((target, name, old))
        delattr(target, name)

    def setitem(self, mapping, name, value):
        self._setitem.append((mapping, name, mapping.get(name, self._notset)))
        mapping[name] = value

    def delitem(self, mapping, name, raising=True):
        if name not in mapping:
            if raising:
                raise KeyError(name)
            return
        self._setitem.append((mapping, name, mapping[name]))
        del mapping[name]

    def setenv(self, name, value, prepend=None):
        import os

        value = str(value)
        if prepend and name in os.environ:
            value = value + prepend + os.environ[name]
        self.setitem(os.environ, name, value)

    def delenv(self, name, raising=True):
        import os

        self.delitem(os.environ, name, raising=raising)

    def syspath_prepend(self, path):
        import sys

        if self._savesyspath is None:
            self._savesyspath = sys.path[:]
        sys.path.insert(0, str(path))

    def chdir(self, path):
        import os

        if self._cwd is None:
            self._cwd = os.getcwd()
        os.chdir(path)

    def undo(self):
        import os
        import sys

        for target, name, old in reversed(self._setattr):
            if old is self._notset:
                try:
                    delattr(target, name)
                except AttributeError:
                    pass
            else:
                setattr(target, name, old)
        self._setattr.clear()
        for mapping, name, old in reversed(self._setitem):
            if old is self._notset:
                mapping.pop(name, None)
            else:
                mapping[name] = old
        self._setitem.clear()
        if self._savesyspath is not None:
            sys.path[:] = self._savesyspath
            self._savesyspath = None
        if self._cwd is not None:
            os.chdir(self._cwd)
            self._cwd = None


@fixture
def monkeypatch():
    m = MonkeyPatch()
    yield m
    m.undo()
