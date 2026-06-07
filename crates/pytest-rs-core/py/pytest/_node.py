"""The `request.node` object: a minimal pytest Item surface."""

# Marks added at runtime (node.add_marker / request.applymarker) for the
# currently running item; the engine re-evaluates xfail against these and
# clears the list per item.
_added_marks: list = []


def record_added_mark(marker):
    mark = getattr(marker, "mark", marker)
    name = getattr(mark, "name", None)
    if isinstance(name, str):
        _added_marks.append((name, mark))


def added_marks():
    return list(_added_marks)


def clear_added_marks():
    _added_marks.clear()


class Collector:
    """Stub collector base (annotations/isinstance upstream)."""

    class CollectError(Exception):
        """An error during collection, shown without a traceback."""


class Item:
    """Stub item base (annotations/isinstance upstream)."""


class File:
    """Stub file collector base."""


class DoctestNode:
    """Node subtype for doctest items; recognized by _pytest.doctest.DoctestItem."""

    _pytest_doctest_item = True

    def __init__(
        self, nodeid, name, marks, fixturenames=None, function=None, path=None, lineno=None
    ):
        self.nodeid = nodeid
        self.name = name
        self.own_markers = list(marks)
        self.fixturenames = list(fixturenames or [])
        self.function = function
        self.obj = function
        self.path = path
        self.lineno = lineno

    @property
    def keywords(self):
        """Mark names (plus the node name) as a mapping — pytest's
        node.keywords, for the common `"xfail" in item.keywords` probes."""
        keywords = {self.name: True}
        for marker in self.own_markers:
            keywords[marker.name] = marker
        return keywords

    def warn(self, warning):
        """Issue a warning attributed to this item's definition site
        (pytest's Node.warn: warn_explicit with the item location)."""
        import warnings

        warnings.warn_explicit(
            warning,
            category=None,
            filename=self.path or "<unknown>",
            lineno=self.lineno or 0,
        )

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
        record_added_mark(marker)


# Session.shouldfail set by plugins (pytest-timeout's session deadline):
# the runner polls this between items and aborts with the message banner.
_session_state: dict = {"shouldfail": None, "items": []}


def session_shouldfail():
    return _session_state["shouldfail"]


def set_session_items(items):
    """Collected item proxies, published once collection finishes (the
    engine fires pytest_collection_finish with them on the session)."""
    _session_state["items"] = list(items)


class _NodeSession:
    """Minimal stand-in for pytest's Session as seen from item.session."""

    def __init__(self, config):
        self.config = config

    @property
    def shouldfail(self):
        return _session_state["shouldfail"]

    @shouldfail.setter
    def shouldfail(self, value):
        _session_state["shouldfail"] = value

    @property
    def items(self):
        return _session_state["items"]

    @property
    def testscollected(self):
        return len(_session_state["items"])


class Node(Item):
    def __init__(
        self, nodeid, name, marks, fixturenames=None, function=None, path=None, lineno=None
    ):
        self.nodeid = nodeid
        self.name = name
        self.own_markers = list(marks)
        self.fixturenames = list(fixturenames or [])
        self.function = function
        self.obj = function
        self.path = path
        self.lineno = lineno

    @property
    def keywords(self):
        """Mark names (plus the node name) as a mapping — pytest's
        node.keywords, for the common `"xfail" in item.keywords` probes."""
        keywords = {self.name: True}
        for marker in self.own_markers:
            keywords[marker.name] = marker
        return keywords

    @property
    def session(self):
        """item.session shim: enough for plugins reaching
        item.session.config (e.g. pytest-timeout's session deadline)."""
        return _NodeSession(getattr(self, "config", None))

    def warn(self, warning):
        """Issue a warning attributed to this item's definition site
        (pytest's Node.warn: warn_explicit with the item location)."""
        import warnings

        warnings.warn_explicit(
            warning,
            category=None,
            filename=self.path or "<unknown>",
            lineno=self.lineno or 0,
        )

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
        record_added_mark(marker)


class Function(Node):
    """Test-function node; the engine builds these for collected test items
    (conftest hooks isinstance-check pytest.Function)."""
