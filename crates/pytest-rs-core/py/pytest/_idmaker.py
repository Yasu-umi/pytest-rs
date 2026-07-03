"""Parametrization ID generation, ported from pytest's _pytest.python.IdMaker.

The pytest test suite (testing/python/metafunc.py) constructs IdMaker directly
to unit-test ID derivation, so we expose a faithful implementation rather than
the engine's internal Rust path.
"""

from __future__ import annotations

import dataclasses
import enum
import re
import textwrap
from collections import Counter, defaultdict

from _pytest._io.saferepr import saferepr
from _pytest.compat import NOTSET, ascii_escaped
from _pytest.outcomes import fail

from pytest._marks import HIDDEN_PARAM
from pytest._node import Collector


def _ascii_escaped_by_config(val, config):
    if config is None:
        escape_option = False
    else:
        escape_option = config.getini(
            "disable_test_id_escaping_and_forfeit_all_rights_to_community_support"
        )
    return val if escape_option else ascii_escaped(val)


def idval_from_value(val, config=None):
    """Make an ID from a value, if the value type is supported."""
    if isinstance(val, (str, bytes)):
        return _ascii_escaped_by_config(val, config)
    elif val is None or isinstance(val, (float, int, bool, complex)):
        return str(val)
    elif isinstance(val, re.Pattern):
        return ascii_escaped(val.pattern)
    elif val is NOTSET:
        pass
    elif isinstance(val, enum.Enum):
        return str(val)
    elif isinstance(getattr(val, "__name__", None), str):
        return val.__name__
    return None


def idval_from_value_required(val, idx, error_prefix="", config=None):
    """Like idval_from_value(), but raises a collect error (upstream's
    IdMaker._idval_from_value_required) if the type is not supported. Called
    directly by the Rust collector for an explicit `ids=[...]` entry."""
    id = idval_from_value(val, config)
    if id is not None:
        return id
    msg = (
        f"{error_prefix}ids contains unsupported value {saferepr(val)} (type: {type(val)!r}) "
        f"at index {idx}. Supported types are: str, bytes, int, float, complex, bool, "
        "enum, regex or anything with a __name__."
    )
    fail(msg, pytrace=False)


@dataclasses.dataclass(frozen=True)
class IdMaker:
    """Make IDs for a parametrization."""

    argnames: object
    parametersets: object
    idfn: object
    ids: object
    config: object
    nodeid: object
    func_name: object

    def make_unique_parameterset_ids(self):
        """Make a unique identifier for each ParameterSet, usable in a node ID.

        Format is <prm_1_token>-...-<prm_n_token>[counter]; the counter suffix is
        appended only when a string wouldn't otherwise be unique. Under
        strict_parametrization_ids, duplicates raise CollectError instead."""
        resolved_ids = list(self._resolve_ids())
        if len(resolved_ids) != len(set(resolved_ids)):
            id_counts = Counter(resolved_ids)

            if self._strict_parametrization_ids_enabled():
                parameters = ", ".join(self.argnames)
                parametersets = ", ".join(
                    [saferepr(list(param.values)) for param in self.parametersets]
                )
                ids = ", ".join(id if id is not HIDDEN_PARAM else "<hidden>" for id in resolved_ids)
                duplicates = ", ".join(
                    id if id is not HIDDEN_PARAM else "<hidden>"
                    for id, count in id_counts.items()
                    if count > 1
                )
                msg = textwrap.dedent(f"""
                    Duplicate parametrization IDs detected, but strict_parametrization_ids is set.

                    Test name:      {self.nodeid}
                    Parameters:     {parameters}
                    Parameter sets: {parametersets}
                    IDs:            {ids}
                    Duplicates:     {duplicates}

                    You can fix this problem using `@pytest.mark.parametrize(..., ids=...)` or `pytest.param(..., id=...)`.
                """).strip()  # noqa: E501
                raise Collector.CollectError(msg)

            id_suffixes: dict[str, int] = defaultdict(int)
            for index, id in enumerate(resolved_ids):
                if id_counts[id] > 1:
                    if id is HIDDEN_PARAM:
                        self._complain_multiple_hidden_parameter_sets()
                    suffix = ""
                    if id and id[-1].isdigit():
                        suffix = "_"
                    new_id = f"{id}{suffix}{id_suffixes[id]}"
                    while new_id in set(resolved_ids):
                        id_suffixes[id] += 1
                        new_id = f"{id}{suffix}{id_suffixes[id]}"
                    resolved_ids[index] = new_id
                    id_suffixes[id] += 1
        assert len(resolved_ids) == len(set(resolved_ids)), f"Internal error: {resolved_ids=}"
        return resolved_ids

    def _strict_parametrization_ids_enabled(self):
        if self.config is None:
            return False
        strict_parametrization_ids = self.config.getini("strict_parametrization_ids")
        if strict_parametrization_ids is None:
            strict_parametrization_ids = self.config.getini("strict")
        return strict_parametrization_ids

    def _resolve_ids(self):
        """Resolve IDs for all ParameterSets (may contain duplicates)."""
        for idx, parameterset in enumerate(self.parametersets):
            if parameterset.id is not None:
                if parameterset.id is HIDDEN_PARAM:
                    yield HIDDEN_PARAM
                else:
                    yield _ascii_escaped_by_config(parameterset.id, self.config)
            elif self.ids and idx < len(self.ids) and self.ids[idx] is not None:
                if self.ids[idx] is HIDDEN_PARAM:
                    yield HIDDEN_PARAM
                else:
                    yield self._idval_from_value_required(self.ids[idx], idx)
            else:
                yield "-".join(
                    self._idval(val, argname, idx)
                    for val, argname in zip(parameterset.values, self.argnames, strict=True)
                )

    def _idval(self, val, argname, idx):
        """Make an ID for a parameter in a ParameterSet."""
        idval = self._idval_from_function(val, argname, idx)
        if idval is not None:
            return idval
        idval = self._idval_from_hook(val, argname)
        if idval is not None:
            return idval
        idval = self._idval_from_value(val)
        if idval is not None:
            return idval
        return self._idval_from_argname(argname, idx)

    def _idval_from_function(self, val, argname, idx):
        if self.idfn is None:
            return None
        try:
            id = self.idfn(val)
        except Exception as e:
            prefix = f"{self.nodeid}: " if self.nodeid is not None else ""
            msg = "error raised while trying to determine id of parameter '{}' at position {}"
            msg = prefix + msg.format(argname, idx)
            raise ValueError(msg) from e
        if id is None:
            return None
        return self._idval_from_value(id)

    def _idval_from_hook(self, val, argname):
        if self.config:
            id = self.config.hook.pytest_make_parametrize_id(
                config=self.config, val=val, argname=argname
            )
            return id
        return None

    def _idval_from_value(self, val):
        """Make an ID from a value, if the value type is supported."""
        return idval_from_value(val, self.config)

    def _idval_from_value_required(self, val, idx):
        return idval_from_value_required(val, idx, self._make_error_prefix(), self.config)

    @staticmethod
    def _idval_from_argname(argname, idx):
        return str(argname) + str(idx)

    def _complain_multiple_hidden_parameter_sets(self):
        fail(
            f"{self._make_error_prefix()}multiple instances of HIDDEN_PARAM "
            "cannot be used in the same parametrize call, "
            "because the tests names need to be unique."
        )

    def _make_error_prefix(self):
        if self.func_name is not None:
            return f"In {self.func_name}: "
        elif self.nodeid is not None:
            return f"In {self.nodeid}: "
        else:
            return ""
