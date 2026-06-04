import pathlib
import tempfile


class TempPathFactory:
    def __init__(self, basetemp=None):
        self._basetemp = pathlib.Path(basetemp) if basetemp else None

    def getbasetemp(self):
        if self._basetemp is None:
            self._basetemp = pathlib.Path(tempfile.mkdtemp(prefix="pytest-rs-basetemp-"))
        return self._basetemp

    def mktemp(self, basename, numbered=True):
        base = self.getbasetemp()
        if not numbered:
            path = base / basename
            path.mkdir()
            return path
        from _pytest.pathlib import make_numbered_dir

        return make_numbered_dir(base, basename)
