//! pytest-asyncio equivalent: owns the event loop lifecycle and the
//! execution of async tests and async (generator) fixtures.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CString;

use pytest_rs_core::collect::TestItem;
use pytest_rs_core::config::{OptDef, OptionParser};
use pytest_rs_core::fixture::{FixtureDef, Scope};
use pytest_rs_core::hooks::{FixtureValue, HookContext, HookResult, Plugin};
use pytest_rs_core::pyo3::exceptions::{PyOSError, PyRuntimeError};
use pytest_rs_core::pyo3::prelude::*;
use pytest_rs_core::pyo3::types::PyModule;
use pytest_rs_core::session::Finalizer;

/// Named loop factories from a pytest_asyncio_loop_factories hook.
type NamedFactories = Vec<(String, Py<PyAny>)>;

const HELPER: &str = include_str!("../py/helper.py");

/// The `pytest_asyncio` shim, written to the per-run shim dir as a real
/// package so upstream `import pytest_asyncio.plugin` resolves through
/// normal import machinery (and `_unused_port` is monkeypatchable).
const SHIM_FILES: &[(&str, &str)] = &[
    (
        "__init__.py",
        include_str!("../py/pytest_asyncio/__init__.py"),
    ),
    ("plugin.py", include_str!("../py/pytest_asyncio/plugin.py")),
];

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
    /// --asyncio-debug / asyncio_debug: new loops run in asyncio debug mode.
    debug: bool,
    /// event_loop_policy fixture values per scope instance (the override
    /// deprecation fires once per created value, like upstream).
    policies: RefCell<HashMap<String, Py<PyAny>>>,
}

/// The deprecation emitted when a user event_loop_policy fixture is used.
const POLICY_DEPRECATION: &str = "Overriding the \"event_loop_policy\" fixture is deprecated \
     and will be removed in a future version of pytest-asyncio. Use the \
     \"pytest_asyncio_loop_factories\" hook to customize event loop creation.";

