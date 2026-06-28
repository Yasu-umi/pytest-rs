import inspect
import warnings
from types import FunctionType

from pytest._warning_types import (  # noqa: F401
    PytestAssertRewriteWarning,
    PytestCacheWarning,
    PytestCollectionWarning,
    PytestConfigWarning,
    PytestDeprecationWarning,
    PytestExperimentalApiWarning,
    PytestFDWarning,
    PytestRemovedIn9Warning,
    PytestRemovedIn10Warning,
    PytestReturnNotNoneWarning,
    PytestUnknownMarkWarning,
    PytestUnraisableExceptionWarning,
    PytestWarning,
    UnformattedWarning,
)

from _pytest._stub import __getattr__  # noqa: E402, F401


def warn_explicit_for(method: FunctionType, message: PytestWarning) -> None:
    lineno = method.__code__.co_firstlineno
    filename = inspect.getfile(method)
    module = method.__module__
    mod_globals = method.__globals__
    try:
        warnings.warn_explicit(
            message,
            type(message),
            filename=filename,
            module=module,
            registry=mod_globals.setdefault("__warningregistry__", {}),
            lineno=lineno,
        )
    except Warning as w:
        raise type(w)(f"{w}\n at {filename}:{lineno}") from None
