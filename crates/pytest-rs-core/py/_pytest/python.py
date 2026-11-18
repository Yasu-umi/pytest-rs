from __future__ import annotations

from pytest._idmaker import IdMaker as IdMaker  # noqa: F401
from pytest._marks import HIDDEN_PARAM as HIDDEN_PARAM  # noqa: F401
from pytest._marks import ParamSpec as ParameterSet  # noqa: F401
from pytest._metafunc import CallSpec2 as CallSpec2  # noqa: F401
from pytest._metafunc import Metafunc as Metafunc  # noqa: F401
from pytest._metafunc import _find_parametrized_scope as _find_parametrized_scope  # noqa: F401
from pytest._node import Class as Class  # noqa: F401
from pytest._node import File as Module  # noqa: F401
from pytest._node import Function as Function  # noqa: F401
from pytest._node import FunctionDefinition as FunctionDefinition  # noqa: F401

from _pytest._stub import __getattr__ as _stub_getattr  # noqa: E402


def __getattr__(name):
    # pytest-rs reproduces pytest 9.0.3, which has no `CallSpec` (9.2 renamed
    # CallSpec2 to it). Plugins feature-detect the rename with
    # `try: from _pytest.python import CallSpec / except ImportError:` (anyio
    # >= 4.16), so it must raise like 9.0.3 rather than resolve to an
    # unsupported-internal stub that only fails once called.
    if name == "CallSpec":
        raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
    return _stub_getattr(name)
