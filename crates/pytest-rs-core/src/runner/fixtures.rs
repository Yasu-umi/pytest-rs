//! Per-item fixture resolution and the RESOLVE_CTX thread-local.

#[allow(unused_imports)]
use super::*;
use crate::collect::TestItem;
use crate::config::Config;
use crate::fixture::Scope;
use crate::hooks::{HookContext, Plugin};
use crate::python;
use crate::session::{Finalizer, PendingFinalizer, Session};

/// State for `request.getfixturevalue()`: raw pointers to the engine state
/// of the item currently running on this thread. Only dereferenced from
/// `getfixturevalue` while Python code called by the runner is on the stack —
/// the suspended Rust frames in between never touch the session concurrently.
pub(crate) struct ResolveCtx {
    plugins: *const [Box<dyn Plugin>],
    session: *mut Session,
    config: *const Config,
    item: *const TestItem,
    class_instance: Option<Py<PyAny>>,
}

thread_local! {
    static RESOLVE_CTX: std::cell::RefCell<Vec<ResolveCtx>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Pops the context pushed by `push_resolve_ctx` (kept alive for the whole
/// item run, teardown included).
pub(crate) struct ResolveCtxGuard(());

impl Drop for ResolveCtxGuard {
    fn drop(&mut self) {
        RESOLVE_CTX.with(|stack| {
            stack.borrow_mut().pop();
        });
    }
}

pub(crate) fn push_resolve_ctx(
    plugins: &[Box<dyn Plugin>],
    session: &mut Session,
    config: &Config,
    item: &TestItem,
) -> ResolveCtxGuard {
    RESOLVE_CTX.with(|stack| {
        stack.borrow_mut().push(ResolveCtx {
            plugins,
            session,
            config,
            item,
            class_instance: None,
        });
    });
    ResolveCtxGuard(())
}

/// Record the test's class instance once setup created it, so dynamically
/// requested fixtures with needs_instance bind to the right object.
pub(crate) fn set_resolve_ctx_instance(py: Python<'_>, instance: Option<&Py<PyAny>>) {
    RESOLVE_CTX.with(|stack| {
        if let Some(ctx) = stack.borrow_mut().last_mut() {
            ctx.class_instance = instance.map(|obj| obj.clone_ref(py));
        }
    });
}

/// The running item's class instance (TestCase or fresh Test class
/// instance), if any — backs `request.instance` / `request.cls`.
pub(crate) fn current_resolve_instance(py: Python<'_>) -> Option<Py<PyAny>> {
    RESOLVE_CTX.with(|stack| {
        stack
            .borrow()
            .last()
            .and_then(|ctx| ctx.class_instance.as_ref().map(|obj| obj.clone_ref(py)))
    })
}

/// `request.getfixturevalue(name)`: dynamic fixture resolution from Python
/// while a test item is running (fixture setup, the test body, or teardown).
#[allow(unsafe_code)]
pub(crate) fn getfixturevalue(py: Python<'_>, name: &str) -> PyResult<Py<PyAny>> {
    let ctx = RESOLVE_CTX.with(|stack| {
        let stack = stack.borrow();
        stack.last().map(|ctx| {
            (
                ctx.plugins,
                ctx.session,
                ctx.config,
                ctx.item,
                ctx.class_instance.as_ref().map(|obj| obj.clone_ref(py)),
            )
        })
    });
    let Some((plugins, session, config, item, instance)) = ctx else {
        return Err(pyo3::exceptions::PyRuntimeError::new_err(
            "getfixturevalue() is only available while a test is running",
        ));
    };
    // Safety: the pointers were pushed by the run_one frame below us on this
    // thread's stack and stay valid until its drop guard pops them; that
    // frame is suspended inside a Python call and does not touch the session
    // while Python (and hence this resolver) runs.
    let (plugins, session, config, item) = unsafe { (&*plugins, &mut *session, &*config, &*item) };
    // pytest raises FixtureLookupError for unknown names (callers catch it).
    if name != "pytestconfig" && session.registry.lookup(name, &item.nodeid).is_none() {
        let err_type = py
            .import("_pytest.fixtures")?
            .getattr("FixtureLookupError")?;
        return Err(PyErr::from_value(
            err_type.call1((format!("fixture '{name}' not found"),))?,
        ));
    }
    let mut stack = Vec::new();
    resolve_fixture(
        py,
        plugins,
        session,
        config,
        name,
        item,
        instance.as_ref(),
        &mut stack,
    )
}

/// Resolve one fixture by name for an item, using the cache, recursing into
/// dependencies, and letting plugins claim setup (async fixtures).
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_fixture(
    py: Python<'_>,
    plugins: &[Box<dyn Plugin>],
    session: &mut Session,
    config: &Config,
    name: &str,
    item: &TestItem,
    class_instance: Option<&Py<PyAny>>,
    stack: &mut Vec<std::sync::Arc<crate::fixture::FixtureDef>>,
) -> PyResult<Py<PyAny>> {
    // Direct (non-indirect) parametrize of a fixture name: the callspec
    // value replaces the fixture outright, its function never runs
    // (pytest's PseudoFixtureDef bypass).
    if let Some((_, value)) = item.callspec.iter().find(|(param, _)| param == name) {
        return Ok(value.clone_ref(py));
    }
    // Override chain through an intermediate fixture: if a fixture of this
    // name is already resolving up the stack (e.g. a class-level `pytester`
    // depends on `django_pytester`, which depends on `pytester`), resolve to
    // the next definition below it instead of re-selecting the same override.
    let looked_up = if let Some(stacked) = stack.iter().rev().find(|d| d.name == name) {
        session
            .registry
            .lookup_overridden(name, &item.nodeid, stacked)
    } else {
        session.registry.lookup(name, &item.nodeid)
    };
    let Some(def) = looked_up else {
        // `pytestconfig` is a builtin backed by the Rust config, not a
        // shim-defined fixture (overridable like any other fixture).
        if name == "pytestconfig" {
            return python::make_py_config(py, config);
        }
        return Err(pyo3::exceptions::PyRuntimeError::new_err(format!(
            "fixture '{name}' not found for test {}",
            item.nodeid
        )));
    };
    resolve_fixture_def(
        py,
        plugins,
        session,
        config,
        def,
        item,
        class_instance,
        stack,
    )
}

/// Resolve a specific fixture definition (override-aware entry point).
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_fixture_def(
    py: Python<'_>,
    plugins: &[Box<dyn Plugin>],
    session: &mut Session,
    config: &Config,
    def: std::sync::Arc<crate::fixture::FixtureDef>,
    item: &TestItem,
    class_instance: Option<&Py<PyAny>>,
    stack: &mut Vec<std::sync::Arc<crate::fixture::FixtureDef>>,
) -> PyResult<Py<PyAny>> {
    // A def appearing twice in the same chain is a real cycle (identity by Arc
    // pointer: an override and the builtin it overrides are distinct defs that
    // share (name, "")).
    if stack.iter().any(|d| std::sync::Arc::ptr_eq(d, &def)) {
        return Err(pyo3::exceptions::PyRuntimeError::new_err(format!(
            "recursive fixture dependency involving '{}'",
            def.name
        )));
    }

    let instance = match def.scope {
        Scope::Function => item.nodeid.clone(),
        Scope::Class => item.class_instance(),
        Scope::Module | Scope::Package => item.module_instance(),
        Scope::Session => String::new(),
    };
    // Parametrized fixtures cache per param index.
    let fixture_param: Option<(usize, Py<PyAny>)> = item
        .fixture_params
        .iter()
        .find(|(fixture, _, _)| fixture == &def.name)
        .map(|(_, index, value)| (*index, value.clone_ref(py)));
    // firstresult: plugins may discriminate the key further (asyncio
    // loop-factory variants recreate loop-bound fixtures per variant).
    let keyed_name = {
        let mut ctx = HookContext {
            py,
            session,
            config,
        };
        let mut suffix = None;
        for plugin in plugins {
            if let Some(value) = plugin.pytest_fixture_cache_key(&mut ctx, &def, item)? {
                suffix = Some(value);
                break;
            }
        }
        match suffix {
            Some(suffix) => format!("{}#{suffix}", def.name),
            None => def.name.clone(),
        }
    };
    let cache_key = (
        keyed_name,
        def.baseid.clone(),
        instance.clone(),
        fixture_param.as_ref().map(|(index, _)| *index),
    );
    if let Some(cached) = session.fixture_cache.get(&cache_key) {
        return Ok(cached.clone_ref(py));
    }

    stack.push(def.clone());
    let mut request: Option<Py<crate::request::PyRequest>> = None;
    let deps_result = (|| -> PyResult<Vec<(String, Py<PyAny>)>> {
        let mut kwargs = Vec::new();
        for dep in &def.param_names {
            if dep == "request" {
                let node = python::make_node(py, item)?;
                let req = Py::new(
                    py,
                    crate::request::PyRequest::new(
                        fixture_param.as_ref().map(|(_, value)| value.clone_ref(py)),
                        node,
                        Some(def.name.clone()),
                    ),
                )?;
                kwargs.push((dep.clone(), req.clone_ref(py).into_any()));
                request = Some(req);
                continue;
            }
            let value = if dep == &def.name {
                // Fixture override: a fixture requesting its own name gets
                // the next less-specific definition.
                let Some(parent) = session.registry.lookup_overridden(dep, &item.nodeid, &def)
                else {
                    return Err(pyo3::exceptions::PyRuntimeError::new_err(format!(
                        "fixture '{dep}' not found (no less-specific definition to override)"
                    )));
                };
                resolve_fixture_def(
                    py,
                    plugins,
                    session,
                    config,
                    parent,
                    item,
                    class_instance,
                    stack,
                )?
            } else {
                resolve_fixture(
                    py,
                    plugins,
                    session,
                    config,
                    dep,
                    item,
                    class_instance,
                    stack,
                )?
            };
            kwargs.push((dep.clone(), value));
        }
        Ok(kwargs)
    })();
    stack.pop();
    let kwargs = deps_result?;

    // firstresult: a plugin may claim this fixture (async fixtures, native
    // plugin fixtures).
    let claimed = {
        let mut ctx = HookContext {
            py,
            session,
            config,
        };
        let mut claimed = None;
        let fixture_instance = if def.needs_instance {
            class_instance
        } else {
            None
        };
        for plugin in plugins {
            if let Some(value) =
                plugin.pytest_fixture_setup(&mut ctx, &def, item, fixture_instance, &kwargs)?
            {
                claimed = Some(value);
                break;
            }
        }
        claimed
    };

    let fixture_instance = if def.needs_instance {
        class_instance
    } else {
        None
    };
    let (value, finalizer) = match claimed {
        Some(fixture_value) => (fixture_value.value, fixture_value.finalizer),
        None => {
            if def.is_coroutine || def.is_async_gen {
                // pytest 8.4 parity: an unhandled async fixture resolves to
                // its raw coroutine/async-generator and warns (this becomes
                // an error in pytest 9.1).
                let test_name = item.nodeid.rsplit("::").next().unwrap_or(&item.nodeid);
                python::warn_explicit_at(
                    py,
                    "PytestRemovedIn9Warning",
                    &format!(
                        "'{test_name}' requested an async fixture '{}', with no plugin or \
                         hook that handled it. This is usually an error, as pytest does not \
                         natively support it. This will turn into an error in pytest 9.\n  \
                         See: https://docs.pytest.org/en/stable/deprecations.html\
                         #sync-test-depending-on-async-fixture",
                        def.name
                    ),
                    "_pytest/fixtures.py",
                    1188,
                )?;
                let value = python::call_fixture(py, &def.func, fixture_instance, &kwargs)?;
                (value.unbind(), None)
            } else if def.is_generator {
                let generator = python::call_fixture(py, &def.func, fixture_instance, &kwargs)?;
                let value = python::next_value(py, &generator)?;
                (value.unbind(), Some(Finalizer::GenNext(generator.unbind())))
            } else {
                let value = python::call_fixture(py, &def.func, fixture_instance, &kwargs)?;
                (value.unbind(), None)
            }
        }
    };

    // --setup-show narration: SETUP now, TEARDOWN via a print finalizer
    // pushed before the real one (LIFO: it prints after the teardown ran).
    if setup_show_active(config) {
        let (scope_char, indent) = scope_display(def.scope);
        // Parametrized fixtures display their current param: name['spam'].
        // With ids= the id shows instead of the value (pytest's
        // cached_param).
        let display_name = match &fixture_param {
            Some((index, value)) => {
                let rendered =
                    python::fixture_param_id(py, def.ids.as_ref(), value.bind(py), *index)
                        .map(|id| id.bind(py).clone())
                        .unwrap_or_else(|| value.bind(py).clone())
                        .repr()
                        .map(|repr| repr.to_string())
                        .unwrap_or_default();
                format!("{}[{rendered}]", def.name)
            }
            None => def.name.clone(),
        };
        let mut dep_names: Vec<&str> = kwargs
            .iter()
            .map(|(name, _)| name.as_str())
            .filter(|name| *name != "request")
            .collect();
        dep_names.sort_unstable();
        // Narration must reach the real terminal, not the item capture.
        // pytest's tw.line() style: a leading newline closes the current
        // line, no trailing one.
        python::capture_suspend(py);
        if dep_names.is_empty() {
            print!("\n{:indent$}SETUP    {scope_char} {display_name}", "");
        } else {
            print!(
                "\n{:indent$}SETUP    {scope_char} {display_name} (fixtures used: {})",
                "",
                dep_names.join(", ")
            );
        }
        let _ = std::io::stdout().flush();
        python::capture_resume(py);
        if let Ok(printer) = py
            .import("pytest._setupshow")
            .and_then(|m| m.getattr("teardown_printer"))
            .and_then(|f| f.call1((" ".repeat(indent), scope_char.to_string(), display_name)))
        {
            session.finalizers.push(PendingFinalizer {
                scope: def.scope,
                instance: instance.clone(),
                finalizer: Finalizer::Callable(printer.unbind()),
            });
        }
    }

    // Finalizers registered through request.addfinalizer run at this
    // fixture's scope teardown, LIFO — drained at teardown time, so
    // late additions (factory fixtures calling addfinalizer during the
    // test, e.g. anyio's sock_or_fd_factory) are included.
    if let Some(request) = &request
        && let Ok(drainer) = request.bind(py).as_any().getattr("_drain_finalizers")
    {
        session.finalizers.push(PendingFinalizer {
            scope: def.scope,
            instance: instance.clone(),
            finalizer: Finalizer::Callable(drainer.unbind()),
        });
    }
    if let Some(finalizer) = finalizer {
        session.finalizers.push(PendingFinalizer {
            scope: def.scope,
            instance: instance.clone(),
            finalizer,
        });
    }
    session.fixture_cache.insert(cache_key, value.clone_ref(py));
    Ok(value)
}
