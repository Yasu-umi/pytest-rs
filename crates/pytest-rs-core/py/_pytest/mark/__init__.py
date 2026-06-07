from pytest._expression import KeywordMatcher, MarkMatcher  # noqa: F401
from pytest._marks import Mark, MarkDecorator, MarkGenerator  # noqa: F401

EMPTY_PARAMETERSET_OPTION = "empty_parameter_set_mark"


def get_empty_parameterset_mark(config, argnames, func):
    """Upstream structures.get_empty_parameterset_mark: the mark applied to
    the synthetic NOTSET set per the empty_parameter_set_mark ini."""
    from pytest._marks import mark
    from pytest._node import Collector

    argslisting = ", ".join(argnames)
    reason = f"got empty parameter set for ({argslisting})"
    requested_mark = None
    if config is not None:
        try:
            requested_mark = config.getini(EMPTY_PARAMETERSET_OPTION)
        except Exception:
            requested_mark = None
    if requested_mark in ("", None, "skip"):
        return mark.skip(reason=reason)
    if requested_mark == "xfail":
        return mark.xfail(reason=reason, run=False)
    if requested_mark == "fail_at_collect":
        lineno = getattr(getattr(func, "__code__", None), "co_firstlineno", 0)
        fname = getattr(func, "__name__", "?")
        raise Collector.CollectError(f"Empty parameter set in '{fname}' at line {lineno + 1}")
    raise LookupError(requested_mark)


def pytest_configure(config):
    pass


from _pytest._stub import __getattr__  # noqa: E402, F401
