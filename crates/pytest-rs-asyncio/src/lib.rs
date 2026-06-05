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

/// Named loop factories from a pytest_asyncio_loop_factories hook.
type NamedFactories = Vec<(String, Py<PyAny>)>;

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

    /// The loop factory for an item: the one recorded by multi-factory
    /// expansion at collection time, else the first factory from conftest
    /// pytest_asyncio_loop_factories hooks.
    fn loop_factory(&self, ctx: &mut HookContext, item: &TestItem) -> PyResult<Option<Py<PyAny>>> {
        let py = ctx.py;
        if let Some(mark) = item.get_closest_marker("_asyncio_loop_factory") {
            // obj is a (name, factory) tuple stashed by modifyitems.
            return Ok(Some(mark.obj.bind(py).get_item(1)?.unbind()));
        }
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

    /// All named factories from conftest hooks, in declaration order.
    fn loop_factories_map(
        &self,
        ctx: &mut HookContext,
        item: &TestItem,
    ) -> PyResult<Option<NamedFactories>> {
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
            let mut factories = Vec::new();
            for key in result.call_method0("keys")?.try_iter()? {
                let key = key?;
                let factory = result.get_item(&key)?;
                factories.push((key.extract::<String>()?, factory.unbind()));
            }
            return Ok(Some(factories));
        }
        Ok(None)
    }

    fn close_loop(&self, py: Python<'_>, loop_: &Py<PyAny>) -> PyResult<()> {
        self.helper(py)?
            .getattr("close_loop")?
            .call1((loop_.bind(py),))?;
        Ok(())
    }

    fn scope_key(py: Python<'_>, scope: Scope, item: &TestItem) -> String {
        let mut key = match scope {
            Scope::Function => item.nodeid.clone(),
            Scope::Class => item
                .nodeid
                .rsplit_once("::")
                .map(|(prefix, _)| prefix.to_string())
                .unwrap_or_else(|| item.module_instance()),
            Scope::Module | Scope::Package => item.module_instance(),
            Scope::Session => String::new(),
        };
        // Items parametrized over loop factories never share loops.
        if let Some(mark) = item.get_closest_marker("_asyncio_loop_factory")
            && let Ok(name) = mark
                .obj
                .bind(py)
                .get_item(0)
                .and_then(|name| name.extract::<String>())
        {
            key.push('#');
            key.push_str(&name);
        }
        key
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

    fn pytest_collection_modifyitems(
        &self,
        ctx: &mut HookContext,
        items: &mut Vec<TestItem>,
    ) -> PyResult<()> {
        let py = ctx.py;
        let taken = std::mem::take(items);
        for item in taken {
            if !item.is_coroutine || !self.applicable(&item) {
                items.push(item);
                continue;
            }
            let factories = self.loop_factories_map(ctx, &item)?;
            // A single factory keeps the plain test name (HIDDEN_PARAM
            // upstream); several parametrize the item.
            let Some(factories) = factories.filter(|factories| factories.len() > 1) else {
                items.push(item);
                continue;
            };
            for (name, factory) in factories {
                let nodeid = if item.nodeid.ends_with(']') {
                    format!("{}-{name}]", &item.nodeid[..item.nodeid.len() - 1])
                } else {
                    format!("{}[{name}]", item.nodeid)
                };
                let mut marks: Vec<pytest_rs_core::collect::MarkData> = item
                    .marks
                    .iter()
                    .map(|mark| pytest_rs_core::collect::MarkData {
                        name: mark.name.clone(),
                        obj: mark.obj.clone_ref(py),
                    })
                    .collect();
                let pair = pytest_rs_core::pyo3::types::PyTuple::new(
                    py,
                    [name.into_pyobject(py)?.into_any(), factory.bind(py).clone()],
                )?;
                marks.push(pytest_rs_core::collect::MarkData {
                    name: "_asyncio_loop_factory".to_string(),
                    obj: pair.into_any().unbind(),
                });
                items.push(TestItem {
                    nodeid,
                    path: item.path.clone(),
                    module_name: item.module_name.clone(),
                    func_name: item.func_name.clone(),
                    func: item.func.clone_ref(py),
                    cls: item.cls.as_ref().map(|cls| cls.clone_ref(py)),
                    is_coroutine: item.is_coroutine,
                    fixture_names: item.fixture_names.clone(),
                    marks,
                    callspec: item
                        .callspec
                        .iter()
                        .map(|(name, value)| (name.clone(), value.clone_ref(py)))
                        .collect(),
                    fixture_params: item
                        .fixture_params
                        .iter()
                        .map(|(name, index, value)| (name.clone(), *index, value.clone_ref(py)))
                        .collect(),
                    lineno: item.lineno,
                });
            }
        }
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
            // A sync fixture with an explicit loop_scope still expects the
            // scope's loop installed as current; set it and let the core
            // call the fixture normally.
            let has_loop_scope = def
                .func
                .bind(ctx.py)
                .hasattr("_pytest_asyncio_loop_scope")
                .unwrap_or(false);
            if has_loop_scope {
                let factory = self.loop_factory(ctx, item)?;
                let policy = self.loop_policy(ctx, item);
                let py = ctx.py;
                let scope = self.fixture_loop_scope(py, def);
                let loop_ = self.loop_for(
                    py,
                    scope,
                    &Self::scope_key(py, scope, item),
                    factory.as_ref(),
                    policy.as_ref(),
                )?;
                self.helper(py)?
                    .getattr("set_current_loop")?
                    .call1((loop_.bind(py),))?;
            }
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
            &Self::scope_key(py, scope, item),
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
            &Self::scope_key(py, scope, item),
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
