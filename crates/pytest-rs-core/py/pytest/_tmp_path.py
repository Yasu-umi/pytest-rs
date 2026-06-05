"""tmp_path / tmp_path_factory builtin fixtures."""

import pathlib
import tempfile

from pytest._fixtures import fixture


class TempPathFactory:
    """Factory for session-scoped temporary directories."""

    def __init__(self, basetemp=None):
        self._basetemp = pathlib.Path(basetemp) if basetemp else None

    def getbasetemp(self):
        if self._basetemp is None:
            self._basetemp = pathlib.Path(tempfile.mkdtemp(prefix="pytest-rs-basetemp-"))
        return self._basetemp

    def mktemp(self, basename, numbered=True):
        base = self.getbasetemp()
        basename = str(basename)
        if not numbered:
            path = base / basename
            path.mkdir()
            return path
        maximum = -1
        for existing in base.iterdir():
            name = existing.name
            if name.startswith(basename) and name[len(basename) :].isdigit():
                maximum = max(maximum, int(name[len(basename) :]))
        path = base / f"{basename}{maximum + 1}"
        path.mkdir()
        return path


@fixture(scope="session")
def tmp_path_factory():
    import shutil

    factory = TempPathFactory()
    yield factory
    if factory._basetemp is not None:
        shutil.rmtree(factory._basetemp, ignore_errors=True)


@fixture
def tmp_path(tmp_path_factory, request):
    import re

    name = re.sub(r"\W", "_", request.node.name)[:30]
    return tmp_path_factory.mktemp(name, numbered=True)


class LocalPath:
    """Minimal py.path.local equivalent backing the legacy tmpdir fixture."""

    def __init__(self, path):
        self._path = pathlib.Path(path)

    def __str__(self):
        return str(self._path)

    def __repr__(self):
        return f"local({str(self._path)!r})"

    def __fspath__(self):
        return str(self._path)

    def __truediv__(self, other):
        return LocalPath(self._path / str(other))

    def __eq__(self, other):
        return str(self) == str(other)

    def __hash__(self):
        return hash(str(self._path))

    @property
    def strpath(self):
        return str(self._path)

    @property
    def basename(self):
        return self._path.name

    @property
    def dirname(self):
        return str(self._path.parent)

    def join(self, *parts):
        return LocalPath(self._path.joinpath(*[str(part) for part in parts]))

    def dirpath(self, *parts):
        return LocalPath(self._path.parent.joinpath(*[str(part) for part in parts]))

    def ensure(self, *parts, dir=False):
        target = self._path.joinpath(*[str(part) for part in parts])
        if dir:
            target.mkdir(parents=True, exist_ok=True)
        else:
            target.parent.mkdir(parents=True, exist_ok=True)
            target.touch()
        return LocalPath(target)

    def mkdir(self, *parts):
        target = self._path.joinpath(*[str(part) for part in parts])
        target.mkdir(parents=True)
        return LocalPath(target)

    def exists(self):
        return self._path.exists()

    def isfile(self):
        return self._path.is_file()

    def isdir(self):
        return self._path.is_dir()

    def remove(self, ignore_errors=False):
        import shutil

        if self._path.is_dir():
            shutil.rmtree(self._path, ignore_errors=ignore_errors)
        else:
            self._path.unlink()

    def open(self, mode="r", encoding=None):
        return open(self._path, mode, encoding=encoding)

    def write(self, data, mode="w"):
        with open(self._path, mode) as f:
            f.write(data)
        return self

    def write_text(self, data, encoding="utf-8"):
        self._path.write_text(data, encoding=encoding)

    def read(self, mode="r"):
        with open(self._path, mode) as f:
            return f.read()

    def read_text(self, encoding="utf-8"):
        return self._path.read_text(encoding=encoding)

    def listdir(self):
        return [LocalPath(child) for child in sorted(self._path.iterdir())]

    def chdir(self):
        import os

        old = os.getcwd()
        os.chdir(self._path)
        return LocalPath(old)


@fixture
def tmpdir(tmp_path):
    return LocalPath(tmp_path)
