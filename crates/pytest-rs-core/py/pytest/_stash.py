"""Stash / StashKey: type-safe heterogeneous mapping, ported from
_pytest/stash.py (plugins key data off config.stash / node.stash; PEP 695
generics instead of Generic[T], like the ExceptionInfo port)."""

from __future__ import annotations

from typing import Any, cast

__all__ = ["Stash", "StashKey"]


class StashKey[T]:
    """``StashKey`` is an object used as a key to a :class:`Stash`.

    A ``StashKey`` is associated with the type ``T`` of the value of the key.

    A ``StashKey`` is unique and cannot conflict with another key.
    """

    __slots__ = ()


class Stash:
    r"""``Stash`` is a type-safe heterogeneous mutable mapping that
    allows keys and value types to be defined separately from
    where it (the ``Stash``) is created.
    """

    __slots__ = ("_storage",)

    def __init__(self) -> None:
        self._storage: dict[StashKey[Any], object] = {}

    def __setitem__[T](self, key: StashKey[T], value: T) -> None:
        """Set a value for key."""
        self._storage[key] = value

    def __getitem__[T](self, key: StashKey[T]) -> T:
        """Get the value for key.

        Raises ``KeyError`` if the key wasn't set before.
        """
        return cast(T, self._storage[key])

    def get[T, D](self, key: StashKey[T], default: D) -> T | D:
        """Get the value for key, or return default if the key wasn't set
        before."""
        try:
            return self[key]
        except KeyError:
            return default

    def setdefault[T](self, key: StashKey[T], default: T) -> T:
        """Return the value of key if already set, otherwise set the value
        of key to default and return default."""
        try:
            return self[key]
        except KeyError:
            self[key] = default
            return default

    def __delitem__[T](self, key: StashKey[T]) -> None:
        """Delete the value for key.

        Raises ``KeyError`` if the key wasn't set before.
        """
        del self._storage[key]

    def __contains__[T](self, key: StashKey[T]) -> bool:
        """Return whether key was set."""
        return key in self._storage

    def __len__(self) -> int:
        """Return how many items exist in the stash."""
        return len(self._storage)
