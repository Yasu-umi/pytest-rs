"""The `request.node` object: a minimal pytest Item surface."""


class Collector:
    """Stub collector base (annotations/isinstance upstream)."""

    class CollectError(Exception):
        """An error during collection, shown without a traceback."""


class Item:
    """Stub item base (annotations/isinstance upstream)."""


class File:
    """Stub file collector base."""


class Node:
    def __init__(self, nodeid, name, marks, fixturenames=None, function=None):
        self.nodeid = nodeid
        self.name = name
        self.own_markers = list(marks)
        self.fixturenames = list(fixturenames or [])
        self.function = function
        self.obj = function

    def get_closest_marker(self, name, default=None):
        for marker in self.own_markers:
            if marker.name == name:
                return marker
        return default

    def iter_markers(self, name=None):
        for marker in self.own_markers:
            if name is None or marker.name == name:
                yield marker

    def iter_markers_with_node(self, name=None):
        for marker in self.own_markers:
            if name is None or marker.name == name:
                yield self, marker

    def add_marker(self, marker, append=True):
        from pytest._marks import Mark, MarkDecorator

        if isinstance(marker, str):
            marker = Mark(marker)
        elif isinstance(marker, MarkDecorator):
            marker = marker.mark
        if append:
            self.own_markers.append(marker)
        else:
            self.own_markers.insert(0, marker)
