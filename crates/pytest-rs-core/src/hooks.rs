use pyo3::prelude::*;

use crate::collect::{MarkData, TestItem};
use crate::config::{Config, OptionParser};
use crate::fixture::{FixtureDef, FixtureRegistry};
use crate::session::{Finalizer, Session};

/// firstresult hooks return `Ok(Some(_))` to claim the call; `Ok(None)` to
/// pass to the next plugin / the core default.
pub type HookResult<T> = PyResult<Option<T>>;

/// Everything a hook may touch, borrowed for the duration of one call.
/// `'py` is the GIL lifetime and must never be stored in plugin fields —
/// plugins keep GIL-independent `Py<PyAny>` handles instead.
pub struct HookContext<'py, 's> {
    pub py: Python<'py>,
    pub session: &'s mut Session,
    pub config: &'s Config,
}

/// The result of providing a fixture value from a plugin.
pub struct FixtureValue {
    pub value: Py<PyAny>,
    pub finalizer: Option<Finalizer>,
}

/// A pluggy-like plugin: implement only the hooks you need.
pub trait Plugin: Send {
    /// Stable name, used for ordering and dependency resolution.
    fn name(&self) -> &str;

    /// Names of plugins that must be registered before this one.
    fn depends_on(&self) -> &[&str] {
        &[]
    }

    fn pytest_addoption(&self, _parser: &mut OptionParser) {}

    fn pytest_configure(&mut self, _ctx: &mut HookContext) -> PyResult<()> {
        Ok(())
    }

