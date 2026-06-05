"""The metafunc object passed to pytest_generate_tests hooks.

parametrize() calls are recorded as parametrize marks; the engine merges
them with decorator marks and expands items as usual.
"""

from pytest._marks import mark as _mark


class Metafunc:
    def __init__(self, function, fixturenames, module, cls=None, config=None):
        self.function = function
        self.definition = function
        self.fixturenames = list(fixturenames)
        self.module = module
        self.cls = cls
        self.config = config
        self._parametrize_marks = []

    def parametrize(self, argnames, argvalues, indirect=False, ids=None, scope=None):
        if indirect:
            raise NotImplementedError(
                "metafunc.parametrize(indirect=...) is not supported by pytest-rs yet"
            )
        decorator = _mark.parametrize(argnames, list(argvalues), ids=ids)
        self._parametrize_marks.append(decorator.mark)
