"""Async tests: pytest-asyncio is bundled — no extra install needed."""

import asyncio


async def test_async_add():
    await asyncio.sleep(0)
    assert 1 + 1 == 2


async def test_gather():
    async def double(n):
        await asyncio.sleep(0)
        return n * 2

    results = await asyncio.gather(double(1), double(2), double(3))
    assert results == [2, 4, 6]
