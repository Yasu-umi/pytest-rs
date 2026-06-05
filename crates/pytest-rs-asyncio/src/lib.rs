//! pytest-asyncio equivalent: owns the event loop lifecycle and the
//! execution of async tests and async (generator) fixtures.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CString;

use pytest_rs_core::collect::TestItem;
use pytest_rs_core::config::{OptDef, OptionParser};
use pytest_rs_core::fixture::{FixtureDef, Scope};
use pytest_rs_core::hooks::{FixtureValue, HookContext, HookResult, Plugin};
use pytest_rs_core::pyo3::exceptions::PyRuntimeError;
use pytest_rs_core::pyo3::prelude::*;
use pytest_rs_core::pyo3::types::PyModule;
use pytest_rs_core::session::Finalizer;

const HELPER: &str = include_str!("../py/helper.py");
const PYTEST_ASYNCIO_SHIM: &str = include_str!("../py/pytest_asyncio_shim.py");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Auto,
    Strict,
}

pub struct AsyncioPlugin {
    mode: Mode,
    helper: Option<Py<PyModule>>,
    /// Event loops cached per (loop scope, scope instance key).
    loops: RefCell<HashMap<(Scope, String), Py<PyAny>>>,
    current_module: RefCell<Option<String>>,
    /// asyncio_default_fixture_loop_scope / asyncio_default_test_loop_scope.
    default_fixture_loop_scope: Option<Scope>,
    default_test_loop_scope: Option<Scope>,
}

impl AsyncioPlugin {
    pub fn new() -> Self {
        Self {
            mode: Mode::Strict,
            helper: None,
            loops: RefCell::new(HashMap::new()),
            current_module: RefCell::new(None),
            default_fixture_loop_scope: None,
            default_test_loop_scope: None,
        }
    }

    fn helper<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyModule>> {
        self.helper
            .as_ref()
            .map(|m| m.bind(py).clone())
            .ok_or_else(|| PyRuntimeError::new_err("asyncio plugin not configured"))
    }

    /// The cached (or new) loop for a scope instance. A factory from the
    /// pytest_asyncio_loop_factories conftest hook customizes creation,
    /// else a user-defined event_loop_policy fixture does.
    fn loop_for(
        &self,
        py: Python<'_>,
        scope: Scope,
        key: &str,
        factory: Option<&Py<PyAny>>,
        policy: Option<&Py<PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let mut loops = self.loops.borrow_mut();
        if let Some(loop_) = loops.get(&(scope, key.to_string())) {
            return Ok(loop_.clone_ref(py));
        }
        let helper = self.helper(py)?;
        let loop_ = match (factory, policy) {
            (Some(factory), _) => helper
                .getattr("new_loop_with_factory")?
                .call1((factory.bind(py),))?
                .unbind(),
            (None, Some(policy)) => helper
                .getattr("new_loop_with_policy")?
                .call1((policy.bind(py),))?
                .unbind(),
            (None, None) => helper.getattr("new_loop")?.call0()?.unbind(),
        };
        loops.insert((scope, key.to_string()), loop_.clone_ref(py));
        Ok(loop_)
    }

    /// The user's event_loop_policy fixture value, if one is defined
    /// (resolved directly: policy fixtures are plain zero-dependency
    /// factories in practice).
    fn loop_policy(&self, ctx: &mut HookContext, item: &TestItem) -> Option<Py<PyAny>> {
        let def = ctx
            .session
            .registry
            .lookup("event_loop_policy", &item.nodeid)?;
        if !def.param_names.is_empty() {
            return None;
        }
        pytest_rs_core::python::call_fixture(ctx.py, &def.func, None, &[])
            .ok()
            .map(|value| value.unbind())
    }

    /// The loop factory for an item from conftest
    /// pytest_asyncio_loop_factories hooks (first factory wins; named
    /// multi-factory parametrization is not supported yet).
    fn loop_factory(&self, ctx: &mut HookContext, item: &TestItem) -> PyResult<Option<Py<PyAny>>> {
        let py = ctx.py;
        let hook_funcs: Vec<Py<PyAny>> = ctx
            .session
            .py_hooks
            .iter()
            .filter(|hook| {
                hook.name == "pytest_asyncio_loop_factories"
                    && item.nodeid.starts_with(&hook.baseid)
            })
            .map(|hook| hook.func.clone_ref(py))
            .collect();
        if hook_funcs.is_empty() {
            return Ok(None);
        }
        let config = pytest_rs_core::python::make_py_config(py, ctx.config)?;
        let node = pytest_rs_core::python::make_node(py, item)?;
        for func in &hook_funcs {
            let result = pytest_rs_core::python::call_py_hook(
                py,
                func,
                &[
                    ("config", config.clone_ref(py)),
                    ("item", node.clone_ref(py)),
                ],
            )?;
            let result = result.bind(py);
            if result.is_none() {
                continue;
            }
            let values = result.call_method0("values")?;
            if let Some(factory) = values.try_iter()?.next() {
                return Ok(Some(factory?.unbind()));
            }
        }
        Ok(None)
    }

