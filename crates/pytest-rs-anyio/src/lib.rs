//! anyio pytest plugin equivalent: runs anyio-marked async tests and their
//! async fixtures through the installed anyio library's TestRunner.
//!
//! The real anyio distribution stays entry-point autoloaded so its plugin
//! module's fixtures (anyio_backend & friends) register normally; only the
//! hooks pytest-rs cannot emulate from Python (pytest_pyfunc_call,
//! pytest_fixture_setup, the marker -> usefixtures injection) live here.

use std::ffi::CString;

use pytest_rs_core::collect::{MarkData, TestItem};
use pytest_rs_core::fixture::{FixtureDef, Scope};
use pytest_rs_core::hooks::{FixtureValue, HookContext, HookResult, Plugin};
use pytest_rs_core::pyo3::exceptions::PyRuntimeError;
use pytest_rs_core::pyo3::prelude::*;
use pytest_rs_core::pyo3::types::{PyDict, PyModule};
use pytest_rs_core::session::Finalizer;

const HELPER: &str = include_str!("../py/helper.py");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Auto,
    Strict,
}

pub struct AnyioPlugin {
    mode: Mode,
    helper: Option<Py<PyModule>>,
}

impl Default for AnyioPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl AnyioPlugin {
    pub fn new() -> Self {
        Self {
            mode: Mode::Strict,
            helper: None,
        }
    }

    fn helper<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyModule>> {
        self.helper
            .as_ref()
            .map(|m| m.bind(py).clone())
            .ok_or_else(|| PyRuntimeError::new_err("anyio plugin not configured"))
    }

    /// Fixture names requested through @pytest.mark.usefixtures marks.
    fn usefixtures_names<'a>(
        py: Python<'a>,
        item: &'a TestItem,
    ) -> impl Iterator<Item = String> + 'a {
        item.marks
            .iter()
            .filter(|mark| mark.name == "usefixtures")
            .flat_map(move |mark| {
                mark.obj
                    .bind(py)
                    .getattr("args")
                    .ok()
                    .and_then(|args| args.extract::<Vec<String>>().ok())
                    .unwrap_or_default()
            })
    }

    /// Whether the item involves anyio_backend at all: the anyio marker
    /// (incl. the one injected at collection), a signature/usefixtures
    /// request, or a parametrized assignment. Mirrors upstream's
    /// `pyfuncitem.funcargs.get("anyio_backend")` reachability.
    fn item_involves_backend(py: Python<'_>, item: &TestItem) -> bool {
        item.get_closest_marker("anyio").is_some()
            || item
                .fixture_names
                .iter()
                .any(|name| name == "anyio_backend")
            || item
                .fixture_params
                .iter()
                .any(|(name, _, _)| name == "anyio_backend")
            || item
                .callspec
                .iter()
                .any(|(name, _)| name == "anyio_backend")
            || Self::usefixtures_names(py, item).any(|name| name == "anyio_backend")
    }

    /// The anyio_backend value for this item: the fixture's own kwargs, the
    /// engine's fixture cache (resolved earlier through the injected
    /// usefixtures mark), or the raw parametrized value as a last resort.
    fn backend_for(
        ctx: &mut HookContext,
        item: &TestItem,
        kwargs: &[(String, Py<PyAny>)],
    ) -> Option<Py<PyAny>> {
        let py = ctx.py;
        if let Some((_, value)) = kwargs.iter().find(|(name, _)| name == "anyio_backend") {
            return Some(value.clone_ref(py));
        }
        // Direct parametrize of the fixture name: the value IS the backend.
        if let Some((_, value)) = item
            .callspec
            .iter()
            .find(|(name, _)| name == "anyio_backend")
        {
            return Some(value.clone_ref(py));
        }
        let def = ctx.session.registry.lookup("anyio_backend", &item.nodeid)?;
        let instance = match def.scope {
            Scope::Function => item.nodeid.clone(),
            Scope::Class => item.class_instance(),
            Scope::Module | Scope::Package => item.module_instance(),
            Scope::Session => String::new(),
        };
        let param = item
            .fixture_params
            .iter()
            .find(|(name, _, _)| name == "anyio_backend");
        let cache_key = (
            def.name.clone(),
            def.baseid.clone(),
            instance,
            param.map(|(_, index, _)| *index),
        );
        if let Some(cached) = ctx.session.fixture_cache.get(&cache_key) {
            return Some(cached.clone_ref(py));
        }
        param.map(|(_, _, value)| value.clone_ref(py))
    }

    /// A short id for the backend param value: the backend name for plain
    /// strings and (name, options) tuples, an indexed fallback otherwise
    /// (matches the engine's id derivation for these shapes).
    fn param_id(py: Python<'_>, def: &FixtureDef, value: &Py<PyAny>, index: usize) -> String {
        if let Some(ids) = def.ids.as_ref() {
            let bound = ids.bind(py);
            let derived = if bound.is_callable() {
                bound.call1((value.bind(py),)).ok()
            } else {
                bound.get_item(index).ok()
            };
            if let Some(id) = derived
                && !id.is_none()
                && let Ok(text) = id.str()
            {
                return text.to_string();
            }
        }
        let bound = value.bind(py);
        if let Ok(text) = bound.extract::<String>() {
            return text;
        }
        format!("anyio_backend{index}")
    }
}

