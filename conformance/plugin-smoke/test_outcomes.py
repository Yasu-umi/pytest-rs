"""Mixed-outcome demo: every report category the terminal reporter renders
(progress letters, instant failures, summaries, stats line)."""

import warnings

import pytest


def test_pass():
    assert 1 + 1 == 2


def test_pass2():
    assert True


def test_fail():
    x = {"a": 1}
    assert x["a"] == 2


def test_skip():
    pytest.skip("not today")


@pytest.mark.xfail(reason="known bug")
def test_xfail():
    assert False


@pytest.mark.xfail(reason="fixed?")
def test_xpass():
    assert True


@pytest.fixture
def broken():
    raise RuntimeError("setup boom")


def test_error(broken):
    pass


def test_warns():
    warnings.warn("deprecated thing", DeprecationWarning)
    assert True
