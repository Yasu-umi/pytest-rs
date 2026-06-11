"""Minimal FixtureManager / FixtureDef shims for plugins that drive pytest's
fixture internals directly — chiefly pytest-bdd, which injects a per-step
fixture into request._fixturemanager._arg2fixturedefs and then resolves it
through request.getfixturevalue.

The Rust engine owns fixture resolution; this exposes just enough of the
pytest FixtureManager surface:
  - _arg2fixturedefs: {name: [ShimFixtureDef]} seeded from the Rust registry
  - getfixturedefs(name, node)
  - _register_fixture(name, func, nodeid, ...)  (pytest >= 8.1 inject path)

FixtureRequest.getfixturevalue (Rust) consults a request's manager for names
not in the Rust registry: a ShimFixtureDef either carries a `cached_result`
(an injected target_fixture value) or a `registry_name` aliasing a real
collected fixture (a matched bdd step fixture)."""


def _visible(baseid, nodeid):
    """A fixture with `baseid` is visible to `nodeid` when the node lives at
    or under the baseid prefix — mirrors the Rust registry's lookup rule."""
    if not baseid:
        return True
    return nodeid.startswith(baseid)


class ShimFixtureDef:
    """Stand-in for _pytest.fixtures.FixtureDef carrying only the attributes
    pytest-bdd reads (func/baseid/argname) plus the bridge back to the Rust
    registry (registry_name) or an injected value (cached_result, set by the
    caller after construction)."""

    def __init__(self, argname, func, baseid="", scope="function", params=None, registry_name=None):
        self.argname = argname
        self.func = func
        self.baseid = baseid
        self.scope = scope
        self.params = params
        self.unittest = False
        # The name to resolve in the Rust registry; None for purely injected
        # defs (target_fixture values), which instead get a cached_result.
        self.registry_name = registry_name

    def __repr__(self):
        return f"<ShimFixtureDef argname={self.argname!r} baseid={self.baseid!r}>"


class ShimFixtureManager:
    def __init__(self, arg2fixturedefs):
        self._arg2fixturedefs = arg2fixturedefs

    def getfixturedefs(self, argname, node):
        defs = self._arg2fixturedefs.get(argname)
        if not defs:
            return None
        nodeid = node if isinstance(node, str) else getattr(node, "nodeid", "")
        visible = tuple(d for d in defs if _visible(d.baseid, nodeid))
        return visible or None

    def _register_fixture(self, *, name, func, nodeid="", scope="function", params=None, **_):
        """pytest >= 8.1 dynamic registration (pytest-bdd inject_fixture). The
        caller sets `.cached_result` on the returned def to pin the value."""
        fixture_def = ShimFixtureDef(argname=name, func=func, baseid=nodeid or "", scope=scope, params=params)
        self._arg2fixturedefs.setdefault(name, []).append(fixture_def)
        return fixture_def


def build_manager(entries):
    """Construct a ShimFixtureManager from (name, func, baseid, scope) tuples
    enumerated from the Rust fixture registry."""
    arg2fixturedefs = {}
    for name, func, baseid, scope in entries:
        arg2fixturedefs.setdefault(name, []).append(
            ShimFixtureDef(argname=name, func=func, baseid=baseid, scope=scope, registry_name=name)
        )
    return ShimFixtureManager(arg2fixturedefs)
