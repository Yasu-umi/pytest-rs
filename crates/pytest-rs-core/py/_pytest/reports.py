class BaseReport:
    pass


class TestReport(BaseReport):
    pass


class CollectReport(BaseReport):
    pass


from _pytest._stub import __getattr__  # noqa: E402, F401
