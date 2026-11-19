from __future__ import annotations

from pytest._idmaker import IdMaker as IdMaker  # noqa: F401
from pytest._marks import HIDDEN_PARAM as HIDDEN_PARAM  # noqa: F401
from pytest._marks import ParamSpec as ParameterSet  # noqa: F401
from pytest._metafunc import CallSpec2 as CallSpec  # noqa: F401
from pytest._metafunc import Metafunc as Metafunc  # noqa: F401
from pytest._metafunc import _find_parametrized_scope as _find_parametrized_scope  # noqa: F401
from pytest._node import Class as Class  # noqa: F401
from pytest._node import File as Module  # noqa: F401
from pytest._node import Function as Function  # noqa: F401
from pytest._node import FunctionDefinition as FunctionDefinition  # noqa: F401

from _pytest._stub import __getattr__ as _stub_getattr  # noqa: E402


def __getattr__(name):
    if name == "CallSpec2":
        # Renamed to CallSpec in pytest 9.2; the alias warns.
        import warnings

        from _pytest.deprecated import CALLSPEC2_RENAMED

        warnings.warn(CALLSPEC2_RENAMED.format(), stacklevel=2)
        return CallSpec
    return _stub_getattr(name)
