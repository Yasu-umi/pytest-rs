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
