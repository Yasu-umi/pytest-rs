"""The @pytest.fixture decorator: records metadata, resolved by the engine."""

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
        # Upstream rejects fixtures applied to a class and double-decoration
        # (@fixture returns a FixtureFunctionDefinition that the next @fixture
        # would re-wrap); mirror both guards (#fixture_disallow_twice).
        if inspect.isclass(function):
            raise ValueError("class fixtures not supported (maybe in the future)")
        if getattr(function, "_pytestfixturefunction", False):
            raise ValueError(
                "@pytest.fixture is being applied more than once to the same "
                f"function {getattr(function, '__name__', function)!r}"
            )
        if hasattr(function, "pytestmark"):
            # Marks below the @fixture decorator are inert (#3364).
            from _pytest.deprecated import MARKED_FIXTURE

            warnings.warn(MARKED_FIXTURE, stacklevel=2)
        function._pytestfixturefunction = self
        # Real pytest wraps fixtures in FixtureFunctionDefinition which has
        # _get_wrapped_function(); replicate that on the plain function so
        # get_real_func() tests can call it.
        _fn = function
        function._get_wrapped_function = lambda: _fn
        return function


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
