from pytest._marks import Mark, MarkDecorator, MarkGenerator  # noqa: F401


class MarkMatcher:
    def __init__(self, own_mark_names):
        self.own_mark_names = set(own_mark_names)

    @classmethod
    def from_markers(cls, markers):
        return cls(m.name for m in markers)

    def __contains__(self, name):
        return name in self.own_mark_names


def get_empty_parameterset_mark(config, argnames, function):
    from pytest._marks import mark

    return mark.skip(reason=f"got empty parameter set for {argnames!r}")


def pytest_configure(config):
    pass