impl AsyncioPlugin {
    pub fn new() -> Self {
        Self {
            mode: Mode::Strict,
            helper: None,
            loops: RefCell::new(HashMap::new()),
            current_module: RefCell::new(None),
            default_fixture_loop_scope: None,
            default_test_loop_scope: None,
            debug: false,
            policies: RefCell::new(HashMap::new()),
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
        if self.debug {
            loop_.bind(py).call_method1("set_debug", (true,))?;
        }
        loops.insert((scope, key.to_string()), loop_.clone_ref(py));
        Ok(loop_)
    }

    /// The user's event_loop_policy fixture value, if one is defined
    /// (resolved directly: policy fixtures are plain factories, optionally
    /// parametrized through `request.param`). Values are cached per scope
    /// instance; creation fires the override deprecation, like upstream.
    fn loop_policy(&self, ctx: &mut HookContext, item: &TestItem) -> Option<Py<PyAny>> {
        let def = ctx
            .session
            .registry
            .lookup("event_loop_policy", &item.nodeid)?;
        let py = ctx.py;
        let param = item
            .fixture_params
            .iter()
            .find(|(name, _, _)| name == "event_loop_policy")
            .map(|(_, index, value)| (*index, value.clone_ref(py)));
        let instance = match def.scope {
            Scope::Function => item.nodeid.clone(),
            Scope::Class => item.class_instance(),
            Scope::Module => item.module_instance(),
            // Shared by every module in the directory.
            Scope::Package => item
                .module_instance()
                .rsplit_once('/')
                .map(|(dir, _)| format!("{dir}/"))
                .unwrap_or_default(),
            Scope::Session => String::new(),
        };
        let cache_key = format!(
            "{:?}|{instance}|{:?}",
            def.scope,
            param.as_ref().map(|(index, _)| *index)
        );
        if let Some(cached) = self.policies.borrow().get(&cache_key) {
            return Some(cached.clone_ref(py));
        }

        let value = if let Some((_, param_value)) = &param {
            let node = pytest_rs_core::python::make_node(py, item).ok()?;
            let request = Py::new(
                py,
                pytest_rs_core::request::PyRequest::new(
                    Some(param_value.clone_ref(py)),
                    node,
                    Some("event_loop_policy".to_string()),
                ),
            )
            .ok()?;
            pytest_rs_core::python::call_fixture(
                py,
                &def.func,
                None,
                &[("request".to_string(), request.into_any())],
            )
            .ok()?
            .unbind()
        } else {
            if !def.param_names.is_empty() {
                return None;
            }
            // Class-defined policy fixtures get a throwaway instance as
            // `self`.
            let cls_instance: Option<Py<PyAny>> = if def.needs_instance {
                item.cls
                    .as_ref()
                    .and_then(|cls| cls.bind(py).call0().ok())
                    .map(|instance| instance.unbind())
            } else {
                None
            };
            pytest_rs_core::python::call_fixture(py, &def.func, cls_instance.as_ref(), &[])
                .ok()?
                .unbind()
        };
        let _ = Self::warn_pytest_deprecation(py, item, POLICY_DEPRECATION);
        self.policies
            .borrow_mut()
            .insert(cache_key, value.clone_ref(py));
        Some(value)
    }

    /// The factory recorded for this item at collection time, as a
    /// (name, factory) pair from the _asyncio_loop_factory pseudo-mark.
    fn item_factory(
        &self,
        py: Python<'_>,
        item: &TestItem,
    ) -> PyResult<Option<(String, Py<PyAny>)>> {
        let Some(mark) = item.get_closest_marker("_asyncio_loop_factory") else {
            return Ok(None);
        };
        let pair = mark.obj.bind(py);
        Ok(Some((
            pair.get_item(0)?.extract::<String>()?,
            pair.get_item(1)?.unbind(),
        )))
    }

    /// pytest_asyncio_loop_factories hook impls visible to an item, deepest
    /// conftest first (pluggy registers nested conftests last → LIFO call).
    fn hook_funcs(&self, ctx: &HookContext, item_nodeid: &str) -> Vec<(usize, Py<PyAny>)> {
        let py = ctx.py;
        let mut funcs: Vec<(usize, Py<PyAny>)> = ctx
            .session
            .py_hooks
            .iter()
            .filter(|hook| {
                hook.name == "pytest_asyncio_loop_factories"
                    && item_nodeid.starts_with(&hook.baseid)
            })
            .map(|hook| (hook.baseid.len(), hook.func.clone_ref(py)))
            .collect();
        funcs.sort_by_key(|func| std::cmp::Reverse(func.0));
        funcs
    }

    fn invalid_factories_error() -> pytest_rs_core::pyo3::PyErr {
        PyRuntimeError::new_err(
            "pytest_asyncio_loop_factories must return a non-empty mapping of \
             factory names to factory callables",
        )
    }

    /// First non-None hook result, validated: it must be a non-empty mapping
    /// of non-empty names to callables. Ok(None) when no hook impl applies;
    /// Err when impls exist but none produced a valid mapping.
    fn resolve_hook_factories(
        &self,
        ctx: &mut HookContext,
        item: &TestItem,
    ) -> PyResult<Option<NamedFactories>> {
        let py = ctx.py;
        let funcs = self.hook_funcs(ctx, &item.nodeid);
        if funcs.is_empty() {
            return Ok(None);
        }
        let config = pytest_rs_core::python::make_py_config(py, ctx.config)?;
        let node = pytest_rs_core::python::make_node(py, item)?;
        for (_, func) in &funcs {
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
            let Ok(keys) = result.call_method0("keys") else {
                return Err(Self::invalid_factories_error());
            };
            for key in keys.try_iter()? {
                let key = key?;
                let factory = result.get_item(&key)?;
                let name: String = key.extract().map_err(|_| Self::invalid_factories_error())?;
                if name.is_empty() || !factory.is_callable() {
                    return Err(Self::invalid_factories_error());
                }
                factories.push((name, factory.unbind()));
            }
            if factories.is_empty() {
                return Err(Self::invalid_factories_error());
            }
            return Ok(Some(factories));
        }
        Err(Self::invalid_factories_error())
    }

    /// The loop_factories kwarg of mark.asyncio: requested factory names.
    /// Err (a ValueError) when the value is not a non-empty sequence of
    /// non-empty factory names.
    fn marker_loop_factories(
        &self,
        py: Python<'_>,
        item: &TestItem,
    ) -> PyResult<Option<Vec<String>>> {
        let Some(mark) = item.get_closest_marker("asyncio") else {
            return Ok(None);
        };
        let Ok(kwargs) = mark.obj.bind(py).getattr("kwargs") else {
            return Ok(None);
        };
        let Ok(value) = kwargs.get_item("loop_factories") else {
            return Ok(None);
        };
        let invalid = || {
            pytest_rs_core::pyo3::exceptions::PyValueError::new_err(
                "mark.asyncio 'loop_factories' must be a non-empty sequence of factory names",
            )
        };
        if value.is_instance_of::<pytest_rs_core::pyo3::types::PyString>() {
            return Err(invalid());
        }
        let mut names = Vec::new();
        for name in value.try_iter().map_err(|_| invalid())? {
            let name: String = name?.extract().map_err(|_| invalid())?;
            if name.is_empty() {
                return Err(invalid());
            }
            names.push(name);
        }
        if names.is_empty() {
            return Err(invalid());
        }
        Ok(Some(names))
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
            Scope::Module => item.module_instance(),
            // Package loops are shared by every module in the directory.
            Scope::Package => {
                let module = item.module_instance();
                match module.rsplit_once('/') {
                    Some((dir, _)) => format!("{dir}/"),
                    None => String::new(),
                }
            }
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
        // Neither do items parametrized over event_loop_policy params.
        if let Some((_, index, _)) = item
            .fixture_params
            .iter()
            .find(|(name, _, _)| name == "event_loop_policy")
        {
            key.push_str(&format!("@policy{index}"));
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
                .and_then(|kwargs| {
                    // scope= is the deprecated loop_scope alias.
                    kwargs
                        .get_item("loop_scope")
                        .or_else(|_| kwargs.get_item("scope"))
                        .ok()
                })
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

    fn is_async_gen_func(py: Python<'_>, func: &Py<PyAny>) -> bool {
        py.import("inspect")
            .and_then(|inspect| inspect.getattr("isasyncgenfunction"))
            .and_then(|check| check.call1((func.bind(py),)))
            .and_then(|result| result.extract())
            .unwrap_or(false)
    }

    /// A hypothesis-decorated sync wrapper around an async inner test still
    /// counts as an async test (no "not an async function" warning).
    fn is_hypothesis_async(&self, py: Python<'_>, func: &Py<PyAny>) -> bool {
        self.helper(py)
            .and_then(|helper| helper.getattr("hypothesis_async_inner"))
            .and_then(|inner| inner.call1((func.bind(py),)))
            .map(|inner| !inner.is_none())
            .unwrap_or(false)
    }

    fn warn(py: Python<'_>, message: &str) -> PyResult<()> {
        let category = py.import("pytest")?.getattr("PytestWarning")?;
        py.import("warnings")?
            .call_method1("warn", (message, category))?;
        Ok(())
    }

    fn warn_deprecation(py: Python<'_>, message: &str) -> PyResult<()> {
        let category = py.import("builtins")?.getattr("DeprecationWarning")?;
        py.import("warnings")?
            .call_method1("warn", (message, category))?;
        Ok(())
    }

    /// warn_explicit with registry=None: never deduplicated, attributed to
    /// the item's file (upstream warns once per fixture setup).
    fn warn_pytest_deprecation(py: Python<'_>, item: &TestItem, message: &str) -> PyResult<()> {
        let category = py.import("pytest")?.getattr("PytestDeprecationWarning")?;
        py.import("warnings")?.call_method1(
            "warn_explicit",
            (
                message,
                category,
                item.path.to_string_lossy().as_ref(),
                item.lineno,
            ),
        )?;
        Ok(())
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
        parser.add_option(OptDef::flag(
            "--asyncio-debug",
            "enable asyncio debug mode for event loops",
        ));
    }

    fn pytest_configure(&mut self, ctx: &mut HookContext) -> PyResult<()> {
        self.debug = ctx.config.get_flag("asyncio-debug")
            || matches!(
                ctx.config.get_ini("asyncio_debug").map(str::trim),
                Some("true") | Some("True") | Some("1")
            );
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
                                 Valid scopes are: function, class, module, package, session."
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

        // `import pytest_asyncio` in upstream suites resolves to our shim:
        // a real package in the shim dir (which leads sys.path), so
        // `pytest_asyncio.plugin` is importable and monkeypatchable.
        let package_root = pytest_rs_core::python::shim_root().join("pytest_asyncio");
        std::fs::create_dir_all(&package_root).map_err(|e| PyOSError::new_err(e.to_string()))?;
        for (rel, content) in SHIM_FILES {
            std::fs::write(package_root.join(rel), content)
                .map_err(|e| PyOSError::new_err(e.to_string()))?;
        }
        let plugin_module = ctx.py.import("pytest_asyncio.plugin")?;
        pytest_rs_core::python::register_plugin_fixtures(
            ctx.py,
            &plugin_module,
            &mut ctx.session.registry,
        )?;
        Ok(())
    }

    fn pytest_collection_modifyitems(
        &self,
        ctx: &mut HookContext,
        items: &mut Vec<TestItem>,
    ) -> PyResult<()> {
        let py = ctx.py;
        // Pre-pass: the implicit asyncio mark in auto mode, and expansion
        // of async items over parametrized event_loop_policy fixtures.
        let pre = std::mem::take(items);
        let mut taken = Vec::new();
        for mut item in pre {
            if !item.is_coroutine || !self.applicable(&item) {
                taken.push(item);
                continue;
            }
            if self.mode == Mode::Auto && item.get_closest_marker("asyncio").is_none() {
                let mark = py
                    .import("pytest")?
                    .getattr("mark")?
                    .getattr("asyncio")?
                    .getattr("mark")?;
                item.marks.push(pytest_rs_core::collect::MarkData {
                    name: "asyncio".to_string(),
                    obj: mark.unbind(),
                });
            }
            let policy_def = ctx
                .session
                .registry
                .lookup("event_loop_policy", &item.nodeid);
            let Some(policy_def) = policy_def else {
                taken.push(item);
                continue;
            };
            let already_expanded = item
                .fixture_params
                .iter()
                .any(|(name, _, _)| name == "event_loop_policy");
            let Some(params) = policy_def.params.as_ref().filter(|_| !already_expanded) else {
                taken.push(item);
                continue;
            };
            let values: Vec<Py<PyAny>> = params
                .bind(py)
                .try_iter()?
                .map(|value| value.map(|v| v.unbind()))
                .collect::<PyResult<_>>()?;
            for (index, value) in values.into_iter().enumerate() {
                let id = format!("event_loop_policy{index}");
                let nodeid = if item.nodeid.ends_with(']') {
                    format!("{}-{id}]", &item.nodeid[..item.nodeid.len() - 1])
                } else {
                    format!("{}[{id}]", item.nodeid)
                };
                let mut fixture_params: Vec<(String, usize, Py<PyAny>)> = item
                    .fixture_params
                    .iter()
                    .map(|(name, idx, val)| (name.clone(), *idx, val.clone_ref(py)))
                    .collect();
                fixture_params.push(("event_loop_policy".to_string(), index, value));
                taken.push(TestItem {
                    nodeid,
                    path: item.path.clone(),
                    module_name: item.module_name.clone(),
                    func_name: item.func_name.clone(),
                    func: item.func.clone_ref(py),
                    cls: item.cls.as_ref().map(|cls| cls.clone_ref(py)),
                    is_coroutine: item.is_coroutine,
                    is_doctest: item.is_doctest,
                    fixture_names: item.fixture_names.clone(),
                    extra_fixture_names: item.extra_fixture_names.clone(),
                    marks: item
                        .marks
                        .iter()
                        .map(|mark| pytest_rs_core::collect::MarkData {
                            name: mark.name.clone(),
                            obj: mark.obj.clone_ref(py),
                        })
                        .collect(),
                    callspec: item
                        .callspec
                        .iter()
                        .map(|(name, value)| (name.clone(), value.clone_ref(py)))
                        .collect(),
                    fixture_params,
                    lineno: item.lineno,
                });
            }
        }
        for item in taken {
            if !item.is_coroutine {
                let is_async_gen = Self::is_async_gen_func(py, &item.func);
                if is_async_gen && self.applicable(&item) {
                    Self::warn(
                        py,
                        "Tests based on asynchronous generators are not supported. \
                         Please use native coroutines, instead.",
                    )?;
                } else if !is_async_gen
                    && item.get_closest_marker("asyncio").is_some()
                    && !self.is_hypothesis_async(py, &item.func)
                {
                    Self::warn(
                        py,
                        &format!(
                            "The test {} is marked with '@pytest.mark.asyncio' but it is \
                             not an async function. Please remove the asyncio mark. If \
                             the test is not marked explicitly, check for global marks \
                             applied via 'pytestmark'.",
                            item.func_name
                        ),
                    )?;
                }
            }
            if !item.is_coroutine || !self.applicable(&item) {
                items.push(item);
                continue;
            }
            // mark.asyncio(scope=...) is the deprecated loop_scope alias.
            if let Some(mark) = item.get_closest_marker("asyncio")
                && let Ok(kwargs) = mark.obj.bind(py).getattr("kwargs")
                && kwargs.get_item("scope").is_ok()
                && kwargs.get_item("loop_scope").is_err()
            {
                Self::warn_deprecation(
                    py,
                    "The 'scope' keyword argument to mark.asyncio is deprecated, \
                     use 'loop_scope' instead.",
                )?;
            }
            let requested = match self.marker_loop_factories(py, &item) {
                Ok(requested) => requested,
                // Invalid marker values error per item at setup, not here.
                Err(_) => {
                    items.push(item);
                    continue;
                }
            };
            let factories = match self.resolve_hook_factories(ctx, &item) {
                Ok(factories) => factories,
                // Invalid hook results error per item at setup, not here.
                Err(_) => {
                    items.push(item);
                    continue;
                }
            };
            let Some(factories) = factories else {
                // No hook impls; a marker-requested subset errors at setup.
                items.push(item);
                continue;
            };
            // mark.asyncio(loop_factories=[...]) selects a subset by name;
            // requested names the hook doesn't provide skip at run time.
            let selected: Vec<(String, Option<Py<PyAny>>)> = match &requested {
                Some(names) => names
                    .iter()
                    .map(|name| {
                        let factory = factories
                            .iter()
                            .find(|(provided, _)| provided == name)
                            .map(|(_, factory)| factory.clone_ref(py));
                        (name.clone(), factory)
                    })
                    .collect(),
                None => factories
                    .into_iter()
                    .map(|(name, factory)| (name, Some(factory)))
                    .collect(),
            };
            // A single factory keeps the plain test name (HIDDEN_PARAM
            // upstream); several parametrize the item.
            let single = selected.len() == 1;
            for (name, factory) in selected {
                let nodeid = if single {
                    item.nodeid.clone()
                } else if item.nodeid.ends_with(']') {
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
                let mut extra_fixture_names = item.extra_fixture_names.clone();
                match factory {
                    Some(factory) => {
                        let pair = pytest_rs_core::pyo3::types::PyTuple::new(
                            py,
                            [
                                name.clone().into_pyobject(py)?.into_any(),
                                factory.bind(py).clone(),
                            ],
                        )?;
                        marks.push(pytest_rs_core::collect::MarkData {
                            name: "_asyncio_loop_factory".to_string(),
                            obj: pair.into_any().unbind(),
                        });
                        extra_fixture_names.push("_asyncio_loop_factory".to_string());
                    }
                    None => {
                        // Requested but not provided by the hook.
                        let reason =
                            format!("Loop factory '{name}' is not available on this platform");
                        let kwargs = pytest_rs_core::pyo3::types::PyDict::new(py);
                        kwargs.set_item("reason", reason)?;
                        let obj = py.import("types")?.getattr("SimpleNamespace")?.call(
                            (),
                            Some(&{
                                let ns = pytest_rs_core::pyo3::types::PyDict::new(py);
                                ns.set_item(
                                    "args",
                                    pytest_rs_core::pyo3::types::PyTuple::empty(py),
                                )?;
                                ns.set_item("kwargs", kwargs)?;
                                ns
                            }),
                        )?;
                        marks.push(pytest_rs_core::collect::MarkData {
                            name: "skip".to_string(),
                            obj: obj.unbind(),
                        });
                    }
                }
                items.push(TestItem {
                    nodeid,
                    path: item.path.clone(),
                    module_name: item.module_name.clone(),
                    func_name: item.func_name.clone(),
                    func: item.func.clone_ref(py),
                    cls: item.cls.as_ref().map(|cls| cls.clone_ref(py)),
                    is_coroutine: item.is_coroutine,
                    is_doctest: item.is_doctest,
                    fixture_names: item.fixture_names.clone(),
                    extra_fixture_names,
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

    fn pytest_fixture_cache_key(
        &self,
        ctx: &mut HookContext,
        def: &FixtureDef,
        item: &TestItem,
    ) -> HookResult<String> {
        let py = ctx.py;
        let func = def.func.bind(py);
        // Loop-bound fixtures are recreated per loop-factory variant.
        let loop_bound = def.is_coroutine
            || def.is_async_gen
            || func.hasattr("_pytest_asyncio_fixture").unwrap_or(false)
            || func.hasattr("_pytest_asyncio_loop_scope").unwrap_or(false);
        if !loop_bound {
            return Ok(None);
        }
        Ok(self.item_factory(py, item)?.map(|(name, _)| name))
    }

    fn pytest_runtest_setup(&self, ctx: &mut HookContext, item: &TestItem) -> PyResult<()> {
        // mark.asyncio takes (known) keyword arguments only.
        if let Some(mark) = item.get_closest_marker("asyncio") {
            let mark = mark.obj.bind(ctx.py);
            let positional = mark
                .getattr("args")
                .ok()
                .and_then(|args| args.len().ok())
                .unwrap_or(0)
                > 0;
            let unknown_kwarg = mark
                .getattr("kwargs")
                .ok()
                .and_then(|kwargs| {
                    kwargs.try_iter().ok().map(|keys| {
                        keys.flatten().any(|key| {
                            !matches!(
                                key.extract::<String>().as_deref(),
                                Ok("loop_scope") | Ok("loop_factories") | Ok("scope")
                            )
                        })
                    })
                })
                .unwrap_or(false);
            if positional || unknown_kwarg {
                return Err(pytest_rs_core::pyo3::exceptions::PyValueError::new_err(
                    "mark.asyncio accepts only keyword arguments",
                ));
            }
            // scope= is the deprecated alias of loop_scope; both together
            // are rejected.
            if let Ok(kwargs) = mark.getattr("kwargs")
                && kwargs.get_item("scope").is_ok()
                && kwargs.get_item("loop_scope").is_ok()
            {
                return Err(pytest_rs_core::pyo3::exceptions::PyValueError::new_err(
                    "The 'scope' and 'loop_scope' arguments are mutually exclusive",
                ));
            }
        }

        // An async item that collection left without a factory mark either
        // has an invalid loop_factories value, requested factories no hook
        // provides, or sits under hook impls that returned no valid
        // mapping: all are per-item setup errors.
        if item.is_coroutine
            && self.applicable(item)
            && item.get_closest_marker("_asyncio_loop_factory").is_none()
        {
            let requested = self.marker_loop_factories(ctx.py, item)?;
            let has_hooks = !self.hook_funcs(ctx, &item.nodeid).is_empty();
            if !has_hooks {
                if requested.is_some() {
                    return Err(PyRuntimeError::new_err(
                        "mark.asyncio 'loop_factories' requires at least one \
                         pytest_asyncio_loop_factories hook implementation.",
                    ));
                }
            } else {
                self.resolve_hook_factories(ctx, item)?;
            }
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
                            && (key == prev
                                || key.starts_with(&format!("{prev}::"))
                                || key.starts_with(&format!("{prev}#"))
                                || key.starts_with(&format!("{prev}@")))
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
        let func = def.func.bind(ctx.py);
        let is_asyncio_fixture = func.hasattr("_pytest_asyncio_fixture").unwrap_or(false)
            || func.hasattr("_pytest_asyncio_loop_scope").unwrap_or(false);
        if !def.is_coroutine && !def.is_async_gen {
            // A sync pytest_asyncio.fixture expects its loop scope's loop
            // installed as current for both setup and teardown.
            if !is_asyncio_fixture {
                return Ok(None);
            }
            let factory = self.item_factory(ctx.py, item)?;
            let policy = self.loop_policy(ctx, item);
            let py = ctx.py;
            let scope = self.fixture_loop_scope(py, def);
            let loop_ = self.loop_for(
                py,
                scope,
                &Self::scope_key(py, scope, item),
                factory.as_ref().map(|(_, factory)| factory),
                policy.as_ref(),
            )?;
            let helper = self.helper(py)?;
            helper
                .getattr("set_current_loop")?
                .call1((loop_.bind(py),))?;
            if !def.is_generator {
                // Plain sync fixture: the core calls it normally.
                return Ok(None);
            }
            // Sync generator fixture: claim it so teardown resumes with the
            // same loop installed as current.
            let generator = pytest_rs_core::python::call_fixture(py, &def.func, instance, kwargs)?;
            let value = pytest_rs_core::python::next_value(py, &generator)?;
            let finalizer = helper
                .getattr("sync_gen_finalizer")?
                .call1((loop_.bind(py), &generator))?;
            return Ok(Some(FixtureValue {
                value: value.unbind(),
                finalizer: Some(Finalizer::Callable(finalizer.unbind())),
            }));
        }
        // Strict mode only handles pytest_asyncio.fixture async fixtures; a
        // plain @pytest.fixture async def resolves to its raw coroutine
        // (the core warns RemovedIn9 — an error unless filtered, pytest 9
        // parity), plus a deprecation when the test opted in.
        if self.mode == Mode::Strict && !is_asyncio_fixture {
            if item.get_closest_marker("asyncio").is_some() {
                let test_name = item.nodeid.rsplit("::").next().unwrap_or(&item.nodeid);
                pytest_rs_core::python::warn_explicit_at(
                    ctx.py,
                    "PytestDeprecationWarning",
                    &format!(
                        "asyncio test '{test_name}' requested async @pytest.fixture '{}' \
                         in strict mode. You might want to use @pytest_asyncio.fixture or \
                         switch to auto mode. This will become an error in future versions \
                         of pytest-asyncio.",
                        def.name
                    ),
                    "/pytest_asyncio/plugin.py",
                    862,
                )?;
            }
            return Ok(None);
        }

        let factory = self.item_factory(ctx.py, item)?;
        let policy = self.loop_policy(ctx, item);
        let py = ctx.py;
        let helper = self.helper(py)?;
        let scope = self.fixture_loop_scope(py, def);
        let loop_ = self.loop_for(
            py,
            scope,
            &Self::scope_key(py, scope, item),
            factory.as_ref().map(|(_, factory)| factory),
            policy.as_ref(),
        )?;

        // Async fixtures adopt a copy of the current context: contextvars
        // they set propagate to dependent fixtures and the test, and the
        // previous context is restored at their teardown.
        let (new_ctx, prev_ctx): (Py<PyAny>, Py<PyAny>) =
            helper.getattr("adopt_context")?.call0()?.extract()?;

        if def.is_coroutine {
            let coro = pytest_rs_core::python::call_fixture(py, &def.func, instance, kwargs)?;
            let value = helper.getattr("run")?.call1((loop_.bind(py), coro))?;
            let finalizer = helper.getattr("context_restoring_finalizer")?.call1((
                py.None(),
                new_ctx.bind(py),
                prev_ctx.bind(py),
            ))?;
            return Ok(Some(FixtureValue {
                value: value.unbind(),
                finalizer: Some(Finalizer::Callable(finalizer.unbind())),
            }));
        }

        // async generator fixture
        let agen = pytest_rs_core::python::call_fixture(py, &def.func, instance, kwargs)?;
        let value = helper
            .getattr("async_gen_first")?
            .call1((loop_.bind(py), &agen))?;
        let inner = helper
            .getattr("async_gen_finalizer")?
            .call1((loop_.bind(py), &agen))?;
        let finalizer = helper.getattr("context_restoring_finalizer")?.call1((
            inner,
            new_ctx.bind(py),
            prev_ctx.bind(py),
        ))?;
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
        if !item.is_coroutine
            && self.applicable(item)
            && Self::is_async_gen_func(ctx.py, &item.func)
        {
            // Raises the imperative XFailed outcome.
            ctx.py.import("pytest")?.call_method1(
                "xfail",
                ("Tests based on asynchronous generators are not supported",),
            )?;
        }
        // Hypothesis wraps async tests in a sync shim; rewire its
        // inner_test to drive each example on the item's loop.
        if !item.is_coroutine && self.applicable(item) {
            let py = ctx.py;
            let inner = self
                .helper(py)?
                .getattr("hypothesis_async_inner")?
                .call1((callable.bind(py),))?;
            if !inner.is_none() {
                let factory = self.item_factory(py, item)?;
                let policy = self.loop_policy(ctx, item);
                let py = ctx.py;
                let helper = self.helper(py)?;
                let scope = self.test_loop_scope(py, item);
                let loop_ = self.loop_for(
                    py,
                    scope,
                    &Self::scope_key(py, scope, item),
                    factory.as_ref().map(|(_, factory)| factory),
                    policy.as_ref(),
                )?;
                let wrapper = helper
                    .getattr("hypothesis_wrap")?
                    .call1((loop_.bind(py), &inner))?;
                callable
                    .bind(py)
                    .getattr("hypothesis")?
                    .setattr("inner_test", wrapper)?;
                pytest_rs_core::python::call_with_kwargs(py, callable, kwargs)?;
                return Ok(Some(()));
            }
        }
        if !item.is_coroutine || !self.applicable(item) {
            return Ok(None);
        }
        let factory = self.item_factory(ctx.py, item)?;
        let policy = self.loop_policy(ctx, item);
        let py = ctx.py;
        let helper = self.helper(py)?;
        let scope = self.test_loop_scope(py, item);
        let loop_ = self.loop_for(
            py,
            scope,
            &Self::scope_key(py, scope, item),
            factory.as_ref().map(|(_, factory)| factory),
            policy.as_ref(),
        )?;
        let coro = pytest_rs_core::python::call_with_kwargs(py, callable, kwargs)?;
        helper.getattr("run")?.call1((loop_.bind(py), coro))?;
        Ok(Some(()))
    }

    fn pytest_runtest_teardown(&self, ctx: &mut HookContext, item: &TestItem) -> PyResult<()> {
        // Function-scoped loops die with their item (keys may carry a
        // "#factory" variant suffix).
        let stale: Vec<_> = self
            .loops
            .borrow()
            .keys()
            .filter(|(scope, key)| {
                *scope == Scope::Function
                    && (key == &item.nodeid
                        || key.starts_with(&format!("{}#", item.nodeid))
                        || key.starts_with(&format!("{}@", item.nodeid)))
            })
            .cloned()
            .collect();
        for entry in stale {
            if let Some(loop_) = self.loops.borrow_mut().remove(&entry) {
                self.close_loop(ctx.py, &loop_)?;
            }
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
