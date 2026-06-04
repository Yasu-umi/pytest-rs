class StashKey:
    """A typed key for Stash."""


class Stash:
    def __init__(self):
        self._storage = {}

    def __setitem__(self, key, value):
        self._storage[key] = value

    def __getitem__(self, key):
        return self._storage[key]

    def __contains__(self, key):
        return key in self._storage

    def __delitem__(self, key):
        del self._storage[key]

    def __len__(self):
        return len(self._storage)

    def get(self, key, default=None):
        return self._storage.get(key, default)

    def setdefault(self, key, default):
        return self._storage.setdefault(key, default)
