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
    /// The fixture defs whose functions are currently executing on this thread,
    /// innermost last. `request.getfixturevalue(<own name>)` consults this so an
    /// override fixture asking for its own name resolves to the next
    /// less-specific definition rather than recursing into itself.
    static EXECUTING: std::cell::RefCell<Vec<std::sync::Arc<crate::fixture::FixtureDef>>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Per-item-run names requested dynamically via `request.getfixturevalue()`.
    /// pytest appends these to the item's `fixturenames`, so `request.fixturenames`
    /// reflects fixtures pulled in at runtime (#3057). One frame per resolve ctx.
    static DYNAMIC_NAMES: std::cell::RefCell<Vec<Vec<String>>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// The running item's node proxy, shared by every `request.node` and the
    /// logreport/makereport report so attributes a plugin sets during the
    /// test (pytest-bdd's `__scenario_report__`) survive to makereport.
    static ITEM_NODE: std::cell::RefCell<Option<(String, Py<PyAny>)>> =
        const { std::cell::RefCell::new(None) };
}

/// A node proxy for `item`, stable across the item's run (cached per nodeid).
/// Use wherever request.node and the report must be the same object.
pub(crate) fn item_node(py: Python<'_>, item: &TestItem) -> PyResult<Py<PyAny>> {
    ITEM_NODE.with(|cell| {
        if let Some((nodeid, node)) = cell.borrow().as_ref()
            && nodeid == &item.nodeid
        {
            return Ok(node.clone_ref(py));
        }
        let node = python::make_node(py, item)?;
        *cell.borrow_mut() = Some((item.nodeid.clone(), node.clone_ref(py)));
        Ok(node)
    })
}

/// Pops the context pushed by `push_resolve_ctx` (kept alive for the whole
/// item run, teardown included).
pub(crate) struct ResolveCtxGuard(());

