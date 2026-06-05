"""Marks (@pytest.mark.*) and pytest.param: metadata records only."""


class Mark:
    def __init__(self, name, args=(), kwargs=None):
        self.name = name
        self.args = tuple(args)
        self.kwargs = dict(kwargs or {})

    def __repr__(self):
        return f"Mark({self.name!r}, {self.args!r}, {self.kwargs!r})"

    def __eq__(self, other):
        if not isinstance(other, Mark):
            return NotImplemented
        return (self.name, self.args, self.kwargs) == (other.name, other.args, other.kwargs)

    def __hash__(self):
        return hash((self.name, self.args))

    def combined_with(self, other):
        assert self.name == other.name
        return Mark(self.name, self.args + other.args, {**self.kwargs, **other.kwargs})


class MarkDecorator:
    def __init__(self, mark, *, _ispytest=False):
        self.mark = mark

    @property
    def name(self):
        return self.mark.name

    @property
    def args(self):
        return self.mark.args

    @property
    def kwargs(self):
        return self.mark.kwargs

    def __repr__(self):
        return f"MarkDecorator({self.mark!r})"

    def __eq__(self, other):
        if isinstance(other, MarkDecorator):
            return self.mark == other.mark
        return NotImplemented

    def __hash__(self):
        return hash(self.mark)

    def with_args(self, *args, **kwargs):
        """Bind arguments without applying — even a lone callable arg is an
        argument, not a decoration target."""
        return MarkDecorator(
            Mark(self.mark.name, self.mark.args + args, {**self.mark.kwargs, **kwargs})
        )

    def __call__(self, *args, **kwargs):
        if len(args) == 1 and not kwargs and (callable(args[0]) or isinstance(args[0], type)):
            func = args[0]
            existing = list(getattr(func, "pytestmark", []))
            existing.append(self.mark)
            func.pytestmark = existing
            return func
        return self.with_args(*args, **kwargs)


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
