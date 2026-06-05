use pyo3::prelude::*;

use crate::collect::TestItem;
use crate::config::{Config, OptionParser};
use crate::fixture::FixtureDef;
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

    fn pytest_sessionstart(&mut self, _ctx: &mut HookContext) -> PyResult<()> {
        Ok(())
    }

    fn pytest_collection_modifyitems(
        &self,
        _ctx: &mut HookContext,
        _items: &mut Vec<TestItem>,
    ) -> PyResult<()> {
        Ok(())
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
