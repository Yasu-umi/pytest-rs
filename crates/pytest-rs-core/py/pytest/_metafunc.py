"""The metafunc object passed to pytest_generate_tests hooks.

parametrize() calls are recorded as parametrize marks; the engine merges
them with decorator marks and expands items as usual.

_calls is also populated so that unit-testing Metafunc directly works
(testing/python/metafunc.py constructs Metafunc, calls .parametrize(),
then inspects ._calls).
"""

from __future__ import annotations

from collections.abc import Callable, Iterable, Sequence
from typing import TYPE_CHECKING, Any

from _pytest._io.saferepr import saferepr
from _pytest.outcomes import fail
from _pytest.scope import Scope as ScopeEnum

from pytest._idmaker import IdMaker
from pytest._marks import HIDDEN_PARAM, ParamSpec
from pytest._marks import mark as _mark
from pytest._node import Collector

if TYPE_CHECKING:
    from _pytest.config import Config
    from _pytest.fixtures import FuncFixtureInfo
    from _pytest.python import FunctionDefinition
    from _pytest.scope import _ScopeName


def combine_generate_hooks(hooks: list) -> Callable:
    def run(metafunc: Metafunc) -> None:
        for hook in hooks:
            hook(metafunc)

    return run


class _Definition:
    def __init__(self, function: Any, marks: list | None = None) -> None:
        self.function = function
        self.obj = function
        self.own_markers: list = list(marks or [])
        self._nodeid: str = ""

    @property
    def nodeid(self) -> str:
        return self._nodeid

    def get_closest_marker(self, name: str, default: Any = None) -> Any:
        for marker in self.own_markers:
            if marker.name == name:
                return marker
        return default


class CallSpec2:
    def __init__(
        self,
        params: dict[str, object] | None = None,
        indices: dict[str, int] | None = None,
        _arg2scope: dict[str, ScopeEnum] | None = None,
        _idlist: Sequence[str] | None = None,
        marks: list | None = None,
    ) -> None:
        self.params: dict[str, object] = dict(params) if params else {}
        self.indices: dict[str, int] = dict(indices) if indices else {}
        self._arg2scope: dict[str, ScopeEnum] = dict(_arg2scope) if _arg2scope else {}
        self._idlist: list[str] = list(_idlist) if _idlist else []
        self.marks: list = list(marks) if marks else []

    @property
    def id(self) -> str:
        return "-".join(str(i) for i in self._idlist)

    def setmulti(
        self,
        *,
        argnames: Iterable[str],
        valset: Iterable[object],
        id: str,
        marks: Iterable[Any],
        scope: ScopeEnum,
        param_index: int,
        nodeid: str = "",
    ) -> CallSpec2:
        params = self.params.copy()
        indices = self.indices.copy()
        arg2scope = dict(self._arg2scope)
        for arg, val in zip(argnames, valset):
            if arg in params:
                raise Collector.CollectError(f"{nodeid}: duplicate parametrization of {arg!r}")
            params[arg] = val
            indices[arg] = param_index
            arg2scope[arg] = scope
        new_idlist = list(self._idlist)
        if id is not HIDDEN_PARAM:
            new_idlist.append(id)
        new_marks = list(self.marks)
        for m in marks:
            if hasattr(m, "mark"):
                new_marks.append(m.mark)
            else:
                new_marks.append(m)
        return CallSpec2(
            params=params,
            indices=indices,
            _arg2scope=arg2scope,
            _idlist=new_idlist,
            marks=new_marks,
        )

    def getparam(self, name: str) -> object:
        try:
            return self.params[name]
        except KeyError as e:
            raise ValueError(name) from e


def _parse_argnames(argnames: str | Sequence[str]) -> list[str]:
    if isinstance(argnames, str):
        return [x.strip() for x in argnames.split(",") if x.strip()]
    return list(argnames)


