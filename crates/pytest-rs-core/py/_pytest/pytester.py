from pytest import LineMatcher, Pytester, RunResult  # noqa: F401


class HookRecorder:
    """Stub: hook recording is not supported yet."""

    def __init__(self, *args, **kwargs):
        raise NotImplementedError("HookRecorder is not supported by pytest-rs yet")
