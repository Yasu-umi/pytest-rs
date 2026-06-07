class PytestWarning(UserWarning):
    __module__ = "pytest"


class PytestDeprecationWarning(PytestWarning, DeprecationWarning):
    __module__ = "pytest"


class PytestRemovedIn9Warning(PytestDeprecationWarning):
    __module__ = "pytest"


class PytestRemovedIn10Warning(PytestDeprecationWarning):
    __module__ = "pytest"


class PytestCollectionWarning(PytestWarning):
    __module__ = "pytest"


class PytestConfigWarning(PytestWarning):
    __module__ = "pytest"


class PytestUnknownMarkWarning(PytestWarning):
    __module__ = "pytest"


class PytestUnraisableExceptionWarning(PytestWarning):
    __module__ = "pytest"


class PytestAssertRewriteWarning(PytestWarning):
    __module__ = "pytest"


class PytestCacheWarning(PytestWarning):
    __module__ = "pytest"


class PytestReturnNotNoneWarning(PytestWarning):
    __module__ = "pytest"


class PytestExperimentalApiWarning(PytestWarning, FutureWarning):
    __module__ = "pytest"

    @classmethod
    def simple(cls, apiname):
        return cls(f"{apiname} is an experimental api that may change over time")


class PytestFDWarning(PytestWarning):
    """When the lsof plugin finds leaked fds."""

    __module__ = "pytest"


class UnformattedWarning:
    """A warning category meant to be formatted before use (upstream
    _pytest.warning_types.UnformattedWarning)."""

    def __init__(self, category, template):
        self.category = category
        self.template = template

    def format(self, **kwargs):
        """Return an instance of the warning category, formatted with kwargs."""
        return self.category(self.template.format(**kwargs))