impl Plugin for AnyioPlugin {
    fn name(&self) -> &str {
        "anyio"
    }

    fn pytest_configure(&mut self, ctx: &mut HookContext) -> PyResult<()> {
        self.mode = match ctx.config.get_ini("anyio_mode").map(str::trim) {
            None | Some("strict") => Mode::Strict,
            Some("auto") => Mode::Auto,
            Some(other) => {
                return Err(pytest_rs_core::python::usage_error(
                    ctx.py,
                    &format!("'{other}' is not a valid anyio_mode. Valid modes: auto, strict."),
                ));
            }
        };
        // Upstream warns when both async plugins run in auto mode.
        let asyncio_auto = ctx
            .config
            .get_value("--asyncio-mode")
            .or_else(|| ctx.config.get_ini("asyncio_mode"))
            .map(str::trim)
            == Some("auto");
        let asyncio_disabled = ctx.config.plugin_opts.iter().any(|spec| {
            spec.strip_prefix("no:").is_some_and(|disabled| {
                disabled
                    .trim_start_matches("pytest_")
                    .trim_start_matches("pytest-")
                    == "asyncio"
            })
        });
        if self.mode == Mode::Auto && asyncio_auto && !asyncio_disabled {
            pytest_rs_core::python::warn_explicit_at(
                ctx.py,
                "PytestConfigWarning",
                "AnyIO auto mode has been enabled together with pytest-asyncio auto \
                 mode. This may cause unexpected behavior.",
                "/anyio/pytest_plugin.py",
                0,
            )?;
        }
        let module = PyModule::from_code(
            ctx.py,
            CString::new(HELPER)?.as_c_str(),
            c"pytest_rs_anyio/helper.py",
            c"_pytest_rs_anyio",
        )?;
        self.helper = Some(module.unbind());
        Ok(())
    }

