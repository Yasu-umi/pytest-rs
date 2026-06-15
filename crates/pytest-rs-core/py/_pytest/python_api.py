from __future__ import annotations

from collections.abc import Sized

from pytest._approx import approx as approx  # noqa: F401


def _is_sequence_like(expected: object) -> bool:
    return (
        hasattr(expected, "__getitem__")
        and isinstance(expected, Sized)
        and not isinstance(expected, (str, bytes))
    )


def _recursive_sequence_map(f, x):
    """Recursively map a function over a sequence of arbitrary depth (upstream
    _pytest.python_api._recursive_sequence_map)."""
    if isinstance(x, (list, tuple)):
        seq_type = type(x)
        return seq_type(_recursive_sequence_map(f, xi) for xi in x)
    elif _is_sequence_like(x):
        return [_recursive_sequence_map(f, xi) for xi in x]
    else:
        return f(x)


from _pytest._stub import __getattr__  # noqa: E402, F401
