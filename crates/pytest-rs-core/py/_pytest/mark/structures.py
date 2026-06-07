from pytest._marks import (  # noqa: F401  # noqa: F401
    Mark,
    MarkDecorator,
    MarkGenerator,
    get_unpacked_marks,
    store_mark,
)
from pytest._marks import ParamSpec as ParameterSet  # noqa: F401

from _pytest._stub import __getattr__  # noqa: E402, F401

EMPTY_PARAMETERSET_OPTION = "empty_parameter_set_mark"
