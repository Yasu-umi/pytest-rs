//! Per-item runtest hook firing (logreport, setup/call/teardown wrappers).

#[allow(unused_imports)]
use super::*;
use crate::collect::TestItem;
use crate::python;
use crate::report::TestReport;
use crate::session::Session;

/// Fire conftest pytest_runtest_{setup,call,teardown} hooks for an item
/// (visibility-scoped by the conftest's directory, item kwarg).
/// pytest_runtest_logreport conftest hooks, fired once per report as it is
/// produced (pytest streams reports through this hook the same way).
pub(crate) fn fire_logreport_hooks(
    py: Python<'_>,
    session: &Session,
    report: &TestReport,
    lineno: Option<u32>,
    item: Option<&TestItem>,
    protocol_delegated: bool,
) {
    // Conftest impls only: a third-party plugin module's impl (entry point
    // or `-p`, tagged with plugin_module) is dispatched through the shim
    // pluginmanager by reporter_logreport below — taking it here too called
    // it twice per report.
    let funcs: Vec<Py<PyAny>> = session
        .py_hooks
        .iter()
        .filter(|hook| {
            hook.name == "pytest_runtest_logreport"
                && hook.plugin_module.is_none()
                && report.nodeid.starts_with(hook.baseid.as_str())
        })
        .map(|hook| hook.func.clone_ref(py))
        .collect();
    let delegated = session.custom_reporter.is_some();
    // The report pytest_runtest_makereport returned for this phase (which
    // hookwrappers may have enriched, e.g. pytest-bdd's .scenario, or
    // replaced) is the one upstream hands to pytest_runtest_logreport.
    let proxy = match take_makereport_result(py, report, lineno) {
        Ok(proxy) => proxy,
        Err(err) => {
            eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
            return;
        }
    };
    // Populate keywords from the item's marks so conftest pytest_terminal_summary
    // hooks (which receive this report via terminalreporter.stats) can inspect marks.
    if let Some(item) = item {
        let _ = (|| -> PyResult<()> {
            use pyo3::types::{PyAnyMethods, PyDict, PyDictMethods};
            let kw = PyDict::new(py);
            for mark in &item.marks {
                kw.set_item(&mark.name, mark.obj.bind(py))?;
            }
            for (name, obj) in super::marks::added_marks(py) {
                kw.set_item(&name, obj.bind(py))?;
            }
            proxy.bind(py).setattr("keywords", kw)?;
            Ok(())
        })();
    }
    python::record_hook(
        py,
        "pytest_runtest_logreport",
        &[("report", proxy.clone_ref(py))],
    );
    for func in &funcs {
        if let Err(err) = python::call_py_hook(py, func, &[("report", proxy.clone_ref(py))]) {
            python::defer_or_print_internal_error(py, &err);
        }
    }
    // When the protocol was delegated (e.g. pytest-rerunfailures), the plugin
    // already called ihook.pytest_runtest_logreport which dispatched to the
    // shim PM's reporters; driving them again would double-count stats.
    if !protocol_delegated {
        python::reporter_logreport(py, proxy.bind(py));
        if !delegated {
            python::reporter_feed_default(py, proxy.bind(py));
        }
    }
}

