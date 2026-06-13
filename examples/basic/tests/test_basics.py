"""Core pytest patterns: assertions, fixtures, parametrize, markers."""

import pytest

from my_project import add, divide


def test_add():
    assert add(2, 3) == 5


def test_divide():
    assert divide(10, 2) == 5.0


def test_divide_by_zero():
    with pytest.raises(ZeroDivisionError, match="cannot divide by zero"):
        divide(1, 0)


@pytest.mark.parametrize(
    "a, b, expected",
    [
        (0, 0, 0),
        (1, 1, 2),
        (-1, 1, 0),
        (100, 200, 300),
    ],
)
def test_add_parametrize(a, b, expected):
    assert add(a, b) == expected


class TestFixtures:
    def test_sorted(self, sample_list):
        assert sorted(sample_list) == [1, 1, 2, 3, 4, 5, 6, 9]

    def test_config_file(self, config_file):
        assert config_file.exists()
        assert 'name = "demo"' in config_file.read_text()


@pytest.mark.skip(reason="not implemented yet")
def test_future_feature():
    pass


@pytest.mark.xfail(reason="known issue")
def test_known_issue():
    assert 1 == 2
