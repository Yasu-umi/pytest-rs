class BaseReport:
    # pytest's BaseReport surface used by report consumers (junitxml):
    # captured-output properties derived from the (header, content)
    # sections list.
    sections = []

    def _join_sections(self, prefix):
        return "\n".join(
            content for (header, content) in self.sections if header.startswith(prefix)
        )

    @property
    def capstdout(self):
        return self._join_sections("Captured stdout")

    @property
    def capstderr(self):
        return self._join_sections("Captured stderr")

    @property
    def caplog(self):
        return self._join_sections("Captured log")

    @property
    def passed(self):
        return getattr(self, "outcome", None) == "passed"

    @property
    def failed(self):
        return getattr(self, "outcome", None) == "failed"

    @property
    def skipped(self):
        return getattr(self, "outcome", None) == "skipped"

    @property
    def count_towards_summary(self):
        return True


class TestReport(BaseReport):
    def __init__(self, **kwargs):
        for name, value in kwargs.items():
            setattr(self, name, value)


class CollectReport(BaseReport):
    def __init__(self, **kwargs):
        for name, value in kwargs.items():
            setattr(self, name, value)


from _pytest._stub import __getattr__  # noqa: E402, F401
