"""The @pytest.fixture decorator: records metadata, resolved by the engine."""


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
        if hasattr(function, "pytestmark"):
            # Marks below the @fixture decorator are inert (#3364).
            import warnings

            from _pytest.deprecated import MARKED_FIXTURE

            warnings.warn(MARKED_FIXTURE, stacklevel=2)
        function._pytestfixturefunction = self
        return function


def fixture(
    fixture_function=None, *, scope="function", params=None, autouse=False, ids=None, name=None
):
    marker = FixtureFunctionMarker(scope=scope, params=params, autouse=autouse, ids=ids, name=name)
    if fixture_function is not None:
        return marker(fixture_function)
    return marker


def yield_fixture(
    fixture_function=None, *, scope="function", params=None, autouse=False, ids=None, name=None
):
    """Deprecated alias for :func:`fixture`."""
    import warnings

    from _pytest.deprecated import YIELD_FIXTURE

    warnings.warn(YIELD_FIXTURE, stacklevel=2)
    return fixture(
        fixture_function, scope=scope, params=params, autouse=autouse, ids=ids, name=name
    )
