"""The @pytest.fixture decorator: records metadata, resolved by the engine."""


class FixtureFunctionMarker:
    def __init__(self, scope="function", params=None, autouse=False, ids=None, name=None):
        self.scope = scope
        self.params = list(params) if params is not None else None
        self.autouse = autouse
        self.ids = ids
        self.name = name

    def __call__(self, function):
        function._pytestfixturefunction = self
        return function


def fixture(
    fixture_function=None, *, scope="function", params=None, autouse=False, ids=None, name=None
):
    marker = FixtureFunctionMarker(scope=scope, params=params, autouse=autouse, ids=ids, name=name)
    if fixture_function is not None:
        return marker(fixture_function)
    return marker
