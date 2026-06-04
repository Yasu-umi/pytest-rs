"""pytest.approx (numeric subset; sequences/dicts of numbers)."""


class _Approx:
    DEFAULT_REL = 1e-6
    DEFAULT_ABS = 1e-12

    def __init__(self, expected, rel=None, abs=None):
        self.expected = expected
        self.rel = rel
        self.abs = abs

    def _eq_scalar(self, actual, expected):
        if expected == actual:
            return True
        abs_tol = self.abs if self.abs is not None else self.DEFAULT_ABS
        rel_tol = self.rel if self.rel is not None else self.DEFAULT_REL
        return abs(actual - expected) <= max(abs_tol, rel_tol * abs(expected))

    def __eq__(self, actual):
        expected = self.expected
        if isinstance(expected, dict):
            return (
                isinstance(actual, dict)
                and actual.keys() == expected.keys()
                and all(self._eq_scalar(actual[k], expected[k]) for k in expected)
            )
        if isinstance(expected, (list, tuple)):
            return len(actual) == len(expected) and all(
                self._eq_scalar(a, e) for a, e in zip(actual, expected, strict=False)
            )
        return self._eq_scalar(actual, expected)

    def __ne__(self, actual):
        return not (self == actual)

    def __repr__(self):
        return f"approx({self.expected!r})"


def approx(expected, rel=None, abs=None, nan_ok=False):
    return _Approx(expected, rel=rel, abs=abs)
