"""Assertion-introspection surface (the comparison explainer is a faithful
port of pytest's _pytest/assertion/util.py)."""

from _pytest.assertion import truncate as truncate
from _pytest.assertion import util as util


def pytest_assertrepr_compare(config, op, left, right):
    return util.assertrepr_compare(config=config, op=op, left=left, right=right)


from _pytest._stub import __getattr__  # noqa: E402, F401
