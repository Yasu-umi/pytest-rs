"""reveal_type corpus: pytest-asyncio / pytest-mock overload precision.

See core.py's module docstring for the `# revealed:` comment convention.
Requires crates/pytest-rs-asyncio/py and crates/pytest-rs-mock/py on
MYPYPATH alongside pytest-rs-core (typing_check.py sets this up).
"""

import pytest_asyncio
from pytest_mock.plugin import MockerFixture


@pytest_asyncio.fixture
async def bare_async_fixture() -> int:
    return 1


reveal_type(bare_async_fixture)  # revealed: def () -> typing.Coroutine[Any, Any, int]


@pytest_asyncio.fixture(loop_scope="session")
async def parametrized_async_fixture() -> str:
    return "x"


reveal_type(parametrized_async_fixture)  # revealed: def () -> typing.Coroutine[Any, Any, str]


def check_mocker_patch_bare_form(mocker: MockerFixture) -> None:
    m = mocker.patch("os.getcwd")
    reveal_type(
        m
    )  # revealed: unittest.mock.MagicMock | unittest.mock.AsyncMock | unittest.mock.NonCallableMagicMock


def check_mocker_patch_new_value_form(mocker: MockerFixture) -> None:
    m = mocker.patch("os.getcwd", new=lambda: "x")
    reveal_type(m)  # revealed: def () -> str


def check_mocker_patch_new_callable_form(mocker: MockerFixture) -> None:
    def make_replacement() -> int:
        return 1

    m = mocker.patch("os.getcwd", new_callable=make_replacement)
    reveal_type(m)  # revealed: int
