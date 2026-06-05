"""Session-end cache persistence.

Lives in a module named ``cacheprovider.py`` so PytestCacheWarnings issued
by ``Cache.set`` (stacklevel 3) point at ``*/cacheprovider.py:NN`` with the
same source lines as pytest's cacheprovider plugin.
"""


class SessionCacheWriter:
    def __init__(self, config, lastfailed, cached_nodeids):
        self.config = config
        self.lastfailed = lastfailed
        self.cached_nodeids = cached_nodeids

    def write(self):
        config = self.config
        saved_lastfailed = config.cache.get("cache/lastfailed", {})
        if saved_lastfailed != self.lastfailed:
            config.cache.set("cache/lastfailed", self.lastfailed)
        config.cache.set("cache/nodeids", sorted(self.cached_nodeids))


def write_session_cache(config, lastfailed, cached_nodeids):
    SessionCacheWriter(config, lastfailed, cached_nodeids).write()