impl Drop for ResolveCtxGuard {
    fn drop(&mut self) {
        RESOLVE_CTX.with(|stack| {
            stack.borrow_mut().pop();
        });
        DYNAMIC_NAMES.with(|stack| {
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
    DYNAMIC_NAMES.with(|stack| stack.borrow_mut().push(Vec::new()));
    // New item run: drop the previous item's cached node so request.node and
    // the report node for this item share a fresh object.
    ITEM_NODE.with(|cell| *cell.borrow_mut() = None);
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
    // Record a dynamically requested fixture so it surfaces in
    // request.fixturenames, like pytest appending to the item's closure (#3057).
    if session.registry.lookup(name, &item.nodeid).is_some() {
        DYNAMIC_NAMES.with(|stack| {
            if let Some(frame) = stack.borrow_mut().last_mut()
                && !frame.iter().any(|n| n == name)
            {
                frame.push(name.to_string());
            }
        });
    }
    let mut stack = Vec::new();
    // Override-reuse via getfixturevalue: if `name` matches a fixture currently
    // executing on this thread, an override is asking for its own name — resolve
    // to the next less-specific definition instead of recursing into itself
    // (pytest's _getnextfixturedef on the subrequest). #1953.
    let executing = EXECUTING.with(|s| s.borrow().iter().rev().find(|d| d.name == name).cloned());
    if let Some(current) = executing
        && let Some(parent) = session
            .registry
            .lookup_overridden(name, &item.nodeid, &current)
    {
        return resolve_fixture_def(
            py,
            plugins,
            session,
            config,
            parent,
            item,
            instance.as_ref(),
            &mut stack,
        );
    }
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

/// `request.fixturenames`: the running item's full fixture closure, in
/// pytest's scope-sorted order (`getfixtureclosure`), plus `request` and any
/// pseudo-fixture names. Returns `None` when no item is running (the caller
/// falls back to the node proxy's static list).
#[allow(unsafe_code)]
pub(crate) fn current_fixturenames(py: Python<'_>) -> Option<Vec<String>> {
    let ptrs =
        RESOLVE_CTX.with(|stack| stack.borrow().last().map(|ctx| (ctx.session, ctx.item)))?;
    // Safety: same invariant as getfixturevalue — the run_one frame below us
    // owns these pointers and is suspended in the Python call that reached here.
    let (session, item) = unsafe { (&*ptrs.0, &*ptrs.1) };
    // Build the closure the way pytest's getfixtureclosure does for the runtime
    // `request.fixturenames` view: autouse first, then the test's directly
    // requested args *in order* (keeping `request` in its declared position,
    // unlike `closure_for` which drops it for setup), BFS-expand dependencies,
    // then stable-sort by scope (higher scope first).
    let registry = &session.registry;
    // The closure pytest's getfixtureclosure builds: autouse + the test's
    // directly-requested args (request kept inline), expanded through override
    // chains and scope-sorted.
    let initialnames = registry.initial_names(&item.nodeid, &item.fixture_names);
    let ignore: std::collections::HashSet<String> =
        item.callspec.iter().map(|(name, _)| name.clone()).collect();
    let mut names = registry.getfixtureclosure(&item.nodeid, &initialnames, &ignore);
    let mut seen: std::collections::HashSet<String> = names.iter().cloned().collect();
    for extra in &item.extra_fixture_names {
        if seen.insert(extra.clone()) {
            names.push(extra.clone());
        }
    }
    // Fixtures pulled in at runtime via request.getfixturevalue() (#3057).
    DYNAMIC_NAMES.with(|stack| {
        if let Some(frame) = stack.borrow().last() {
            for n in frame {
                if seen.insert(n.clone()) {
                    names.push(n.clone());
                }
            }
        }
    });
    let _ = py;
    Some(names)
}

/// Build a pytest-bdd-compatible FixtureManager view of the running item's
/// fixture registry: `_arg2fixturedefs` seeded with a ShimFixtureDef per
/// registered definition (carrying its func/baseid so pytest-bdd can match
/// step fixtures by `_pytest_bdd_step_context` and alias them by name).
#[allow(unsafe_code)]
pub(crate) fn build_fixturemanager(py: Python<'_>) -> PyResult<Py<PyAny>> {
    let session_ptr = RESOLVE_CTX.with(|stack| stack.borrow().last().map(|ctx| ctx.session));
    let Some(session_ptr) = session_ptr else {
        return Err(pyo3::exceptions::PyRuntimeError::new_err(
            "_fixturemanager is only available while a test is running",
        ));
    };
    // Safety: same invariant as getfixturevalue — the run_one frame below us
    // owns this pointer and is suspended in the Python call that reached here.
    let session = unsafe { &*session_ptr };
    let entries = pyo3::types::PyList::empty(py);
    for def in session.registry.all_defs() {
        entries.append((
            def.name.as_str(),
            def.func.bind(py),
            def.baseid.as_str(),
            def.scope.as_str(),
        ))?;
    }
    py.import("pytest._fixturemanager")?
        .getattr("build_manager")?
        .call1((entries,))
        .map(|fm| fm.unbind())
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
        // A name reappearing in the active resolution stack with no
        // less-specific definition to override is a dependency cycle, not a
        // missing fixture (e.g. fix1 -> fix2 -> fix1). pytest reports it as a
        // recursive dependency rather than "not found".
        if stack.iter().any(|d| d.name == name) {
            return Err(fail_no_trace(
                py,
                &format!("recursive dependency involving fixture '{name}' detected"),
            ));
        }
        return Err(fixture_not_found_error(py, session, item, name, stack));
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

/// Raise pytest's `Failed` outcome with `pytrace=False`, so the message is the
/// whole longrepr (no traceback) — pytest's `fail(msg, pytrace=False)`.
fn fail_no_trace(py: Python<'_>, msg: &str) -> PyErr {
    match py
        .import("pytest._outcomes")
        .and_then(|m| m.getattr("Failed"))
        .and_then(|failed| failed.call1((msg,)))
    {
        Ok(exc) => {
            let _ = exc.setattr("pytrace", false);
            PyErr::from_value(exc)
        }
        Err(_) => pyo3::exceptions::PyRuntimeError::new_err(msg.to_string()),
    }
}

/// Fire the conftest `pytest_fixture_setup` hooks for a just-created fixture
/// and schedule its `pytest_fixture_post_finalizer` hooks as a finalizer (so
/// they run at the fixture's scope teardown). Conftest impls are ordered
/// most-specific first (pluggy LIFO). Each receives `fixturedef`/`request`.
#[allow(clippy::too_many_arguments)]
fn fire_fixture_lifecycle_hooks(
    py: Python<'_>,
    session: &mut Session,
    item: &TestItem,
    def: &crate::fixture::FixtureDef,
    scope: Scope,
    instance: &str,
    bindings: &[crate::session::Binding],
) -> PyResult<()> {
    let collect = |name: &str| -> Vec<Py<PyAny>> {
        let mut hooks: Vec<(usize, Py<PyAny>)> = session
            .py_hooks
            .iter()
            .filter(|h| h.name == name && item.nodeid.starts_with(h.baseid.as_str()))
            .map(|h| (h.baseid.len(), h.func.clone_ref(py)))
            .collect();
        hooks.sort_by_key(|h| std::cmp::Reverse(h.0));
        hooks.into_iter().map(|(_, f)| f).collect()
    };
    let setup_funcs = collect("pytest_fixture_setup");
    let final_funcs = collect("pytest_fixture_post_finalizer");
    if setup_funcs.is_empty() && final_funcs.is_empty() {
        return Ok(());
    }
    let node = item_node(py, item)?;
    let request = Py::new(
        py,
        crate::request::PyRequest::new(None, node, Some(def.name.clone()), scope),
    )?;
    let fixturedef = py
        .import("pytest._fixturemanager")?
        .getattr("ShimFixtureDef")?
        .call1((
            def.name.as_str(),
            def.func.bind(py),
            def.baseid.as_str(),
            scope.as_str(),
        ))?;
    let fire = py
        .import("pytest._pluginmanager")?
        .getattr("fire_fixture_hooks")?;
    if !setup_funcs.is_empty() {
        let funcs = pyo3::types::PyList::new(py, setup_funcs.iter().map(|f| f.bind(py)))?;
        fire.call1((funcs, &fixturedef, request.bind(py)))?;
    }
    if !final_funcs.is_empty() {
        let funcs = pyo3::types::PyList::new(py, final_funcs.iter().map(|f| f.bind(py)))?;
        let partial = py.import("functools")?.getattr("partial")?.call1((
            fire,
            funcs,
            &fixturedef,
            request.bind(py),
        ))?;
        session.finalizers.push(PendingFinalizer {
            scope,
            instance: instance.to_string(),
            finalizer: Finalizer::Callable(partial.unbind()),
            bindings: bindings.to_vec(),
        });
    }
    Ok(())
}

/// Build the ScopeMismatch error for a fixture (the top of `stack`) requesting
/// the narrower-scoped `requested`. Mirrors pytest's message: a `Failed`
/// outcome with `pytrace=False`, listing the requesting fixture stack and the
/// requested fixture, each as `path:lineno:  def name(sig)`.
fn scope_mismatch_error(
    py: Python<'_>,
    config: &Config,
    stack: &[std::sync::Arc<crate::fixture::FixtureDef>],
    requested: &crate::fixture::FixtureDef,
) -> PyErr {
    let requesting_scope = stack.last().map(|d| d.scope).unwrap_or(Scope::Function);
    let rootdir = config.rootdir.to_string_lossy();
    let line = |def: &crate::fixture::FixtureDef| -> String {
        py.import("pytest._showfixtures")
            .and_then(|m| m.getattr("fixturedef_line"))
            .and_then(|f| f.call1((def.func.bind(py), rootdir.as_ref())))
            .and_then(|s| s.extract::<String>())
            .unwrap_or_else(|_| format!("  def {}()", def.name))
    };
    let fixture_stack = stack.iter().map(|d| line(d)).collect::<Vec<_>>().join("\n");
    let msg = format!(
        "ScopeMismatch: You tried to access the {} scoped fixture {} with a {} scoped \
         request object. Requesting fixture stack:\n{}\nRequested fixture:\n{}",
        requested.scope.as_str(),
        requested.name,
        requesting_scope.as_str(),
        fixture_stack,
        line(requested),
    );
    fail_no_trace(py, &msg)
}

/// Build pytest's "no parameter defined for test" error for a parametrized
/// fixture requested via `getfixturevalue()` without a bound param. The Python
/// helper captures the call-site frame (the engine's Rust frames are invisible
/// to Python) and raises a `Failed` with `pytrace=False`.
fn no_parameter_error(
    py: Python<'_>,
    config: &Config,
    def: &crate::fixture::FixtureDef,
    nodeid: &str,
) -> PyErr {
    let rootpath = config.rootdir.to_string_lossy();
    match py
        .import("_pytest.fixtures")
        .and_then(|m| m.getattr("fail_subrequest_no_param"))
        .and_then(|f| {
            f.call1((
                nodeid,
                def.func.bind(py),
                def.name.as_str(),
                rootpath.as_ref(),
            ))
        }) {
        // The helper always raises; reaching Ok means it did not.
        Ok(_) => {
            pyo3::exceptions::PyRuntimeError::new_err("fixture has no parameter defined for test")
        }
        Err(err) => err,
    }
}

/// Build pytest's rich "fixture not found" error: the requesting function's
/// def line(s), the message, the sorted available-fixtures list, and the
/// --fixtures help line. The requesting function is the innermost fixture on
/// the resolution stack, or the test function for a directly requested name.
fn fixture_not_found_error(
    py: Python<'_>,
    session: &Session,
    item: &TestItem,
    name: &str,
    stack: &[std::sync::Arc<crate::fixture::FixtureDef>],
) -> PyErr {
    // The request chain that led here: each fixture currently resolving (test
    // → call_fail → fail), outermost first, so the repr lists every requester's
    // def line. Falls back to the test function for a directly requested name.
    let chain: Vec<Py<PyAny>> = if stack.is_empty() {
        crate::runner::item_node(py, item)
            .and_then(|node| Ok(node.bind(py).getattr("function")?.unbind()))
            .map(|f| vec![f])
            .unwrap_or_else(|_| vec![py.None()])
    } else {
        stack.iter().map(|d| d.func.clone_ref(py)).collect()
    };
    // Fixture names visible to this item, de-duplicated and sorted.
    let mut names: Vec<String> = session
        .registry
        .all_defs()
        .filter(|d| session.registry.lookup(&d.name, &item.nodeid).is_some())
        .map(|d| d.name.clone())
        .collect();
    names.sort();
    names.dedup();
    let result = (|| {
        let avail = pyo3::types::PyList::new(py, &names)?;
        let funcs = pyo3::types::PyList::new(py, &chain)?;
        let exc = py
            .import("_pytest.fixtures")?
            .getattr("fixture_lookup_error")?
            .call1((name, funcs, avail))?;
        Ok::<_, PyErr>(PyErr::from_value(exc))
    })();
    result.unwrap_or_else(|_| {
        pyo3::exceptions::PyLookupError::new_err(format!("fixture '{name}' not found"))
    })
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

    // An invalid declared scope (e.g. scope="functions") fails when the fixture
    // is requested, with pytest's "unexpected scope value" message.
    if let Some(msg) = &def.scope_error {
        return Err(fail_no_trace(py, msg));
    }

    // A dynamic scope (`scope=<callable>`): evaluate the callable now — the
    // config it is passed is available here — to get the effective scope, like
    // pytest's _eval_scope_callable + Scope.from_user. A bad return value is
    // the same "unexpected scope value" failure as a bad literal scope.
    let scope = match &def.scope_callable {
        None => def.scope,
        Some(callable) => {
            let py_config = python::make_py_config(py, config)?;
            let result: String = py
                .import("pytest._fixtures")?
                .getattr("eval_scope_callable")?
                .call1((callable.bind(py), def.name.as_str(), py_config))?
                .extract()?;
            match Scope::parse(&result) {
                Some(scope) => scope,
                None => {
                    let where_ = def.baseid.trim_end_matches("::");
                    let from = if where_.is_empty() {
                        String::new()
                    } else {
                        format!("from {where_} ")
                    };
                    return Err(fail_no_trace(
                        py,
                        &format!(
                            "Fixture '{}' {from}got an unexpected scope value '{result}'",
                            def.name
                        ),
                    ));
                }
            }
        }
    };

    // ScopeMismatch: a fixture must not request a narrower-scoped fixture than
    // its own (pytest's FixtureRequest._check_scope). The requesting fixture is
    // the one whose dependencies we are resolving — the top of the stack. The
    // check precedes the cache lookup because pytest reports the mismatch even
    // when the narrower fixture's value is already cached. When resolving via
    // request.getfixturevalue() the stack starts empty, so fall back to the
    // fixture currently executing on this thread (the one that called it).
    let requesting: Vec<std::sync::Arc<crate::fixture::FixtureDef>> = if stack.is_empty() {
        EXECUTING.with(|s| s.borrow().clone())
    } else {
        stack.to_vec()
    };
    if let Some(parent) = requesting.last()
        && parent.scope > scope
    {
        return Err(scope_mismatch_error(py, config, &requesting, &def));
    }

    // Parametrized fixtures cache per param index.
    let fixture_param: Option<(usize, Py<PyAny>)> = item
        .fixture_params
        .iter()
        .find(|(fixture, _, _)| fixture == &def.name)
        .map(|(_, index, value)| (*index, value.clone_ref(py)));
    // A parametrized fixture resolved without a bound param. The parametrize
    // machinery always binds a param for fixtures in a test's closure, so this
    // is only reachable via request.getfixturevalue() of a fixture the test
    // never parametrized. Report pytest's dedicated error.
    if def.params.is_some()
        && fixture_param.is_none()
        && !item.callspec.iter().any(|(param, _)| param == &def.name)
    {
        return Err(no_parameter_error(py, config, &def, &item.nodeid));
    }
    let instance = match scope {
        Scope::Function => item.nodeid.clone(),
        Scope::Class => item.class_instance(),
        Scope::Module | Scope::Package => item.module_instance(),
        // Parametrized session-scope fixtures use a per-param instance key so
        // they can be torn down when the last test using that param finishes,
        // rather than batched at session end.  Non-parametrized session
        // fixtures still use the shared "" key (one instance per session).
        Scope::Session => match &fixture_param {
            Some((idx, _)) => format!("\x00session_param:{}:{}", def.name, idx),
            None => String::new(),
        },
    };
    // Non-function-scope parametrizations this fixture transitively depends
    // on. When one such param moves to its next value while its scope-instance
    // stays the same (e.g. a class `params=` fixture between class param sets),
    // every fixture carrying that binding is torn down and evicted before the
    // next value is set up — mirroring pytest's per-FixtureDef finish on a
    // differently-parametrized cached value.
    let bindings: Vec<crate::session::Binding> = {
        let closure = session.registry.transitive_argnames(&item.nodeid, &def);
        item.scope_sort_keys
            .iter()
            .filter(|(argname, scope, _)| {
                !matches!(scope, Scope::Session) && closure.contains(argname)
            })
            .map(|(argname, scope, idx)| (*scope, item.instance_at(*scope), argname.clone(), *idx))
            .collect()
    };
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
        scope,
        keyed_name,
        def.baseid.clone(),
        instance.clone(),
        fixture_param.as_ref().map(|(index, _)| *index),
    );
    if let Some(cached) = session.fixture_cache.get(&cache_key) {
        // A cached setup failure re-raises (pytest re-raises the cached
        // exception) — the fixture body is not run again for this scope. The
        // traceback is reset to the one captured at the original raise so it
        // doesn't accumulate frames across sibling items (#12204).
        if let Some(err) = &cached.error {
            let exc = err.bind(py).clone();
            if let Some(tb) = &cached.error_tb {
                let _ = exc.setattr("__traceback__", tb.bind(py));
            }
            return Err(PyErr::from_value(exc));
        }
        return Ok(cached.value.clone_ref(py));
    }

    stack.push(def.clone());
    let mut request: Option<Py<crate::request::PyRequest>> = None;
    let deps_result = (|| -> PyResult<Vec<(String, Py<PyAny>)>> {
        let mut kwargs = Vec::new();
        for dep in &def.param_names {
            if dep == "request" {
                let node = item_node(py, item)?;
                let req = Py::new(
                    py,
                    crate::request::PyRequest::new(
                        fixture_param.as_ref().map(|(_, value)| value.clone_ref(py)),
                        node,
                        Some(def.name.clone()),
                        scope,
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
    // Track this def as executing so a fixture body calling
    // request.getfixturevalue(<own name>) resolves to the overridden super.
    EXECUTING.with(|s| s.borrow_mut().push(def.clone()));
    let call_result: PyResult<(Py<PyAny>, Option<Finalizer>)> = if config.get_flag("setup-plan") {
        // --setup-plan: resolve the dependency graph for narration but do not
        // execute any fixture functions (upstream pytest behaviour).
        Ok((py.None().into_pyobject(py)?.unbind(), None))
    } else {
        match claimed {
            Some(fixture_value) => Ok((fixture_value.value, fixture_value.finalizer)),
            None => (|| {
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
                    Ok((value.unbind(), None))
                } else if def.is_generator {
                    let generator = python::call_fixture(py, &def.func, fixture_instance, &kwargs)?;
                    let value = python::next_value(py, &generator)?;
                    Ok((value.unbind(), Some(Finalizer::GenNext(generator.unbind()))))
                } else {
                    let value = python::call_fixture(py, &def.func, fixture_instance, &kwargs)?;
                    Ok((value.unbind(), None))
                }
            })(),
        }
    };
    EXECUTING.with(|s| {
        s.borrow_mut().pop();
    });
    let (value, finalizer) = match call_result {
        Ok(value_finalizer) => value_finalizer,
        Err(err) => {
            // A setup failure is cached so sibling items in this scope re-raise
            // it without re-running the body (pytest's cached_result). Any
            // finalizers the fixture registered via request.addfinalizer before
            // raising must still run at scope teardown (pytest schedules the
            // finalizer in a `finally`), so drain them once here.
            if let Some(request) = &request
                && let Ok(drainer) = request.bind(py).as_any().getattr("_drain_finalizers")
            {
                session.finalizers.push(PendingFinalizer {
                    scope: def.scope,
                    instance: instance.clone(),
                    finalizer: Finalizer::Callable(drainer.unbind()),
                    bindings: bindings.clone(),
                });
            }
            let exc = err.value(py).clone().into_any().unbind();
            let exc_tb = err.traceback(py).map(|tb| tb.into_any().unbind());
            session.fixture_cache.insert(
                cache_key,
                crate::session::CachedFixture {
                    value: py.None(),
                    error: Some(exc),
                    error_tb: exc_tb,
                    bindings,
                },
            );
            return Err(err);
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
                bindings: bindings.clone(),
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
            bindings: bindings.clone(),
        });
    }
    if let Some(finalizer) = finalizer {
        session.finalizers.push(PendingFinalizer {
            scope: def.scope,
            instance: instance.clone(),
            finalizer,
            bindings: bindings.clone(),
        });
    }
    // Conftest pytest_fixture_setup / pytest_fixture_post_finalizer hooks.
    fire_fixture_lifecycle_hooks(py, session, item, &def, scope, &instance, &bindings)?;
    session.fixture_cache.insert(
        cache_key,
        crate::session::CachedFixture {
            value: value.clone_ref(py),
            error: None,
            error_tb: None,
            bindings,
        },
    );
    Ok(value)
}
