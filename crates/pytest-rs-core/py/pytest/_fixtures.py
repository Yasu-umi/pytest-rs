"""The @pytest.fixture decorator: records metadata, resolved by the engine."""

import functools
import inspect
import warnings
from typing import TYPE_CHECKING, Any, overload

if TYPE_CHECKING:
    from collections.abc import Callable, Iterable, Sequence

    from _pytest.config import Config
    from _pytest.scope import _ScopeName


class FixtureLookupError(LookupError):
    """Raised by request.getfixturevalue() for unknown fixture names."""


class FixtureFunctionMarker:
    def __init__(self, scope="function", params=None, autouse=False, ids=None, name=None):
        self.scope = scope
        self.params = list(params) if params is not None else None
        self.autouse = autouse
        self.ids = ids
        self.name = name

    def __call__(self, function: "Callable[..., object]") -> "FixtureFunctionDefinition":
        if inspect.isclass(function):
            raise ValueError("class fixtures not supported (maybe in the future)")
        if isinstance(function, FixtureFunctionDefinition):
            raise ValueError(
                "@pytest.fixture is being applied more than once to the same "
                f"function {function.__name__!r}"
            )
        if getattr(function, "_pytestfixturefunction", False):
            raise ValueError(
                "@pytest.fixture is being applied more than once to the same "
                f"function {getattr(function, '__name__', function)!r}"
            )
        if hasattr(function, "pytestmark"):
            # An error since pytest 9 (#3364).
            from _pytest.outcomes import fail

            fail(
                "Marks cannot be applied to fixtures.\n"
                "See docs: https://docs.pytest.org/en/stable/deprecations.html"
                "#applying-a-mark-to-a-fixture-function"
            )
        return FixtureFunctionDefinition(
            function=function,
            fixture_function_marker=self,
        )


class FixtureFunctionDefinition:
    def __init__(self, *, function, fixture_function_marker, instance=None):
        self.name = fixture_function_marker.name or function.__name__
        self.__name__ = self.name
        self._fixture_function_marker = fixture_function_marker
        if instance is not None:
            self._fixture_function = function.__get__(instance)
        else:
            self._fixture_function = function
        self._pytestfixturefunction = fixture_function_marker
        functools.update_wrapper(self, function)

    def __repr__(self):
        return f"<pytest_fixture({self._fixture_function})>"

    def __get__(self, instance, owner=None):
        if instance is None:
            return self
        return FixtureFunctionDefinition(
            function=self._fixture_function,
            fixture_function_marker=self._fixture_function_marker,
            instance=instance,
        )

    def __call__(self, *args, **kwargs):
        from _pytest.outcomes import fail

        message = (
            f'Fixture "{self.name}" called directly. Fixtures are not meant to be called directly,\n'
            "but are created automatically when test functions request them as parameters.\n"
            "See https://docs.pytest.org/en/stable/explanation/fixtures.html for more information about fixtures, and\n"
            "https://docs.pytest.org/en/stable/deprecations.html#calling-fixtures-directly"
        )
        fail(message, pytrace=False)

    def _get_wrapped_function(self):
        return self._fixture_function


@overload
def fixture(
    fixture_function: "Callable[..., object]",
    *,
    scope: "_ScopeName | Callable[[str, Config], _ScopeName]" = ...,
    params: "Iterable[object] | None" = ...,
    autouse: bool = ...,
    ids: "Sequence[object | None] | Callable[[Any], object | None] | None" = ...,
    name: str | None = ...,
) -> FixtureFunctionDefinition: ...


@overload
def fixture(
    fixture_function: None = ...,
    *,
    scope: "_ScopeName | Callable[[str, Config], _ScopeName]" = ...,
    params: "Iterable[object] | None" = ...,
    autouse: bool = ...,
    ids: "Sequence[object | None] | Callable[[Any], object | None] | None" = ...,
    name: str | None = ...,
) -> FixtureFunctionMarker: ...


def fixture(
    fixture_function=None, *, scope="function", params=None, autouse=False, ids=None, name=None
):
    marker = FixtureFunctionMarker(scope=scope, params=params, autouse=autouse, ids=ids, name=name)
    if fixture_function is not None:
        return marker(fixture_function)
    return marker


def eval_scope_callable(scope_callable, fixture_name, config):
    """Evaluate a dynamic `@pytest.fixture(scope=<callable>)` to a scope name,
    mirroring _pytest.fixtures._eval_scope_callable. The engine validates the
    returned string. A non-str result fails like upstream."""
    from _pytest.outcomes import fail

    try:
        result = scope_callable(fixture_name=fixture_name, config=config)
    except Exception as e:
        raise TypeError(
            f"Error evaluating {scope_callable} while defining fixture '{fixture_name}'.\n"
            "Expected a function with the signature (*, fixture_name, config)"
        ) from e
    if not isinstance(result, str):
        fail(
            f"Expected {scope_callable} to return a 'str' while defining fixture "
            f"'{fixture_name}', but it returned:\n{result!r}",
            pytrace=False,
        )
    return result


def yield_fixture(
    fixture_function=None, *, scope="function", params=None, autouse=False, ids=None, name=None
):
    """Deprecated alias for :func:`fixture`."""
    from _pytest.deprecated import YIELD_FIXTURE

    warnings.warn(YIELD_FIXTURE, stacklevel=2)
    return fixture(
        fixture_function, scope=scope, params=params, autouse=autouse, ids=ids, name=name
    )


def _find_wrapped_fixture_def(obj: object) -> FixtureFunctionDefinition | None:
    """A FixtureFunctionDefinition in ``obj``'s ``__wrapped__`` chain (upstream's
    FixtureManager._find_wrapped_fixture_def)."""
    try:
        if getattr(obj, "_mock_name", None) is not None:
            return None
    except Exception:
        return None
    try:
        unwrapped = inspect.unwrap(
            obj,  # type: ignore[arg-type]
            stop=lambda o: type(o) is FixtureFunctionDefinition,
        )
    except Exception:
        return None
    if type(unwrapped) is FixtureFunctionDefinition:
        return unwrapped
    return None


def check_for_wrapped_fixture(name: str, obj: object) -> bool:
    """Warn if the raw class/module ``__dict__`` entry ``obj`` is a fixture
    hidden behind wrappers (pytest 9.2, #13507). Returns True when discovery
    must skip it: a classmethod or custom decorator above @pytest.fixture.
    @staticmethod above @pytest.fixture warns but is still discovered."""
    import types

    from _pytest.warning_types import PytestWarning, warn_explicit_for

    fixture_def = _find_wrapped_fixture_def(obj)
    if fixture_def is None or fixture_def is obj:
        return False
    fixture_func: Any = fixture_def._get_wrapped_function()
    if type(fixture_func) is classmethod or type(fixture_func) is staticmethod:
        fixture_func = fixture_func.__func__
    if type(obj) is classmethod:
        msg = (
            f"cannot discover fixture {name!r} because it is wrapped by "
            f"@classmethod; place @pytest.fixture above @classmethod"
        )
    elif type(obj) is staticmethod:
        msg = (
            f"fixture {name!r} is wrapped by @staticmethod above "
            f"@pytest.fixture; place @pytest.fixture above @staticmethod"
        )
    else:
        msg = f"cannot discover fixture {name!r} due to being wrapped in decorators"
    if isinstance(fixture_func, types.FunctionType):
        warn_explicit_for(fixture_func, PytestWarning(msg))
    return type(obj) is not staticmethod