thread_local! {
    /// The exception each phase of the running item raised, recorded where
    /// the engine turns it into a report: `pytest_runtest_makereport`'s
    /// `call.excinfo` (pytest-rerunfailures matches `--only-rerun` against it).
    static PHASE_EXCEPTIONS: std::cell::RefCell<Vec<(String, Phase, Py<PyAny>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// The reports `pytest_runtest_makereport` returned, in phase order, until
    /// `pytest_runtest_logreport` (or a delegated `runtestprotocol`) takes them.
    static MAKEREPORT_RESULTS: std::cell::RefCell<Vec<(String, Phase, Py<PyAny>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Forget the previous item's recorded phase exceptions and unlogged
/// makereport results (called as an item's protocol starts).
pub(crate) fn reset_makereport_state() {
    PHASE_EXCEPTIONS.with(|cell| cell.borrow_mut().clear());
    MAKEREPORT_RESULTS.with(|cell| cell.borrow_mut().clear());
}

/// Record the exception a phase of `nodeid` raised, for that phase's
/// `pytest_runtest_makereport` call info. The last one of a phase wins.
pub(crate) fn record_phase_exception(py: Python<'_>, nodeid: &str, phase: Phase, err: &PyErr) {
    let value = err.value(py).clone().into_any().unbind();
    PHASE_EXCEPTIONS.with(|cell| {
        let mut entries = cell.borrow_mut();
        entries.retain(|(id, p, _)| !(id == nodeid && *p == phase));
        entries.push((nodeid.to_string(), phase, value));
    });
}

/// The exception recorded for `nodeid`'s `phase`, left in place for that
/// phase's makereport.
pub(crate) fn peek_phase_exception(py: Python<'_>, nodeid: &str, phase: Phase) -> Option<PyErr> {
    PHASE_EXCEPTIONS.with(|cell| {
        cell.borrow()
            .iter()
            .find(|(id, p, _)| id == nodeid && *p == phase)
            .map(|(_, _, value)| PyErr::from_value(value.bind(py).clone()))
    })
}

/// Forget the exception recorded for `nodeid`'s `phase` (a rerun attempt
/// starting that phase afresh).
pub(crate) fn clear_phase_exception(nodeid: &str, phase: Phase) {
    PHASE_EXCEPTIONS.with(|cell| {
        cell.borrow_mut()
            .retain(|(id, p, _)| !(id == nodeid && *p == phase));
    });
}

fn phase_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Setup => "setup",
        Phase::Call => "call",
        Phase::Teardown => "teardown",
    }
}

/// Fire `pytest_runtest_makereport(item, call)` for each report in
/// `reports[*fired..]`, as the phase that produced it finishes (upstream's
/// `call_and_report`), and advance `fired`. Plugins see every phase, setup
/// included, before the next one runs — pytest-rerunfailures creates its
/// per-attempt bookkeeping on the item from the setup report. The hook's
/// result is kept for the phase's `pytest_runtest_logreport`, and an outcome
/// a hook changed is carried back into the engine's report. A no-op unless a
/// conftest or plugin registered the hook for this item.
pub(crate) fn fire_makereport(
    py: Python<'_>,
    session: &Session,
    item: &TestItem,
    reports: &mut [TestReport],
    fired: &mut usize,
) {
    let start = (*fired).min(reports.len());
    *fired = reports.len();
    let hooked = session.py_hooks.iter().any(|h| {
        h.name == "pytest_runtest_makereport" && item.nodeid.starts_with(h.baseid.as_str())
    });
    if !hooked {
        return;
    }
    for report in &mut reports[start..] {
        // Subtest reports come from pytest-subtests' own makereport call.
        if report.subtest_desc.is_some() || report.nodeid != item.nodeid {
            continue;
        }
        let exc = PHASE_EXCEPTIONS.with(|cell| {
            let mut entries = cell.borrow_mut();
            entries
                .iter()
                .position(|(id, p, _)| id == &report.nodeid && *p == report.phase)
                .map(|pos| entries.remove(pos).2)
        });
        let result = (|| -> PyResult<Py<PyAny>> {
            let proxy = python::make_report_proxy(py, report, Some(item.lineno))?;
            let node = super::item_node(py, item)?;
            let result = py
                .import("pytest._reporter")?
                .getattr("run_makereport")?
                .call1((
                    proxy.bind(py),
                    node,
                    phase_name(report.phase),
                    exc.as_ref().map_or_else(|| py.None(), |e| e.clone_ref(py)),
                ))?;
            Ok(result.unbind())
        })();
        match result {
            Ok(result) => {
                if let Ok(updated) = super::report_from_proxy(py, result.bind(py))
                    && (updated.outcome != report.outcome || updated.rerun != report.rerun)
                {
                    report.outcome = updated.outcome;
                    report.rerun = updated.rerun;
                    if updated.longrepr.is_some() {
                        report.longrepr = updated.longrepr;
                        report.reprcrash_message = updated.reprcrash_message;
                    }
                }
                MAKEREPORT_RESULTS.with(|cell| {
                    cell.borrow_mut()
                        .push((report.nodeid.clone(), report.phase, result));
                });
            }
            Err(err) => python::defer_or_print_internal_error(py, &err),
        }
    }
}

/// The `pytest_runtest_makereport` result recorded for `report`'s phase, or a
/// fresh report proxy when no hook produced one. A teardown whose outcome
/// changed afterwards (a deferred scope teardown failed into it) carries the
/// engine's final outcome.
pub(crate) fn take_makereport_result(
    py: Python<'_>,
    report: &TestReport,
    lineno: Option<u32>,
) -> PyResult<Py<PyAny>> {
    let cached = MAKEREPORT_RESULTS.with(|cell| {
        let mut entries = cell.borrow_mut();
        entries
            .iter()
            .position(|(id, p, _)| id == &report.nodeid && *p == report.phase)
            .map(|pos| entries.remove(pos).2)
    });
    let fresh = || python::make_report_proxy(py, report, lineno);
    let Some(cached) = cached else {
        return fresh();
    };
    let synced = super::report_from_proxy(py, cached.bind(py))
        .map(|r| r.outcome == report.outcome)
        .unwrap_or(false);
    if !synced {
        let fresh = fresh()?;
        for attr in ["outcome", "longrepr", "sections", "wasxfail"] {
            if let Ok(value) = fresh.bind(py).getattr(attr) {
                cached.bind(py).setattr(attr, value)?;
            } else if attr == "wasxfail" {
                let _ = cached.bind(py).delattr(attr);
            }
        }
    }
    Ok(cached)
}

/// A `pytest_report_teststatus` hook result: the verbose word and any
/// explicit markup codes the hook attached to it (a `(word, {name: True})`
/// tuple, upstream). The category/letter members of the triple are parsed
/// for shape but not yet consumed (stats stay outcome-driven here).
pub(crate) struct TestStatus {
    pub word: String,
    pub markup: Option<Vec<u8>>,
}

/// Resolve a report through the registered `pytest_report_teststatus`
/// conftest/plugin hooks (firstresult: first non-None wins, pluggy order).
/// Returns None when no hook claims the report, so the caller falls back
/// to the built-in outcome word/color.
pub(crate) fn report_teststatus(
    py: Python<'_>,
    config: &crate::config::Config,
    session: &Session,
    report: &TestReport,
    lineno: Option<u32>,
) -> Option<TestStatus> {
    let mut funcs: Vec<Py<PyAny>> = session
        .py_hooks
        .iter()
        .filter(|hook| {
            hook.name == "pytest_report_teststatus"
                && report.nodeid.starts_with(hook.baseid.as_str())
        })
        .map(|hook| hook.func.clone_ref(py))
        .collect();
    funcs.extend(python::instance_hook_funcs(py, "pytest_report_teststatus"));
    if funcs.is_empty() {
        return None;
    }
    let proxy = python::make_report_proxy(py, report, lineno).ok()?;
    let config_proxy = python::make_py_config(py, config).ok()?;
    for func in funcs {
        let result = match python::call_py_hook(
            py,
            &func,
            &[
                ("report", proxy.clone_ref(py)),
                ("config", config_proxy.clone_ref(py)),
            ],
        ) {
            Ok(result) => result,
            Err(err) => {
                python::defer_or_print_internal_error(py, &err);
                continue;
            }
        };
        let bound = result.bind(py);
        if bound.is_none() {
            continue;
        }
        if let Some(status) = TestStatus::from_py(bound) {
            return Some(status);
        }
    }
    None
}

impl TestStatus {
    /// Parse a `(category, letter, word)` triple, where `word` may itself
    /// be a `(word, markup_dict)` tuple carrying explicit color markup.
    fn from_py(value: &Bound<'_, PyAny>) -> Option<TestStatus> {
        let _category: String = value.get_item(0).ok()?.extract().ok()?;
        let _letter: String = value.get_item(1).ok()?.extract().ok()?;
        let word_item = value.get_item(2).ok()?;
        let (word, markup) = if let Ok((word, markup)) =
            word_item.extract::<(String, Bound<'_, pyo3::types::PyDict>)>()
        {
            let codes: Vec<u8> = markup
                .iter()
                .filter_map(|(k, v)| {
                    let name: String = k.extract().ok()?;
                    let on: bool = v.extract().unwrap_or(false);
                    on.then(|| crate::tw::markup_code(&name)).flatten()
                })
                .collect();
            (word, Some(codes))
        } else {
            (word_item.extract::<String>().ok()?, None)
        };
        Some(TestStatus { word, markup })
    }
}

/// Fire conftest/plugin `pytest_pyfunc_call(pyfuncitem)` hooks (firstresult)
/// before the native call. A truthy return means a plugin ran the test, so
/// the engine skips its native invocation; None (e.g. a logging-only hook)
/// falls through. The pyfuncitem exposes funcargs/obj like upstream.
pub(crate) fn fire_pyfunc_call_hooks(
    py: Python<'_>,
    session: &Session,
    config: &crate::config::Config,
    item: &TestItem,
    callable: &mut Py<PyAny>,
    kwargs: &[(String, Py<PyAny>)],
) -> PyResult<bool> {
    let mut funcs: Vec<Py<PyAny>> = session
        .py_hooks
        .iter()
        .filter(|hook| {
            hook.name == "pytest_pyfunc_call" && item.nodeid.starts_with(hook.baseid.as_str())
        })
        .map(|hook| hook.func.clone_ref(py))
        .collect();
    // Hooks registered on a plugin instance (e.g. --trace's PdbTrace, which
    // pytest._debugging.configure() registers via pm.register(...) rather
    // than as a module-level conftest/entry-point hook that session.py_hooks
    // scans for) — PdbTrace is the only known instance-registered
    // pytest_pyfunc_call impl and only ever gets registered when --trace is
    // passed, so gate the (Python-side, O(registered plugins)) lookup behind
    // that flag: this runs once per test item, and unconditionally scanning
    // every plugin here measurably slowed down large suites.
    if config.get_flag("trace") {
        funcs.extend(python::instance_hook_funcs(py, "pytest_pyfunc_call"));
    }
    if funcs.is_empty() {
        return Ok(false);
    }
    let node = super::item_node(py, item)?;
    // Setup already filled `funcargs` with the whole closure; make sure
    // the test's own arguments are there.
    let funcargs = match node
        .bind(py)
        .getattr("funcargs")
        .ok()
        .and_then(|existing| existing.cast_into::<pyo3::types::PyDict>().ok())
    {
        Some(existing) => existing,
        None => pyo3::types::PyDict::new(py),
    };
    for (name, value) in kwargs {
        funcargs.set_item(name, value.bind(py))?;
    }
    node.bind(py).setattr("funcargs", &funcargs)?;
    node.bind(py).setattr("obj", callable.bind(py))?;
    for func in funcs {
        let result = python::call_py_hook_raw(py, &func, &[("pyfuncitem", node.clone_ref(py))])?;
        if result.bind(py).is_truthy()? {
            return Ok(true);
        }
    }
    // A hook may not have claimed the call but still rebound pyfuncitem.obj
    // (e.g. --trace wraps it in a pdb.runcall closure) — the native call
    // below must use that wrapper, not the pre-hook callable.
    *callable = node.bind(py).getattr("obj")?.unbind();
    Ok(false)
}

/// Fire the conftest/plugin impls of a per-item runtest hook.
///
/// `nextitem` is only consulted for `pytest_runtest_teardown`, whose upstream
/// hookspec is `(item, nextitem)` — pass the item that runs after this one
/// (None when it is the last). Every other hook here ignores it.
pub(crate) fn fire_runtest_py_hooks(
    py: Python<'_>,
    session: &Session,
    item: &TestItem,
    nextitem: Option<&TestItem>,
    name: &str,
) -> PyResult<()> {
    let funcs: Vec<Py<PyAny>> = session
        .py_hooks
        .iter()
        .filter(|hook| hook.name == name && item.nodeid.starts_with(hook.baseid.as_str()))
        .map(|hook| hook.func.clone_ref(py))
        .collect();
    let recording = crate::engine::inprocess::recording();
    if funcs.is_empty() && !recording {
        return Ok(());
    }
    // logstart/logfinish hookspecs take (nodeid, location); setup takes
    // (item); teardown takes (item, nextitem). call_py_hook passes only what
    // each impl's signature requests, so providing the right available kwargs
    // per hook is enough — but an impl asking for a kwarg that isn't offered
    // raises TypeError, so teardown must offer `nextitem` even though nothing
    // in the engine itself needs it.
    let location_based = name == "pytest_runtest_logstart" || name == "pytest_runtest_logfinish";
    let mut kwargs: Vec<(&str, Py<PyAny>)> = if location_based {
        let location = python::item_location(py, item)?;
        vec![
            (
                "nodeid",
                item.nodeid.clone().into_pyobject(py)?.into_any().unbind(),
            ),
            ("location", location.unbind()),
        ]
    } else {
        let node = super::item_node(py, item)?;
        vec![("item", node)]
    };
    if name == "pytest_runtest_teardown" {
        let next_node = match nextitem {
            // Deliberately NOT `item_node`: that cache holds exactly one node,
            // the *running* item's, so that attributes a plugin sets during the
            // test (pytest-bdd's `__scenario_report__`) are still on the object
            // makereport sees. Asking it for the next item would evict the
            // running item's entry, and the next lookup would hand out a fresh
            // node with none of those attributes.
            Some(next) => python::make_node(py, next)?,
            None => py.None(),
        };
        kwargs.push(("nextitem", next_node));
    }
    for func in &funcs {
        python::call_py_hook(py, func, &kwargs)?;
    }
    if recording {
        python::record_hook(py, name, &kwargs);
    }
    Ok(())
}

/// Start `name` py hookwrappers around a phase: generator-function impls
/// (pluggy wrapper style, e.g. pytest-timeout's timer) advance to their
/// yield and are returned so the caller finishes them once the wrapped
/// phase is over. Plain impls either run immediately (`call_plain`, the
/// pytest_runtest_call behavior) or are skipped — a plain
/// pytest_runtest_protocol impl REPLACES the protocol upstream
/// (firstresult), which pytest-rs does not emulate; invoking it for side
/// effects would run foreign protocol code on top of ours.
pub(crate) fn start_runtest_py_wrappers(
    py: Python<'_>,
    session: &Session,
    item: &TestItem,
    nextitem: Option<&TestItem>,
    name: &str,
    call_plain: bool,
) -> PyResult<Vec<(Py<PyAny>, bool)>> {
    let funcs: Vec<Py<PyAny>> = session
        .py_hooks
        .iter()
        .filter(|hook| hook.name == name && item.nodeid.starts_with(hook.baseid.as_str()))
        .map(|hook| hook.func.clone_ref(py))
        .collect();
    // In a nested run, surface these hooks to the HookRecorder once per item
    // (fixtures tests read getcalls("pytest_runtest_call")[0].item; other
    // tests check that pytest_runtest_protocol itself was called).
    if matches!(name, "pytest_runtest_call" | "pytest_runtest_protocol")
        && crate::engine::inprocess::recording()
    {
        let node = super::item_node(py, item)?;
        python::record_hook(py, name, &[("item", node)]);
    }
    if funcs.is_empty() {
        return Ok(Vec::new());
    }
    let node = super::item_node(py, item)?;
    let mut kwargs = vec![("item", node)];
    // Upstream's pytest_runtest_protocol hookspec is (item, nextitem); an
    // impl declaring nextitem (logfire's pytest plugin) needs it passed.
    if name == "pytest_runtest_protocol" {
        let next_node = match nextitem {
            // make_node, not item_node: see fire_runtest_py_hooks' teardown
            // case for why the running item's cached node must survive.
            Some(next) => python::make_node(py, next)?,
            None => py.None(),
        };
        kwargs.push(("nextitem", next_node));
    }
    let next_fn = py.import("builtins")?.getattr("next")?;
    let isgenfunc = py.import("inspect")?.getattr("isgeneratorfunction")?;
    let mut wrappers = Vec::new();
    for func in funcs {
        if !isgenfunc.call1((func.bind(py),))?.extract::<bool>()? {
            if call_plain {
                python::call_py_hook(py, &func, &kwargs)?;
            }
            continue;
        }
        let result = python::call_py_hook_raw(py, &func, &kwargs)?;
        let bound = result.bind(py);
        match next_fn.call1((bound,)) {
            // Kept with the pluggy wrapper style for finish_runtest_py_wrappers.
            Ok(_) => wrappers.push((
                result.clone_ref(py),
                hookimpl_flag(py, &func, "hookwrapper"),
            )),
            Err(err) if err.is_instance_of::<pyo3::exceptions::PyStopIteration>(py) => {}
            Err(err) => return Err(err),
        }
    }
    Ok(wrappers)
}

/// A boolean flag of a hookimpl's `pytest_impl` options dict.
fn hookimpl_flag(py: Python<'_>, func: &Py<PyAny>, flag: &str) -> bool {
    func.bind(py)
        .getattr("pytest_impl")
        .ok()
        .and_then(|d| d.get_item(flag).ok())
        .and_then(|v| v.extract::<bool>().ok())
        .unwrap_or(false)
}

/// Finish started hookwrappers in reverse order (pluggy unwinds LIFO),
/// handing each the hook's outcome: `error` is the exception the wrapped
/// call raised, if any. A new-style wrapper (`wrapper=True`) gets it thrown
/// in at its `yield`; an old-style one (`hookwrapper=True`) gets a pluggy
/// `Result` it can inspect, re-raise (`get_result()`) or override. Returns
/// the exception that remains once every wrapper has run, or Ok when none
/// does (a wrapper swallowed it, or there was none).
pub(crate) fn finish_runtest_py_wrappers(
    py: Python<'_>,
    wrappers: &[(Py<PyAny>, bool)],
    error: Option<PyErr>,
) -> PyResult<()> {
    let mut error = error;
    let result_cls = py.import("pytest._pluginmanager")?.getattr("_Result")?;
    for (wrapper, old_style) in wrappers.iter().rev() {
        let wrapper = wrapper.bind(py);
        let step = if *old_style {
            let outcome = result_cls.call1((py.None(),))?;
            if let Some(err) = &error {
                outcome.call_method1("force_exception", (err.value(py),))?;
            }
            let step = wrapper.call_method1("send", (&outcome,));
            // The outcome as the wrapper left it (force_result /
            // force_exception) carries on to the next wrapper.
            error = outcome
                .getattr("exception")
                .ok()
                .filter(|exc| !exc.is_none())
                .map(PyErr::from_value);
            step
        } else {
            match error.take() {
                Some(err) => wrapper.call_method1("throw", (err.value(py),)),
                None => wrapper.call_method1("send", (py.None(),)),
            }
        };
        match step {
            Ok(_) => {
                return Err(pyo3::exceptions::PyRuntimeError::new_err(
                    "hook wrapper yielded more than once",
                ));
            }
            Err(err) if err.is_instance_of::<pyo3::exceptions::PyStopIteration>(py) => {}
            Err(err) => error = Some(err),
        }
    }
    match error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}
