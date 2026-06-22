"""Config.trace (TagTracer) and Config._getconftest_pathlist helpers."""

import pathlib


class _TagTracerRoot:
    def __init__(self):
        self._writer = None

    def setwriter(self, writer):
        self._writer = writer

    def setprocessor(self, tags, processor):
        pass


class ConfigTrace:
    """A TagTracer-compatible callable: ``config.trace("msg")`` writes
    ``"msg [config]\\n"`` through the writer set on ``.root``."""

    def __init__(self):
        self.root = _TagTracerRoot()

    def __call__(self, *args):
        if self.root._writer is not None:
            msg = " ".join(str(a) for a in args)
            self.root._writer(f"{msg} [config]\n")


def getconftest_pathlist(name, path, rootdir):
    if path is None:
        return None
    if isinstance(path, str):
        path = pathlib.Path(path)
    conftest = path / "conftest.py" if path.is_dir() else path
    if not conftest.exists():
        return None
    ns = {}
    try:
        exec(compile(conftest.read_text(encoding="utf-8"), str(conftest), "exec"), ns)
    except Exception:
        return None
    if name not in ns:
        return None
    values = ns[name]
    if not isinstance(values, (list, tuple)):
        return None
    basedir = conftest.parent
    return [basedir / pathlib.Path(str(v)) for v in values]
