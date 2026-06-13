def add(a: int, b: int) -> int:
    return a + b


def divide(a: float, b: float) -> float:
    if b == 0:
        raise ZeroDivisionError("cannot divide by zero")
    return a / b


def greet(name: str) -> str:
    return f"Hello, {name}!"


async def fetch_json(url: str) -> dict:
    """Simulate an async HTTP fetch (no real network)."""
    import asyncio

    await asyncio.sleep(0.01)
    return {"url": url, "status": 200}
