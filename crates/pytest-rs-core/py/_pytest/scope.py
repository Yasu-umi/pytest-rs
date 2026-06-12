from __future__ import annotations

from enum import Enum
from functools import total_ordering
from typing import Literal

_ScopeName = Literal["session", "package", "module", "class", "function"]


@total_ordering
class Scope(Enum):
    """Fixture scope, ordered from lower to higher."""

    # Listed from lower to higher.
    Function = "function"
    Class = "class"
    Module = "module"
    Package = "package"
    Session = "session"

    def next_lower(self) -> Scope:
        index = _SCOPE_INDICES[self]
        if index == 0:
            raise ValueError(f"{self} is the lower-most scope")
        return _ALL_SCOPES[index - 1]

    def next_higher(self) -> Scope:
        index = _SCOPE_INDICES[self]
        if index == len(_SCOPE_INDICES) - 1:
            raise ValueError(f"{self} is the upper-most scope")
        return _ALL_SCOPES[index + 1]

    def __lt__(self, other: Scope) -> bool:
        return _SCOPE_INDICES[self] < _SCOPE_INDICES[other]

    @classmethod
    def from_user(cls, scope_name: _ScopeName, descr: str, where: str | None = None) -> Scope:
        from _pytest.outcomes import fail

        try:
            return Scope(scope_name)
        except ValueError:
            fail(
                "{} {}got an unexpected scope value '{}'".format(
                    descr, f"from {where} " if where else "", scope_name
                ),
                pytrace=False,
            )


_ALL_SCOPES = list(Scope)
_SCOPE_INDICES = {scope: index for index, scope in enumerate(_ALL_SCOPES)}
HIGH_SCOPES = [x for x in Scope if x is not Scope.Function]
