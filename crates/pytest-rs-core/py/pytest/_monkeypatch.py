"""MonkeyPatch and its fixture."""

import contextlib
import inspect
import os
import sys
import warnings

from pytest._fixtures import fixture
from pytest._warning_types import PytestWarning


class MonkeyPatch:
    _notset = object()

    def __init__(self):
        self._setattr = []
        self._setitem = []
        self._cwd = None
        self._savesyspath = None

    @classmethod
    def context(cls):
        @contextlib.contextmanager
        def _context():
            m = cls()
            try:
                yield m
            finally:
                m.undo()

        return _context()

    @staticmethod
    def _resolve(name):
        """Import-or-getattr each dotted segment, pytest's monkeypatch
        resolve(): attribute lookup wins, failed imports of submodules get
        an "import error in {path}" message."""
        parts = name.split(".")
        used = parts.pop(0)
        found = __import__(used)
        for part in parts:
            used += "." + part
            try:
                found = getattr(found, part)
            except AttributeError:
                pass
            else:
                continue
            try:
                __import__(used)
            except ImportError as ex:
                expected = str(ex).split()[-1]
                if expected == used:
                    raise
                raise ImportError(f"import error in {used}: {ex}") from ex
            found = getattr(found, part)
        return found

    def setattr(self, target, name, value=_notset, raising=True):
        if isinstance(target, str):
            # setattr("module.path.attr", value) form
            if value is self._notset:
                value = name
            module_path, _, name = target.rpartition(".")
            target = self._resolve(module_path)
        elif value is self._notset:
            raise TypeError("setattr requires a value when target is an object")
        if raising and not hasattr(target, name):
            raise AttributeError(f"{target!r} has no attribute {name!r}")
        self._setattr.append((target, name, self._old_value(target, name)))
        setattr(target, name, value)

    def delattr(self, target, name=_notset, raising=True):
        if isinstance(target, str):
            module_path, _, attr_name = target.rpartition(".")
            target = self._resolve(module_path)
            name = attr_name
        if not hasattr(target, name):
            if raising:
                raise AttributeError(name)
            return
        self._setattr.append((target, name, self._old_value(target, name)))
        delattr(target, name)

    def _old_value(self, target, name):
        """The restore value: for classes, read the raw __dict__ entry so
        descriptors (staticmethod/classmethod) are not unwrapped."""
        if inspect.isclass(target):
            return target.__dict__.get(name, self._notset)
        return getattr(target, name, self._notset)

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

    @staticmethod
    def _warn_if_env_name_is_not_str(name):
        if not isinstance(name, str):
            warnings.warn(
                PytestWarning(f"Environment variable name {name!r} should be a str"),
                stacklevel=3,
            )

    def setenv(self, name, value, prepend=None):
        if not isinstance(value, str):
            warnings.warn(
                PytestWarning(
                    f"Value of environment variable {name} type should be str, but got "
                    f"{value!r} (type: {type(value).__name__}); converted to str implicitly"
                ),
                stacklevel=2,
            )
            value = str(value)
        if prepend and name in os.environ:
            value = value + prepend + os.environ[name]
        self._warn_if_env_name_is_not_str(name)
        self.setitem(os.environ, name, value)

    def delenv(self, name, raising=True):
        self._warn_if_env_name_is_not_str(name)
        self.delitem(os.environ, name, raising=raising)

    def syspath_prepend(self, path):
        if self._savesyspath is None:
            self._savesyspath = sys.path[:]
        sys.path.insert(0, str(path))

    def chdir(self, path):
        if self._cwd is None:
            self._cwd = os.getcwd()
        os.chdir(path)

    def undo(self):
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
