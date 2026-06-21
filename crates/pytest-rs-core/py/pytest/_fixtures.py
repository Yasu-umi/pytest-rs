"""The @pytest.fixture decorator: records metadata, resolved by the engine."""

import functools
import inspect
import warnings


class FixtureLookupError(LookupError):
    """Raised by request.getfixturevalue() for unknown fixture names."""


class FixtureFunctionMarker:
    def __init__(self, scope="function", params=None, autouse=False, ids=None, name=None):
        self.scope = scope
        self.params = list(params) if params is not None else None
        self.autouse = autouse
        self.ids = ids
        self.name = name

    def __call__(self, function):
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
            from _pytest.deprecated import MARKED_FIXTURE
            warnings.warn(MARKED_FIXTURE, stacklevel=2)
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