    /// A forked xdist worker (see `worker.rs::run_worker_forked`) never
    /// re-fires `pytest_configure` for native plugins — it inherits
    /// whichever plugin state the controller's single, pre-fork call
    /// already set up, via `fork()`'s copy-on-write. That's correct for
    /// state that doesn't need to differ per-process, but wrong for
    /// anything keyed on the controller's own pid (e.g. a temp directory
    /// name): every forked sibling would otherwise share that one
    /// controller-owned resource and race on it. Called once per forked
    /// worker, right after fork, alongside the capture/reporter
    /// `reinit_post_fork` calls — a no-op for plugins with nothing
    /// pid-specific to redo.
    fn reinit_post_fork(&mut self, _py: Python<'_>) {}

    /// Fires once `-p NAME` / entry-point plugins have been imported (their
    /// module-level code has already run), before conftest discovery and
    /// test collection. Upstream pytest imports `-p`/entry-point plugins
    /// during early config parsing, well before any `pytest_configure`; a
    /// plugin that needs to distinguish "already loaded" from "not yet
    /// collected" code (e.g. pytest-cov's coverage-start point, which in
    /// real pytest-cov is `pytest_load_initial_conftests`, itself after
    /// `-p` loading) should act here rather than in `pytest_configure`,
    /// which native plugins run earlier, before `-p` plugins even import.
    fn pytest_plugins_registered(&mut self, _ctx: &mut HookContext) -> PyResult<()> {
        Ok(())
    }

    fn pytest_sessionstart(&mut self, _ctx: &mut HookContext) -> PyResult<()> {
        Ok(())
    }

    /// Runs after collection but before parametrized-fixture expansion:
    /// marks added here (e.g. usefixtures) participate in the fixture
    /// closure and its param axes, like upstream pytest_pycollect_makeitem
    /// mark injection.
    fn pytest_collection_preexpand(
        &self,
        _ctx: &mut HookContext,
        _items: &mut Vec<TestItem>,
    ) -> PyResult<()> {
        Ok(())
    }

    fn pytest_collection_modifyitems(
        &self,
        _ctx: &mut HookContext,
        _items: &mut Vec<TestItem>,
    ) -> PyResult<()> {
        Ok(())
    }

    /// Extra fixture names to treat as requested by one test function for
    /// parametrize-argname validation, for a plugin that connects its
    /// fixture to matching items only in `pytest_collection_preexpand`/
    /// `pytest_collection_modifyitems` (e.g. anyio's `anyio_backend`) — by
    /// which point this file's items have already been validated. Called
    /// once per test function during that file's collection, so it must
    /// decide from per-function state alone (marks, is_coroutine, the
    /// fixture registry), not full-session state.
    fn pytest_collect_implied_fixtures(
        &self,
        _py: Python<'_>,
        _is_coroutine: bool,
        _func: &Bound<'_, PyAny>,
        _marks: &[MarkData],
        _registry: &FixtureRegistry,
        _nodeid: &str,
    ) -> PyResult<Vec<String>> {
        Ok(Vec::new())
    }

    /// firstresult: provide the value (and optional finalizer) for a fixture
    /// the core cannot set up itself (async fixtures, native fixtures).
    /// `instance` is the test class instance for class-defined fixtures.
    fn pytest_fixture_setup(
        &self,
        _ctx: &mut HookContext,
        _def: &FixtureDef,
        _item: &TestItem,
        _instance: Option<&Py<PyAny>>,
        _kwargs: &[(String, Py<PyAny>)],
    ) -> HookResult<FixtureValue> {
        Ok(None)
    }

    /// firstresult: an extra discriminator appended to a fixture's cache key
    /// (e.g. the asyncio loop-factory variant, so loop-bound fixtures are
    /// recreated per variant instead of shared across them).
    fn pytest_fixture_cache_key(
        &self,
        _ctx: &mut HookContext,
        _def: &FixtureDef,
        _item: &TestItem,
    ) -> HookResult<String> {
        Ok(None)
    }

    fn pytest_runtest_setup(&self, _ctx: &mut HookContext, _item: &TestItem) -> PyResult<()> {
        Ok(())
    }

    /// firstresult: actually invoke the test function. Return `Some(())`
    /// after running it (exceptions propagate as Err). The core's default
    /// sync caller runs if no plugin claims the item. `callable` is already
    /// bound to the test class instance for methods.
    fn pytest_pyfunc_call(
        &self,
        _ctx: &mut HookContext,
        _item: &TestItem,
        _callable: &Py<PyAny>,
        _kwargs: &[(String, Py<PyAny>)],
    ) -> HookResult<()> {
        Ok(None)
    }

    fn pytest_runtest_teardown(&self, _ctx: &mut HookContext, _item: &TestItem) -> PyResult<()> {
        Ok(())
    }

    fn pytest_terminal_summary(&self, _ctx: &mut HookContext, _out: &mut String) -> PyResult<()> {
        Ok(())
    }

    /// Lines printed under the session header (upstream's
    /// `pytest_report_header` hookspec) — e.g. pytest-benchmark's
    /// "benchmark: X.Y.Z (defaults: ...)" line.
    fn pytest_report_header(&self, _ctx: &mut HookContext) -> PyResult<Vec<String>> {
        Ok(Vec::new())
    }

    /// A fully-rendered `--help` section this plugin's own CLI options
    /// belong under (upstream's `parser.getgroup("name")`, e.g.
    /// pytest-benchmark's own `"benchmark:"` heading) — `None` when the
    /// plugin has no dedicated option group to show. Plugins own the exact
    /// rendering (metavar/`=VALUE` syntax/wrapping) themselves; core only
    /// prints whatever text comes back, right after the core option listing.
    fn pytest_help_group(&self, _ctx: &mut HookContext) -> PyResult<Option<String>> {
        Ok(None)
    }

    fn pytest_sessionfinish(&mut self, _ctx: &mut HookContext, _exit_code: i32) -> PyResult<()> {
        Ok(())
    }

    /// Worker mode (-n): serialize per-process state for the parent to
    /// merge (cov hits, benchmark results). Called after sessionfinish.
    fn pytest_worker_dump(&mut self, _ctx: &mut HookContext) -> PyResult<Option<String>> {
        Ok(None)
    }

    /// Parent side of `pytest_worker_dump`: merge one worker's payload
    /// (matched by plugin name). Called before sessionfinish.
    fn pytest_worker_load(&mut self, _ctx: &mut HookContext, _payload: &str) -> PyResult<()> {
        Ok(())
    }
}
