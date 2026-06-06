"""tmp_path / tmp_path_factory builtin fixtures."""

import pathlib
import tempfile

from pytest._fixtures import fixture

_BASETEMP_PREFIX = "pytest-rs-basetemp-"
# A SIGKILLed session (timeouts, ^C -9) never reaches the factory teardown;
# sweep leftovers this much older than now before creating a new basetemp.
# Generous so concurrently live sessions are never touched.
_STALE_SECONDS = 3 * 60 * 60

# --basetemp value, set by the engine at startup (None → mkdtemp per run).
_given_basetemp = None


def configure(basetemp):
    global _given_basetemp
    _given_basetemp = basetemp


def rm_rf(path):
    """shutil.rmtree clearing read-only bits on the way (pytest's rm_rf):
    plain rmtree silently leaves e.g. chmod-0 dirs made by tests behind."""
    import os
    import shutil
    import stat

    def onexc(func, failed, exc):
        for target in (os.path.dirname(failed), failed):
            try:
                os.chmod(target, os.stat(target).st_mode | stat.S_IRWXU)
            except OSError:
                pass
        try:
            func(failed)
        except OSError:
            pass

    try:
        shutil.rmtree(path, onexc=onexc)
    except OSError:
        pass


def _sweep_stale_basetemps():
    """Best-effort removal of basetemps left behind by killed sessions."""
    import time

    cutoff = time.time() - _STALE_SECONDS
    try:
        entries = list(pathlib.Path(tempfile.gettempdir()).iterdir())
    except OSError:
        return
    for entry in entries:
        if not entry.name.startswith(_BASETEMP_PREFIX):
            continue
        try:
            if entry.stat().st_mtime < cutoff:
                rm_rf(entry)
        except OSError:
            continue


class TempPathFactory:
    """Factory for session-scoped temporary directories."""

    def __init__(self, basetemp=None):
        basetemp = basetemp if basetemp is not None else _given_basetemp
        self._given = basetemp is not None
        self._basetemp = None
        if self._given:
            # pytest semantics for an explicit --basetemp: cleared at session
            # start, kept after the run.
            path = pathlib.Path(basetemp)
            if path.exists():
                rm_rf(path)
            path.mkdir(parents=True, exist_ok=True)
            self._basetemp = path.resolve()

    def getbasetemp(self):
        if self._basetemp is None:
            _sweep_stale_basetemps()
            # resolve() so chdir(tmp_path) round-trips through os.getcwd()
            # (macOS /tmp is a symlink to /private/tmp).
            self._basetemp = pathlib.Path(tempfile.mkdtemp(prefix=_BASETEMP_PREFIX)).resolve()
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
    factory = TempPathFactory()
    yield factory
    # Auto-created basetemps are removed with the session; an explicit
    # --basetemp survives the run (pytest semantics).
    if factory._basetemp is not None and not factory._given:
        rm_rf(factory._basetemp)


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