    fn close_loop(&self, py: Python<'_>, loop_: &Py<PyAny>) -> PyResult<()> {
        self.helper(py)?
            .getattr("close_loop")?
            .call1((loop_.bind(py),))?;
        Ok(())
    }

    fn scope_key(scope: Scope, item: &TestItem) -> String {
        match scope {
            Scope::Function => item.nodeid.clone(),
            Scope::Class => item
                .nodeid
                .rsplit_once("::")
                .map(|(prefix, _)| prefix.to_string())
                .unwrap_or_else(|| item.module_instance()),
            Scope::Module | Scope::Package => item.module_instance(),
            Scope::Session => String::new(),
        }
    }

    /// The loop scope of a test item: marker kwarg, else the configured
    /// default, else function.
    fn test_loop_scope(&self, py: Python<'_>, item: &TestItem) -> Scope {
        let from_marker = item.get_closest_marker("asyncio").and_then(|mark| {
            mark.obj
                .bind(py)
                .getattr("kwargs")
                .ok()
                .and_then(|kwargs| kwargs.get_item("loop_scope").ok())
                .and_then(|scope| scope.extract::<String>().ok())
        });
        from_marker
            .and_then(|name| Scope::parse(&name))
            .or(self.default_test_loop_scope)
            .unwrap_or(Scope::Function)
    }

