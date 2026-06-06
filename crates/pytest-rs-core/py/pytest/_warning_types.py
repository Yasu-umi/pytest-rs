class PytestWarning(UserWarning):
    pass


class PytestDeprecationWarning(PytestWarning, DeprecationWarning):
    pass


class PytestRemovedIn9Warning(PytestDeprecationWarning):
    pass


class PytestRemovedIn10Warning(PytestDeprecationWarning):
    pass


class PytestCollectionWarning(PytestWarning):
    pass


class PytestConfigWarning(PytestWarning):
    pass


class PytestUnknownMarkWarning(PytestWarning):
    pass


class PytestUnraisableExceptionWarning(PytestWarning):
    pass


class PytestAssertRewriteWarning(PytestWarning):
    pass


class PytestCacheWarning(PytestWarning):
    pass


class PytestReturnNotNoneWarning(PytestWarning):
    pass


class PytestExperimentalApiWarning(PytestWarning, FutureWarning):
    @classmethod
    def simple(cls, apiname):
        return cls(f"{apiname} is an experimental api that may change over time")


class PytestFDWarning(PytestWarning):
    """When the lsof plugin finds leaked fds."""