    /// Marked (or auto-mode) coroutine tests implicitly request the
    /// anyio_backend fixture (upstream applies usefixtures in
    /// pytest_pycollect_makeitem); param expansion already ran, so items
    /// are cloned per backend here, like the asyncio policy expansion.
    fn pytest_collection_modifyitems(
        &self,
        ctx: &mut HookContext,
        items: &mut Vec<TestItem>,
    ) -> PyResult<()> {
        let py = ctx.py;
        let pre = std::mem::take(items);
        let mut taken = Vec::new();
        for mut item in pre {
            let marked = item.get_closest_marker("anyio").is_some();
            if self.mode == Mode::Strict && !marked {
                taken.push(item);
                continue;
            }
            // Hypothesis wraps async tests in a sync shim; its coroutine
            // inner_test makes the item anyio-run like a plain async test.
            let async_like = item.is_coroutine
                || !self
                    .helper(py)?
                    .getattr("hypothesis_async_inner")?
                    .call1((item.func.bind(py),))?
                    .is_none();
            if !async_like {
                taken.push(item);
                continue;
            }
            if self.mode == Mode::Auto && !marked {
                let mark = py
                    .import("pytest")?
                    .getattr("mark")?
                    .getattr("anyio")?
                    .getattr("mark")?;
                item.marks.push(MarkData {
                    name: "anyio".to_string(),
                    obj: mark.unbind(),
                });
            }
            // Already requested through the signature, an existing
            // usefixtures mark, or a direct parametrize of the fixture name
            // (the callspec value overrides the fixture).
            let already_requested = item.fixture_names.iter().any(|n| n == "anyio_backend")
                || item
                    .callspec
                    .iter()
                    .any(|(name, _)| name == "anyio_backend")
                || Self::usefixtures_names(py, &item).any(|name| name == "anyio_backend");
            if already_requested {
                taken.push(item);
                continue;
            }
            let Some(def) = ctx.session.registry.lookup("anyio_backend", &item.nodeid) else {
                // No anyio_backend fixture visible (anyio not installed):
                // leave the item to the engine's native-support failure.
                taken.push(item);
                continue;
            };
            // The runner resolves usefixtures-named fixtures before the
            // test's own, so anyio_backend is in the cache by the time the
            // test (or any async fixture) needs it.
            let usefixtures_mark = py
                .import("pytest")?
                .getattr("mark")?
                .getattr("usefixtures")?
                .call1(("anyio_backend",))?
                .getattr("mark")?;
            item.marks.push(MarkData {
                name: "usefixtures".to_string(),
                obj: usefixtures_mark.unbind(),
            });
            // indirect parametrize already assigned the backend param;
            // resolution happens through the mark above, no cloning.
            let already_assigned = item
                .fixture_params
                .iter()
                .any(|(name, _, _)| name == "anyio_backend");
            let Some(params) = def.params.as_ref().filter(|_| !already_assigned) else {
                taken.push(item);
                continue;
            };
            let values: Vec<Py<PyAny>> = params
                .bind(py)
                .try_iter()?
                .map(|value| value.map(|v| v.unbind()))
                .collect::<PyResult<_>>()?;
            for (index, wrapped) in values.into_iter().enumerate() {
                let (value, spec_id, extra_marks) =
                    pytest_rs_core::python::unwrap_fixture_param(py, wrapped.bind(py))?;
                let id = spec_id.unwrap_or_else(|| Self::param_id(py, &def, &value, index));
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
                fixture_params.push(("anyio_backend".to_string(), index, value));
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
                        .map(|mark| MarkData {
                            name: mark.name.clone(),
                            obj: mark.obj.clone_ref(py),
                        })
                        .chain(extra_marks)
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
        *items = taken;
        Ok(())
    }

    /// Async fixtures of one item run per backend; without this, a
    /// module-scoped async fixture set up under [asyncio] would be reused
    /// (with its closed runner) by the [trio] twin.
    fn pytest_fixture_cache_key(
        &self,
        ctx: &mut HookContext,
        def: &FixtureDef,
        item: &TestItem,
    ) -> HookResult<String> {
        if !def.is_coroutine && !def.is_async_gen {
            return Ok(None);
        }
        if !Self::item_involves_backend(ctx.py, item) {
            return Ok(None);
        }
        let Some(backend) = Self::backend_for(ctx, item, &[]) else {
            return Ok(None);
        };
        let py = ctx.py;
        let name = match backend.bind(py).extract::<String>() {
            Ok(name) => name,
            Err(_) => backend
                .bind(py)
                .get_item(0)
                .and_then(|n| n.str().map(|s| s.to_string()))
                .unwrap_or_else(|_| "anyio".to_string()),
        };
        Ok(Some(format!("anyio:{name}")))
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
        // Upstream wraps async fixtures only when anyio_backend is in the
        // request's fixturenames; everything else stays unhandled (the
        // engine's native-support warning, or another async plugin).
        if !Self::item_involves_backend(ctx.py, item) {
            return Ok(None);
        }
        let Some(backend) = Self::backend_for(ctx, item, kwargs) else {
            return Ok(None);
        };
        let py = ctx.py;
        let helper = self.helper(py)?;
        let fixture_kwargs = PyDict::new(py);
        for (name, value) in kwargs {
            fixture_kwargs.set_item(name, value.bind(py))?;
        }
        let fixture_instance = if def.needs_instance {
            instance.map(|obj| obj.clone_ref(py))
        } else {
            None
        };
        if def.is_coroutine {
            let value = helper.getattr("run_fixture")?.call1((
                def.func.bind(py),
                fixture_instance,
                backend.bind(py),
                &fixture_kwargs,
            ))?;
            return Ok(Some(FixtureValue {
                value: value.unbind(),
                finalizer: None,
            }));
        }
        // Async generator fixture: the helper object keeps the runner lease
        // open from setup until the finalizer runs.
        let gen_fixture = helper.getattr("AsyncGenFixture")?.call1((
            def.func.bind(py),
            fixture_instance,
            backend.bind(py),
            &fixture_kwargs,
        ))?;
        let value = gen_fixture.call_method0("setup")?;
        let finalizer = gen_fixture.getattr("finalize")?;
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
        if !Self::item_involves_backend(ctx.py, item) {
            return Ok(None);
        }
        if !item.is_coroutine {
            // Hypothesis-wrapped async test: rewire inner_test to drive each
            // example through this item's backend runner.
            let py = ctx.py;
            let helper = self.helper(py)?;
            let inner = helper
                .getattr("hypothesis_async_inner")?
                .call1((callable.bind(py),))?;
            if inner.is_none() {
                return Ok(None);
            }
            let Some(backend) = Self::backend_for(ctx, item, kwargs) else {
                return Ok(None);
            };
            let wrapper = helper
                .getattr("hypothesis_wrap")?
                .call1((&inner, backend.bind(py)))?;
            callable
                .bind(py)
                .getattr("hypothesis")?
                .setattr("inner_test", wrapper)?;
            pytest_rs_core::python::call_with_kwargs(py, callable, kwargs)?;
            return Ok(Some(()));
        }
        let Some(backend) = Self::backend_for(ctx, item, kwargs) else {
            return Ok(None);
        };
        let py = ctx.py;
        let helper = self.helper(py)?;
        let test_kwargs = PyDict::new(py);
        for (name, value) in kwargs {
            test_kwargs.set_item(name, value.bind(py))?;
        }
        helper
            .getattr("run_test")?
            .call1((callable.bind(py), backend.bind(py), &test_kwargs))?;
        Ok(Some(()))
    }
}
