"""The metafunc object passed to pytest_generate_tests hooks.

parametrize() calls are recorded as parametrize marks; the engine merges
them with decorator marks and expands items as usual.
"""

from pytest._marks import mark as _mark


def combine_generate_hooks(hooks):
    """One callable running every pytest_generate_tests impl (module-level +
    plugin/conftest) on the metafunc, in order."""

    def run(metafunc):
        for hook in hooks:
            hook(metafunc)

    return run


class _Definition:
    """metafunc.definition: the function node plugins read markers off
    (pytest-repeat checks definition.get_closest_marker('repeat'))."""

    def __init__(self, function, marks=None):
        self.function = function
        self.obj = function
        self.own_markers = list(marks or [])

    def get_closest_marker(self, name, default=None):
        for marker in self.own_markers:
            if marker.name == name:
                return marker
        return default


class Metafunc:
    def __init__(self, function, fixturenames, module, cls=None, config=None, marks=None):
        self.function = function
        self.definition = _Definition(function, marks)
        self.fixturenames = list(fixturenames)
        self.module = module
        self.cls = cls
        self.config = config
        self._parametrize_marks = []

    def parametrize(self, argnames, argvalues, indirect=False, ids=None, scope=None):
        # indirect/scope flow through to the parametrize mark; the engine's
        # expand_parametrize routes indirect values to the same-named
        # fixture's request.param.
        decorator = _mark.parametrize(
            argnames, list(argvalues), indirect=indirect, ids=ids, scope=scope
        )
        self._parametrize_marks.append(decorator.mark)