    /// The loop scope of an async fixture: explicit loop_scope recorded by
    /// pytest_asyncio.fixture, else the configured default, else the
    /// fixture's own scope.
    fn fixture_loop_scope(&self, py: Python<'_>, def: &FixtureDef) -> Scope {
        def.func
            .bind(py)
            .getattr("_pytest_asyncio_loop_scope")
            .ok()
            .and_then(|scope| scope.extract::<String>().ok())
            .and_then(|name| Scope::parse(&name))
            .or(self.default_fixture_loop_scope)
            .unwrap_or(def.scope)
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
            None,
            "asyncio mode: auto or strict",
        ));
    }

    fn pytest_configure(&mut self, ctx: &mut HookContext) -> PyResult<()> {
        let mode_value = ctx
            .config
            .get_value("--asyncio-mode")
            .or_else(|| ctx.config.get_ini("asyncio_mode"));
        self.mode = match mode_value {
            None | Some("strict") => Mode::Strict,
            Some("auto") => Mode::Auto,
            Some(other) => {
                return Err(pytest_rs_core::python::usage_error(
                    ctx.py,
                    &format!("'{other}' is not a valid asyncio_mode. Valid modes: auto, strict."),
                ));
            }
        };
        for (ini_key, slot) in [
            (
                "asyncio_default_fixture_loop_scope",
                &mut self.default_fixture_loop_scope,
            ),
            (
                "asyncio_default_test_loop_scope",
                &mut self.default_test_loop_scope,
            ),
        ] {
            if let Some(value) = ctx.config.get_ini(ini_key) {
                match Scope::parse(value) {
                    Some(scope) => *slot = Some(scope),
                    None => {
                        return Err(pytest_rs_core::python::usage_error(
                            ctx.py,
                            &format!(
                                "'{value}' is not a valid {ini_key}. \
                                 Valid scopes: function, class, module, package, session."
                            ),
                        ));
                    }
                }
            }
        }

        let module = PyModule::from_code(
            ctx.py,
            CString::new(HELPER)?.as_c_str(),
            c"pytest_rs_asyncio/helper.py",
            c"_pytest_rs_asyncio",
        )?;
        self.helper = Some(module.unbind());

        // `import pytest_asyncio` in upstream suites resolves to our shim.
        let shim = PyModule::from_code(
            ctx.py,
            CString::new(PYTEST_ASYNCIO_SHIM)?.as_c_str(),
            c"pytest_rs_asyncio/pytest_asyncio_shim.py",
            c"pytest_asyncio",
        )?;
        ctx.py
            .import("sys")?
            .getattr("modules")?
            .set_item("pytest_asyncio", shim)?;
        Ok(())
    }

    fn pytest_runtest_setup(&self, ctx: &mut HookContext, item: &TestItem) -> PyResult<()> {
        // mark.asyncio takes keyword arguments only.
        if let Some(mark) = item.get_closest_marker("asyncio")
            && let Ok(args) = mark.obj.bind(ctx.py).getattr("args")
            && args.len().unwrap_or(0) > 0
        {
            return Err(pytest_rs_core::pyo3::exceptions::PyValueError::new_err(
                "mark.asyncio accepts only keyword arguments",
            ));
        }

        // Close module/class-scoped loops from a previous module.
        let module = item.module_instance();
        let mut current = self.current_module.borrow_mut();
        if current.as_ref() != Some(&module) {
            if let Some(prev) = current.as_ref() {
                let stale: Vec<_> = self
                    .loops
                    .borrow()
                    .keys()
                    .filter(|(scope, key)| {
                        matches!(scope, Scope::Module | Scope::Package | Scope::Class)
                            && (key == prev || key.starts_with(&format!("{prev}::")))
                    })
                    .cloned()
                    .collect();
                for entry in stale {
                    if let Some(loop_) = self.loops.borrow_mut().remove(&entry) {
                        self.close_loop(ctx.py, &loop_)?;
                    }
                }
            }
            *current = Some(module);
        }
        Ok(())
    }

    fn pytest_fixture_setup(
        &self,
        ctx: &mut HookContext,
        def: &FixtureDef,
        item: &TestItem,
        instance: Option<&Py<PyAny>>,
        kwargs: &[(String, Py<PyAny>)],
    ) -> HookResult<FixtureValue> {
        if !def.is_coroutine && !def.is_async_gen {
            return Ok(None);
        }
        let factory = self.loop_factory(ctx, item)?;
        let policy = self.loop_policy(ctx, item);
        let py = ctx.py;
        let helper = self.helper(py)?;
        let scope = self.fixture_loop_scope(py, def);
        let loop_ = self.loop_for(
            py,
            scope,
            &Self::scope_key(scope, item),
            factory.as_ref(),
            policy.as_ref(),
        )?;

        if def.is_coroutine {
            let coro = pytest_rs_core::python::call_fixture(py, &def.func, instance, kwargs)?;
            let value = helper.getattr("run")?.call1((loop_.bind(py), coro))?;
            return Ok(Some(FixtureValue {
                value: value.unbind(),
                finalizer: None,
            }));
        }

        // async generator fixture
        let agen = pytest_rs_core::python::call_fixture(py, &def.func, instance, kwargs)?;
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
        callable: &Py<PyAny>,
        kwargs: &[(String, Py<PyAny>)],
    ) -> HookResult<()> {
        if !item.is_coroutine || !self.applicable(item) {
            return Ok(None);
        }
        let factory = self.loop_factory(ctx, item)?;
        let policy = self.loop_policy(ctx, item);
        let py = ctx.py;
        let helper = self.helper(py)?;
        let scope = self.test_loop_scope(py, item);
        let loop_ = self.loop_for(
            py,
            scope,
            &Self::scope_key(scope, item),
            factory.as_ref(),
            policy.as_ref(),
        )?;
        let coro = pytest_rs_core::python::call_with_kwargs(py, callable, kwargs)?;
        helper.getattr("run")?.call1((loop_.bind(py), coro))?;
        Ok(Some(()))
    }

    fn pytest_runtest_teardown(&self, ctx: &mut HookContext, item: &TestItem) -> PyResult<()> {
        // Function-scoped loops die with their item.
        let entry = (Scope::Function, item.nodeid.clone());
        if let Some(loop_) = self.loops.borrow_mut().remove(&entry) {
            self.close_loop(ctx.py, &loop_)?;
        }
        Ok(())
    }

    fn pytest_sessionfinish(&mut self, ctx: &mut HookContext, _exit_code: i32) -> PyResult<()> {
        let remaining: Vec<_> = self.loops.borrow_mut().drain().map(|(_, l)| l).collect();
        for loop_ in remaining {
            self.close_loop(ctx.py, &loop_)?;
        }
        Ok(())
    }
}
