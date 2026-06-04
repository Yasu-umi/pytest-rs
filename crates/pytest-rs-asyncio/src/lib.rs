//! pytest-asyncio equivalent: owns the event loop lifecycle and the
//! execution of async tests and async (generator) fixtures.

use std::cell::RefCell;
use std::ffi::CString;

use pytest_rs_core::collect::TestItem;
use pytest_rs_core::config::{OptDef, OptionParser};
use pytest_rs_core::fixture::FixtureDef;
use pytest_rs_core::hooks::{FixtureValue, HookContext, HookResult, Plugin};
use pytest_rs_core::pyo3::prelude::*;
use pytest_rs_core::pyo3::types::PyModule;
use pytest_rs_core::session::Finalizer;

const HELPER: &str = include_str!("../py/helper.py");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Auto,
    Strict,
}

pub struct AsyncioPlugin {
    mode: Mode,
    helper: Option<Py<PyModule>>,
    /// The function-scoped loop for the item currently running.
    current_loop: RefCell<Option<Py<PyAny>>>,
}

impl AsyncioPlugin {
    pub fn new() -> Self {
        Self {
            mode: Mode::Strict,
            helper: None,
            current_loop: RefCell::new(None),
        }
    }

    fn helper<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyModule>> {
        self.helper
            .as_ref()
            .map(|m| m.bind(py).clone())
            .ok_or_else(|| {
                pytest_rs_core::pyo3::exceptions::PyRuntimeError::new_err(
                    "asyncio plugin not configured",
                )
            })
    }

    /// The loop for the current item, created lazily.
    fn ensure_loop(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let mut slot = self.current_loop.borrow_mut();
        if let Some(loop_) = slot.as_ref() {
            return Ok(loop_.clone_ref(py));
        }
        let loop_ = self.helper(py)?.getattr("new_loop")?.call0()?.unbind();
        *slot = Some(loop_.clone_ref(py));
        Ok(loop_)
    }

    fn applicable(&self, item: &TestItem) -> bool {
        match self.mode {
            Mode::Auto => true,
            Mode::Strict => item.get_closest_marker("asyncio").is_some(),
        }
    }
}

impl Default for AsyncioPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for AsyncioPlugin {
    fn name(&self) -> &str {
        "asyncio"
    }

    fn pytest_addoption(&self, parser: &mut OptionParser) {
        parser.add_option(OptDef::value(
            "--asyncio-mode",
            Some("strict"),
            "asyncio mode: auto or strict",
        ));
    }

    fn pytest_configure(&mut self, ctx: &mut HookContext) -> PyResult<()> {
        self.mode = match ctx.config.get_value("--asyncio-mode") {
            Some("auto") => Mode::Auto,
            _ => Mode::Strict,
        };
        let module = PyModule::from_code(
            ctx.py,
            CString::new(HELPER)?.as_c_str(),
            c"pytest_rs_asyncio/helper.py",
            c"_pytest_rs_asyncio",
        )?;
        self.helper = Some(module.unbind());
        Ok(())
    }

    fn pytest_fixture_setup(
        &self,
        ctx: &mut HookContext,
        def: &FixtureDef,
        _item: &TestItem,
        kwargs: &[(String, Py<PyAny>)],
    ) -> HookResult<FixtureValue> {
        if !def.is_coroutine && !def.is_async_gen {
            return Ok(None);
        }
        let py = ctx.py;
        let helper = self.helper(py)?;
        let loop_ = self.ensure_loop(py)?;

        if def.is_coroutine {
            let coro = pytest_rs_core::python::call_with_kwargs(py, &def.func, kwargs)?;
            let value = helper.getattr("run")?.call1((loop_.bind(py), coro))?;
            return Ok(Some(FixtureValue {
                value: value.unbind(),
                finalizer: None,
            }));
        }

        // async generator fixture
        let agen = pytest_rs_core::python::call_with_kwargs(py, &def.func, kwargs)?;
        let value = helper
            .getattr("async_gen_first")?
            .call1((loop_.bind(py), &agen))?;
        let finalizer = helper
            .getattr("async_gen_finalizer")?
            .call1((loop_.bind(py), &agen))?;
        Ok(Some(FixtureValue {
            value: value.unbind(),
            finalizer: Some(Finalizer::Callable(finalizer.unbind())),
        }))
    }

    fn pytest_pyfunc_call(
        &self,
        ctx: &mut HookContext,
        item: &TestItem,
        kwargs: &[(String, Py<PyAny>)],
    ) -> HookResult<()> {
        if !item.is_coroutine || !self.applicable(item) {
            return Ok(None);
        }
        let py = ctx.py;
        let helper = self.helper(py)?;
        let loop_ = self.ensure_loop(py)?;
        let coro = pytest_rs_core::python::call_with_kwargs(py, &item.func, kwargs)?;
        helper.getattr("run")?.call1((loop_.bind(py), coro))?;
        Ok(Some(()))
    }

    fn pytest_runtest_teardown(&self, ctx: &mut HookContext, _item: &TestItem) -> PyResult<()> {
        if let Some(loop_) = self.current_loop.borrow_mut().take() {
            self.helper(ctx.py)?
                .getattr("close_loop")?
                .call1((loop_.bind(ctx.py),))?;
        }
        Ok(())
    }
}
