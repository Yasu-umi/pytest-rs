import sys

from pytest import LineMatcher, Pytester, RunResult  # noqa: F401


class SysModulesSnapshot:
    """Snapshot of ``sys.modules`` that ``restore()`` reinstates in place
    (the live mapping object is preserved, only its contents are reset)."""

    def __init__(self, preserve=None):
        self.__saved = dict(sys.modules)
        self.__preserve = preserve

    def restore(self):
        if self.__preserve:
            self.__saved.update(
                (k, m) for k, m in sys.modules.items() if self.__preserve(k)
            )
        sys.modules.clear()
        sys.modules.update(self.__saved)


class SysPathsSnapshot:
    """Snapshot of ``sys.path``/``sys.meta_path`` restored by in-place slice
    assignment, so the live list objects are preserved."""

    def __init__(self):
        self.__saved = list(sys.path), list(sys.meta_path)

    def restore(self):
        sys.path[:], sys.meta_path[:] = self.__saved


class HookRecorder:
    """Stub: hook recording is not supported yet."""

    def __init__(self, *args, **kwargs):
        raise NotImplementedError("HookRecorder is not supported by pytest-rs yet")


from _pytest._stub import __getattr__  # noqa: E402, F401