def _resolve_parametersets(
    argnames: list[str], argvalues: Iterable[Any], func: Any
) -> list[ParamSpec]:
    nargs = len(argnames)
    sets: list[ParamSpec] = []
    for val in argvalues:
        if isinstance(val, ParamSpec):
            if nargs == 1 and len(val.values) == 1:
                sets.append(val)
            elif len(val.values) == nargs:
                sets.append(val)
            else:
                sets.append(val)
        elif nargs == 1:
            sets.append(ParamSpec((val,), [], None))
        elif isinstance(val, (tuple, list)):
            sets.append(ParamSpec(tuple(val), [], None))
        else:
            sets.append(ParamSpec((val,), [], None))
    return sets


def _find_parametrized_scope(
    argnames: list[str],
    arg2fixturedefs: dict[str, list],
    indirect: bool | Sequence[str],
) -> ScopeEnum:
    if indirect is True:
        all_indirect: set[str] = set(argnames)
    elif isinstance(indirect, (list, tuple)):
        all_indirect = set(indirect)
    else:
        all_indirect = set()

    used_scopes: list[ScopeEnum] = []
    for argname in argnames:
        if argname not in all_indirect:
            continue
        fixturedefs = arg2fixturedefs.get(argname)
        if fixturedefs:
            used_scopes.append(fixturedefs[-1]._scope)

    if used_scopes:
        return min(used_scopes)
    return ScopeEnum.Function


