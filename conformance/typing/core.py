"""reveal_type corpus: pytest core API overload precision.

Each `reveal_type(...)` call must be followed by a `# revealed: <type>`
comment; conformance/typing_check.py asserts mypy's actual output matches.
This is a static-analysis regression suite (does the public API resolve to
precise types), not a runtime pass/fail suite -- see typing_check.py's
docstring.
"""

import pytest


class MyError(ValueError):
    pass


def check_raises_type_form() -> None:
    with pytest.raises(MyError) as exc_info:
        raise MyError("x")
    reveal_type(exc_info)  # revealed: pytest._raises.ExceptionInfo[core.MyError]


def check_raises_match_only_form() -> None:
    with pytest.raises(match="x") as exc_info:
        raise ValueError("x")
    reveal_type(exc_info)  # revealed: pytest._raises.ExceptionInfo[BaseException]


def check_raises_callable_form() -> None:
    def raiser(x: int) -> None:
        raise MyError(str(x))

    exc_info = pytest.raises(MyError, raiser, 1)
    reveal_type(exc_info)  # revealed: pytest._raises.ExceptionInfo[core.MyError]


def check_excinfo_attributes() -> None:
    """The captured exception is the raised type, not `Any | None`.

    A test suite reads `excinfo.value.<attr>` constantly; an optional or Any
    `value` turns each of those into a union-attr error downstream.
    """
    with pytest.raises(MyError) as exc_info:
        raise MyError("x")
    reveal_type(exc_info.value)  # revealed: core.MyError
    reveal_type(exc_info.type)  # revealed: type[core.MyError]
    reveal_type(exc_info.tb)  # revealed: types.TracebackType


def check_outcomes_are_noreturn(flag: bool) -> int:
    """A branch may end in skip/fail/xfail/exit instead of a return."""
    if flag:
        return 1
    # `.Exception` has to be visible statically (upstream declares it as a
    # ClassVar on the callable class), and reaching the end of an `int`-returning
    # function without a return only type-checks because fail is NoReturn.
    reveal_type(pytest.fail.Exception)  # revealed: type[pytest._outcomes.Failed]
    reveal_type(pytest.skip.Exception)  # revealed: type[pytest._outcomes.Skipped]
    pytest.fail("no value to return")


@pytest.fixture
def bare_fixture() -> int:
    return 1


reveal_type(bare_fixture)  # revealed: pytest._fixtures.FixtureFunctionDefinition


@pytest.fixture(scope="session")
def parametrized_fixture() -> str:
    return "x"


reveal_type(parametrized_fixture)  # revealed: pytest._fixtures.FixtureFunctionDefinition


@pytest.mark.parametrize("x", [1, 2, 3])
def check_parametrize(x: int) -> None:
    pass


reveal_type(pytest.mark.parametrize)  # revealed: pytest._marks._ParametrizeMarkDecorator
reveal_type(pytest.param(1, 2, id="case1"))  # revealed: pytest._marks.ParamSpec


def check_warns_cm_form() -> None:
    with pytest.warns(UserWarning) as w:
        pass
    reveal_type(w)  # revealed: pytest._warns.WarningsChecker


def check_warns_callable_form() -> None:
    def raiser() -> int:
        return 1

    result = pytest.warns(UserWarning, raiser)
    reveal_type(result)  # revealed: int


def check_deprecated_call_cm_form() -> None:
    with pytest.deprecated_call() as w:
        pass
    reveal_type(w)  # revealed: pytest._warns.WarningsRecorder


def check_monkeypatch_setattr(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr("os.path.join", lambda *a: "x")
