"""xunit-style setup/teardown helpers (setup_module, setup_function, ...).

The engine calls these around tests; teardowns are bound into zero-arg
callables and pushed onto the session finalizer stack.
"""

import inspect


def first_non_fixture(obj, *names):
    """Return the first attribute named in `names` that exists on `obj` and is
    not a pytest fixture, mirroring _pytest.python._get_first_non_fixture_func.

    A function decorated with @pytest.fixture but named like an xunit hook
    (e.g. `setup_module`) must not also run as the xunit hook (#517)."""
    for name in names:
        meth = getattr(obj, name, None)
        if meth is not None and not hasattr(meth, "_pytestfixturefunction"):
            return meth
    return None


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
