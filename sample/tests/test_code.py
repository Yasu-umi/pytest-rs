import pytest
from src.code import add, async_add


@pytest.fixture
def a():
    return 1


@pytest.fixture
async def b():
    return 2


@pytest.fixture
def c():
    yield 3


def test_add_success(a, c):
    assert add(add(a, 2), c) == 6


def test_add_failed(c):
    assert add(2, c) == 4


@pytest.mark.asyncio
async def test_async_add_success(b):
    assert await async_add(1, b) == 3


@pytest.mark.asyncio
async def test_async_add_failed():
    assert await async_add(1, 2) == 4


@pytest.mark.skip(reason="demonstrates skip")
def test_skipped():
    raise AssertionError("never runs")
