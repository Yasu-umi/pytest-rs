"""xunit-style setup/teardown helpers (setup_module, setup_function, ...).

The engine calls these around tests; teardowns are bound into zero-arg
callables and pushed onto the session finalizer stack.
"""

import inspect


def call_optional(func, arg):
    """Call with `arg` if the function accepts a (non-defaulted) positional
    parameter, else without — pytest's optional-argument protocol."""
    try:
        signature = inspect.signature(func)
    except (ValueError, TypeError):
        func()
        return
    wants_arg = any(
        parameter.default is inspect.Parameter.empty
        and parameter.kind
        in (inspect.Parameter.POSITIONAL_ONLY, inspect.Parameter.POSITIONAL_OR_KEYWORD)
        for parameter in signature.parameters.values()
    )
    if wants_arg:
        func(arg)
    else:
        func()


def bind(func, arg):
    """A zero-arg finalizer calling `func` per the optional-arg protocol."""

    def finalizer():
        call_optional(func, arg)

    return finalizer