class Metafunc:
    function: Any
    definition: _Definition | FunctionDefinition
    fixturenames: list[str]
    module: Any
    cls: Any
    config: Config | None
    _parametrize_marks: list
    _calls: list[CallSpec2]
    _params_directness: dict[str, str]
    _arg2fixturedefs: dict[str, list]

    def __init__(
        self,
        definition_or_func: Any,
        fixtureinfo_or_names: list[str] | FuncFixtureInfo | None = None,
        module: Any = None,
        cls: Any = None,
        config: Config | None = None,
        marks: list | None = None,
        *,
        _ispytest: bool = False,
    ) -> None:
        if hasattr(definition_or_func, "obj"):
            defn = definition_or_func
            self.definition = defn
            self.function = defn.obj
            if fixtureinfo_or_names is not None:
                self.fixturenames = list(getattr(fixtureinfo_or_names, "names_closure", []))
                self._arg2fixturedefs = getattr(fixtureinfo_or_names, "name2fixturedefs", {})
            else:
                self.fixturenames = []
                self._arg2fixturedefs = {}
            self.config = config
            self.module = module
            self.cls = cls
        else:
            self.function = definition_or_func
            self.definition = _Definition(definition_or_func, marks)
            if isinstance(fixtureinfo_or_names, (list, tuple)):
                self.fixturenames = list(fixtureinfo_or_names)
            else:
                self.fixturenames = list(fixtureinfo_or_names) if fixtureinfo_or_names else []
            self._arg2fixturedefs = {}
            self.module = module
            self.cls = cls
            self.config = config
        self._parametrize_marks = []
        self._calls = []
        self._params_directness = {}

    def parametrize(
        self,
        argnames: str | Sequence[str],
        argvalues: Iterable[Any],
        indirect: bool | Sequence[str] = False,
        ids: Iterable[object | None] | Callable[[Any], object | None] | None = None,
        scope: _ScopeName | None = None,
        *,
        _param_mark: Any = None,
    ) -> None:
        decorator = _mark.parametrize(
            argnames, list(argvalues), indirect=indirect, ids=ids, scope=scope
        )
        self._parametrize_marks.append(decorator.mark)

        argnames_parsed = _parse_argnames(argnames)
        nodeid = getattr(self.definition, "nodeid", "")

        if "request" in argnames_parsed:
            fail(
                f"{nodeid}: 'request' is a reserved name and cannot be used in @pytest.mark.parametrize",
                pytrace=False,
            )

        if scope is not None:
            scope_ = ScopeEnum.from_user(
                scope, descr=f"parametrize() call in {self.function.__name__}"
            )
        else:
            scope_ = _find_parametrized_scope(argnames_parsed, self._arg2fixturedefs, indirect)

        self._validate_if_using_arg_names(argnames_parsed, indirect)

        parametersets = _resolve_parametersets(argnames_parsed, argvalues, self.function)

        if _param_mark and hasattr(_param_mark, "_param_ids_from") and _param_mark._param_ids_from:
            generated_ids = getattr(_param_mark._param_ids_from, "_param_ids_generated", None)
            if generated_ids is not None:
                ids = generated_ids

        resolved_ids = self._resolve_ids(argnames_parsed, ids, parametersets, nodeid)

        if _param_mark and hasattr(_param_mark, "_param_ids_from") and _param_mark._param_ids_from:
            gen = getattr(_param_mark._param_ids_from, "_param_ids_generated", None)
            if gen is None:
                object.__setattr__(
                    _param_mark._param_ids_from, "_param_ids_generated", resolved_ids
                )

        arg_directness = self._resolve_args_directness(argnames_parsed, indirect)
        self._params_directness.update(arg_directness)

        newcalls: list[CallSpec2] = []
        for callspec in self._calls or [CallSpec2()]:
            for param_index, (param_id, param_set) in enumerate(zip(resolved_ids, parametersets)):
                newcallspec = callspec.setmulti(
                    argnames=argnames_parsed,
                    valset=param_set.values,
                    id=param_id,
                    marks=param_set.marks,
                    scope=scope_,
                    param_index=param_index,
                    nodeid=nodeid,
                )
                newcalls.append(newcallspec)
        self._calls = newcalls

    def _resolve_ids(
        self,
        argnames: list[str],
        ids: Iterable[object | None] | Callable[[Any], object | None] | None,
        parametersets: list[ParamSpec],
        nodeid: str,
    ) -> list[str]:
        try:
            return IdMaker(
                argnames=argnames,
                parametersets=parametersets,
                idfn=ids if callable(ids) else None,
                ids=None if ids is None or callable(ids) else list(ids),
                config=self.config,
                nodeid=nodeid,
                func_name=self.function.__name__,
            ).make_unique_parameterset_ids()
        except Exception:
            return self._resolve_ids_simple(argnames, ids, parametersets)

    @staticmethod
    def _resolve_ids_simple(
        argnames: list[str],
        ids: Iterable[object | None] | Callable[[Any], object | None] | None,
        parametersets: list[ParamSpec],
    ) -> list[str]:
        result: list[str] = []
        for i, ps in enumerate(parametersets):
            if ps.id is not None:
                result.append(ps.id)
            elif ids is not None and not callable(ids) and isinstance(ids, (list, tuple)):
                id_list = list(ids)
                if i < len(id_list) and id_list[i] is not None:
                    result.append(str(id_list[i]))
                    continue
                result.append("-".join(saferepr(v, maxsize=50) for v in ps.values))
            elif ids is not None and callable(ids):
                val = ps.values[0] if len(ps.values) == 1 else ps.values
                id_val = ids(val)
                if id_val is not None:
                    result.append(str(id_val))
                else:
                    result.append("-".join(saferepr(v, maxsize=50) for v in ps.values))
            else:
                result.append("-".join(saferepr(v, maxsize=50) for v in ps.values))
        return result

    def _validate_if_using_arg_names(
        self, argnames: list[str], indirect: bool | Sequence[str]
    ) -> None:
        pass

    def _resolve_args_directness(
        self, argnames: list[str], indirect: bool | Sequence[str]
    ) -> dict[str, str]:
        if indirect is True:
            return {name: "indirect" for name in argnames}
        elif isinstance(indirect, (list, tuple)):
            return {name: ("indirect" if name in indirect else "direct") for name in argnames}
        return {name: "direct" for name in argnames}
