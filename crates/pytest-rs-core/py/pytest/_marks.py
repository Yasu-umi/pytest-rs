"""Marks (@pytest.mark.*) and pytest.param: metadata records only."""


class Mark:
    def __init__(self, name, args=(), kwargs=None):
        self.name = name
        self.args = tuple(args)
        self.kwargs = dict(kwargs or {})

    def __repr__(self):
        return f"Mark({self.name!r}, {self.args!r}, {self.kwargs!r})"


class MarkDecorator:
    def __init__(self, mark):
        self.mark = mark

    @property
    def name(self):
        return self.mark.name

    def __call__(self, *args, **kwargs):
        if len(args) == 1 and not kwargs and (callable(args[0]) or isinstance(args[0], type)):
            func = args[0]
            existing = list(getattr(func, "pytestmark", []))
            existing.append(self.mark)
            func.pytestmark = existing
            return func
        return MarkDecorator(
            Mark(self.mark.name, self.mark.args + args, {**self.mark.kwargs, **kwargs})
        )


class MarkGenerator:
    def __init__(self, *, _ispytest=False):
        pass

    def __getattr__(self, name):
        if name.startswith("_"):
            raise AttributeError(name)
        return MarkDecorator(Mark(name))


mark = MarkGenerator()


class ParamSpec:
    """The object returned by pytest.param(): values + per-param marks/id."""

    def __init__(self, values, marks, id):
        self.values = tuple(values)
        self.marks = list(marks)
        self.id = id


def param(*values, marks=(), id=None):
    if not isinstance(marks, list | tuple):
        marks = [marks]
    return ParamSpec(values, [decorator.mark for decorator in marks], id)
