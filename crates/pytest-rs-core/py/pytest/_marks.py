"""Marks (@pytest.mark.*) and pytest.param: metadata records only."""

import enum
import warnings

from pytest._warning_types import PytestUnknownMarkWarning


class _HiddenParam(enum.Enum):
    token = 0


#: pytest.param(..., id=HIDDEN_PARAM) hides the parameter set from the test ID.
HIDDEN_PARAM = _HiddenParam.token


class Mark:
    def __init__(
        self,
        name,
        args=(),
        kwargs=None,
        *,
        _param_ids_from=None,
        _param_ids_generated=None,
        _ispytest=False,
    ):
        self.name = name
        self.args = tuple(args)
        self.kwargs = dict(kwargs or {})
        self._param_ids_from = _param_ids_from
        self._param_ids_generated = _param_ids_generated

    def __repr__(self):
        return f"Mark({self.name!r}, {self.args!r}, {self.kwargs!r})"

    def __eq__(self, other):
        if not isinstance(other, Mark):
            return NotImplemented
        return (self.name, self.args, self.kwargs) == (other.name, other.args, other.kwargs)

    def __hash__(self):
        return hash((self.name, self.args))

    def _has_param_ids(self):
        return "ids" in self.kwargs or len(self.args) >= 4

    def combined_with(self, other):
        assert self.name == other.name
        param_ids_from = None
        if self.name == "parametrize":
            if other._has_param_ids():
                param_ids_from = other
            elif self._has_param_ids():
                param_ids_from = self
        return Mark(
            self.name,
            self.args + other.args,
            {**self.kwargs, **other.kwargs},
            _param_ids_from=param_ids_from,
        )


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
        other = Mark(self.mark.name, args, kwargs)
        return MarkDecorator(self.mark.combined_with(other))

    def __call__(self, *args, **kwargs):
        if args and not kwargs:
            func = args[0]
            is_class = isinstance(func, type)
            # For staticmethods/classmethods, the marks are eventually
            # fetched from the function object, not the descriptor (#12863).
            unwrapped = func
            if isinstance(func, staticmethod | classmethod):
                unwrapped = func.__func__
            # Lambdas are mark arguments, not decoration targets (upstream
            # istestfunc excludes "<lambda>").
            istestfunc = (
                callable(unwrapped) and getattr(unwrapped, "__name__", "<lambda>") != "<lambda>"
            )
            if len(args) == 1 and (istestfunc or is_class):
                store_mark(unwrapped, self.mark)
                return func
        return self.with_args(*args, **kwargs)


def get_unpacked_marks(obj, *, consider_mro=True):
    """Obtain the unpacked marks that are stored on an object (upstream
    _pytest.mark.structures.get_unpacked_marks).

    If obj is a class and consider_mro is true, return marks applied to
    this class and all of its super-classes in MRO order (base first)."""
    if isinstance(obj, type):
        if not consider_mro:
            mark_lists = [obj.__dict__.get("pytestmark", [])]
        else:
            mark_lists = [x.__dict__.get("pytestmark", []) for x in reversed(obj.__mro__)]
        mark_list = []
        for item in mark_lists:
            if isinstance(item, list):
                mark_list.extend(item)
            else:
                mark_list.append(item)
    else:
        mark_attribute = getattr(obj, "pytestmark", [])
        if isinstance(mark_attribute, list):
            mark_list = mark_attribute
        else:
            mark_list = [mark_attribute]
    result = []
    for m in mark_list:
        m = getattr(m, "mark", m)
        if not isinstance(m, Mark):
            raise TypeError(f"got {m!r} instead of Mark")
        result.append(m)
    return result


def store_mark(obj, mark, *, stacklevel=3):
    """Store a Mark on an object (upstream store_mark): reassigns pytestmark
    from the object's OWN marks so inherited lists are never mutated."""
    if hasattr(obj, "_pytestfixturefunction"):
        # Marks applied to a fixture are inert (#3364).
        from _pytest.deprecated import MARKED_FIXTURE

        warnings.warn(MARKED_FIXTURE, stacklevel=stacklevel)
    obj.pytestmark = [*get_unpacked_marks(obj, consider_mro=False), mark]


class MarkGenerator:
    def __init__(self, *, _ispytest=False):
        self._config = None
        self._strict = False
        self._markers = set()

    def __getattr__(self, name):
        if name.startswith("_"):
            raise AttributeError(name)
        if self._config is not None:
            # Known-marks set is a cache; refresh from the (mutable) ini
            # before deciding the mark really is unknown.
            if name not in self._markers:
                for line in self._config.getini("markers") or []:
                    marker = line.split(":")[0].split("(")[0].strip()
                    if marker:
                        self._markers.add(marker)
            if name not in self._markers:
                # Raise a specific error for common misspellings of "parametrize".
                if name in ("parameterize", "parametrise", "parameterise"):
                    from pytest._outcomes import fail

                    fail(f"Unknown '{name}' mark, did you mean 'parametrize'?")
                # Under --strict-markers the engine fails collection itself.
                if not self._strict:
                    warnings.warn(
                        f"Unknown pytest.mark.{name} - is this a typo?  You can register "
                        "custom marks to avoid this warning - for details, see "
                        "https://docs.pytest.org/en/stable/how-to/mark.html",
                        PytestUnknownMarkWarning,
                        2,
                    )
        return MarkDecorator(Mark(name))


mark = MarkGenerator()


def configure_mark_generator(config, builtin_names, strict, strict_parametrization_ids=False):
    """Arm unknown-mark validation once the session config is known."""
    mark._config = config
    mark._strict = strict
    mark._markers = set(builtin_names)
    mark._strict_parametrization_ids = strict_parametrization_ids


class ParamSpec:
    """The object returned by pytest.param(): values + per-param marks/id."""

    def __init__(self, values, marks, id):
        self.values = tuple(values)
        self.marks = list(marks)
        self.id = id


def param(*values, marks=(), id=None):
    if not isinstance(marks, list | tuple):
        marks = [marks]
    if id is not None and id is not HIDDEN_PARAM and not isinstance(id, str):
        raise TypeError(
            "Expected id to be a string or a `pytest.HIDDEN_PARAM` sentinel, "
            f"got {type(id)}: {id!r}"
        )
    return ParamSpec(
        values,
        [m if isinstance(m, Mark) else m.mark for m in marks],
        id,
    )
