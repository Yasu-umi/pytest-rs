"""The plugin's own fixtures, running under pytest-rs.

`aiohttp_client` and `aiohttp_server` come from pytest-aiohttp; the async test
bodies are driven by pytest-rs's native asyncio support (`asyncio_mode = auto`),
which is what pytest-asyncio would otherwise provide.
"""

from aiohttp import web


async def hello(request: web.Request) -> web.Response:
    return web.Response(text=f"hello {request.query.get('name', 'world')}")


def make_app() -> web.Application:
    app = web.Application()
    app.router.add_get("/", hello)
    return app


async def test_client_fixture_serves_the_app(aiohttp_client) -> None:
    client = await aiohttp_client(make_app())

    response = await client.get("/")

    assert response.status == 200
    assert await response.text() == "hello world"


async def test_server_fixture_reports_its_port(aiohttp_server) -> None:
    server = await aiohttp_server(make_app())

    assert server.port > 0
