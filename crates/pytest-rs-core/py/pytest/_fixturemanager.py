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
collected fixture (a matched bdd step fixture).

ShimFixtureDef instances are also handed to *any* conftest/plugin
`pytest_fixture_setup`/`pytest_fixture_post_finalizer` hookimpl as the
`fixturedef` argument (see fire_fixture_lifecycle_hooks in the Rust engine) —
not just pytest-bdd's own code — so it must carry the same commonly-read
attributes as upstream's real `_pytest.fixtures.FixtureDef`, e.g. `argnames`
(aiohttp's own pytest plugin reads `fixturedef.argnames`)."""

import inspect


def _func_argnames(func):
    try:
        return tuple(inspect.signature(func).parameters)
    except (TypeError, ValueError):
        return ()


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

    def __init__(
        self,
        argname,
        func,
        baseid="",
        scope="function",
        params=None,
        registry_name=None,
        argnames=None,
    ):
        self.argname = argname
        self.func = func
        self.baseid = baseid
        self.scope = scope
        self.params = params
        self.argnames = tuple(argnames) if argnames is not None else _func_argnames(func)
        self.unittest = False
        # The name to resolve in the Rust registry; None for purely injected
        # defs (target_fixture values), which instead get a cached_result.
        self.registry_name = registry_name
        self._cached_result = None
        self._has_cached_result = False
        # Set by PyRequest._get_active_fixturedef on the def it returns, so
        # the cached_result setter below can forward an injected value
        # (pytest-bdd's target_fixture) to the Rust-native resolver's
        # per-item override map. Without this, a *sibling* fixture that
        # merely depends on the injected name (rather than resolving it via
        # request.getfixturevalue on this same request) never sees the
        # override — each FixtureRequest gets its own throwaway
        # ShimFixtureManager, but native fixture-dependency resolution
        # bypasses it entirely.
        self.owner = None

    @property
    def cached_result(self):
        return self._cached_result

    @cached_result.setter
    def cached_result(self, value):
        self._cached_result = value
        self._has_cached_result = True
        if self.owner is not None and value is not None:
            self.owner._set_injected_fixture(self.argname, value[0])

    def __repr__(self):
        return f"<ShimFixtureDef argname={self.argname!r} baseid={self.baseid!r}>"


class ShimFixtureManager:
    def __init__(self, arg2fixturedefs, autousenames):
        self._arg2fixturedefs = arg2fixturedefs
        self._autousenames = autousenames

    def getfixturedefs(self, argname, node):
        defs = self._arg2fixturedefs.get(argname)
        if not defs:
            return None
        nodeid = node if isinstance(node, str) else getattr(node, "nodeid", "")
        visible = tuple(d for d in defs if _visible(d.baseid, nodeid))
        return visible or None

    def _getautousenames(self, node):
        nodeid = getattr(node, "nodeid", "")
        for baseid, names in self._autousenames:
            if _visible(baseid, nodeid):
                yield from names

    def _register_fixture(self, *, name, func, nodeid="", scope="function", params=None, **_):
        fixture_def = ShimFixtureDef(
            argname=name, func=func, baseid=nodeid or "", scope=scope, params=params
        )
        self._arg2fixturedefs.setdefault(name, []).append(fixture_def)
        return fixture_def


def build_manager(entries):
    arg2fixturedefs = {}
    autousenames = {}
    for name, func, baseid, scope, autouse in entries:
        arg2fixturedefs.setdefault(name, []).append(
            ShimFixtureDef(argname=name, func=func, baseid=baseid, scope=scope, registry_name=name)
        )
        if autouse:
            autousenames.setdefault(baseid, []).append(name)
    return ShimFixtureManager(arg2fixturedefs, tuple(autousenames.items()))
