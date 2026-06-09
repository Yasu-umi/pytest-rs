//! Delegating `pytest_runtest_protocol` to a plain (non-wrapper) Python hook
//! impl that replaces the protocol — pytest-rerunfailures' rerun loop. The
//! plugin calls `_pytest.runner.runtestprotocol(item, log=False)` to run the
//! phases (re-entering `run_one_body` here), mutates report outcomes (e.g.
//! "rerun"), and drives `item.ihook.pytest_runtest_logreport`; those reports
//! are captured and returned as the item's reports so the engine renders and
//! counts them normally.

#[allow(unused_imports)]
use super::*;
use crate::collect::TestItem;
use crate::config::Config;
use crate::hooks::Plugin;
use crate::report::{Outcome, Phase, TestReport};
use crate::session::Session;

/// Raw pointers to the engine state of the delegated item, so the
/// Python-exposed `runtestprotocol` can re-enter `run_one_body`. Same safety
/// model as `ResolveCtx`: dereferenced only while the suspended `run_one`
/// frame (which pushed them) is on this thread's stack inside a Python call.
struct ProtocolCtx {
    plugins: *const [Box<dyn Plugin>],
    session: *mut Session,
    config: *const Config,
    item: *const TestItem,
}

thread_local! {
    static PROTOCOL_CTX: std::cell::RefCell<Vec<ProtocolCtx>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Reports the delegated protocol logged via ihook.pytest_runtest_logreport,
    /// one buffer per active delegation (nestable).
    static CAPTURE: std::cell::RefCell<Vec<Vec<TestReport>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// True while a delegated protocol is driving this item (gates the capture
/// sink so it is a no-op during normal runs).
pub(crate) fn capture_active() -> bool {
    CAPTURE.with(|s| !s.borrow().is_empty())
}

/// `_pytest.runner.runtestprotocol` re-entry: run the current item's
/// setup/call/teardown once and return the reports as `_pytest.reports`
/// proxies (the plugin logs them itself).
#[allow(unsafe_code)]
pub(crate) fn run_item_phases(py: Python<'_>) -> PyResult<Py<PyAny>> {
    let ptrs = PROTOCOL_CTX.with(|stack| {
        stack
            .borrow()
            .last()
            .map(|c| (c.plugins, c.session, c.config, c.item))
    });
    let Some((plugins, session, config, item)) = ptrs else {
        return Err(pyo3::exceptions::PyRuntimeError::new_err(
            "runtestprotocol() is only available while a delegated protocol runs",
        ));
    };
    // Safety: pushed by the run_one frame suspended below us in the delegated
    // hook call; it does not touch the session while Python runs.
    let (plugins, session, config, item) = unsafe { (&*plugins, &mut *session, &*config, &*item) };
    let reports = super::run_one_body(py, plugins, session, config, item);
    let proxies = pyo3::types::PyList::empty(py);
    for report in &reports {
        let lineno = (report.phase == Phase::Call).then_some(item.lineno);
        proxies.append(crate::python::make_report_proxy(py, report, lineno)?)?;
    }
    Ok(proxies.unbind().into_any())
}

/// The capture sink registered in the shim pluginmanager: when a delegated
/// protocol is active, record each report the plugin logs.
pub(crate) fn capture_logreport(py: Python<'_>, report: &Bound<'_, PyAny>) -> PyResult<()> {
    if !capture_active() {
        return Ok(());
    }
    let converted = report_from_proxy(py, report)?;
    CAPTURE.with(|stack| {
        if let Some(buf) = stack.borrow_mut().last_mut() {
            buf.push(converted);
        }
    });
    Ok(())
}

/// `_pytest.reports.TestReport` proxy -> Rust `TestReport`. A "rerun" outcome
/// (set by pytest-rerunfailures) maps to a failed report flagged `rerun`.
fn report_from_proxy(py: Python<'_>, report: &Bound<'_, PyAny>) -> PyResult<TestReport> {
    let nodeid: String = report.getattr("nodeid")?.extract()?;
    let when: String = report
        .getattr("when")
        .and_then(|w| w.extract())
        .unwrap_or_else(|_| "call".to_string());
    let outcome_str: String = report.getattr("outcome")?.extract()?;
    let phase = match when.as_str() {
        "setup" => Phase::Setup,
        "teardown" => Phase::Teardown,
        _ => Phase::Call,
    };
    let wasxfail = report.getattr("wasxfail").ok().filter(|v| !v.is_none());
    let (outcome, rerun) = match outcome_str.as_str() {
        "rerun" => (Outcome::Failed, true),
        "failed" => (Outcome::Failed, false),
        "skipped" if wasxfail.is_some() => (Outcome::XFailed, false),
        "skipped" => (Outcome::Skipped, false),
        "passed" if wasxfail.is_some() => (Outcome::XPassed, false),
        _ => (Outcome::Passed, false),
    };
    let longrepr = report
        .getattr("longrepr")
        .ok()
        .filter(|v| !v.is_none())
        .map(|v| v.str().map(|s| s.to_string()))
        .transpose()?;
    let duration = report
        .getattr("duration")
        .and_then(|d| d.extract::<f64>())
        .map(std::time::Duration::from_secs_f64)
        .unwrap_or_default();
    let _ = py;
    Ok(TestReport {
        nodeid,
        phase,
        outcome,
        duration,
        longrepr,
        location: None,
        subtest_desc: None,
        sections: Vec::new(),
        rerun,
        xfail_longrepr: None,
    })
}

/// If a plain (non-generator) `pytest_runtest_protocol` hook visible to this
/// item handles it (returns non-None), drive it and return the reports it
/// logged. Returns None when no hook claims the item (run the native body).
pub(crate) fn delegate_protocol(
    py: Python<'_>,
    plugins: &[Box<dyn Plugin>],
    session: &mut Session,
    config: &Config,
    item: &TestItem,
    nextitem: Option<&TestItem>,
) -> PyResult<Option<Vec<TestReport>>> {
    let isgenfunc = py.import("inspect")?.getattr("isgeneratorfunction")?;
    let hook_funcs: Vec<Py<PyAny>> = session
        .py_hooks
        .iter()
        .filter(|hook| {
            hook.name == "pytest_runtest_protocol" && item.nodeid.starts_with(hook.baseid.as_str())
        })
        .map(|hook| hook.func.clone_ref(py))
        .filter(|func| {
            !isgenfunc
                .call1((func.bind(py),))
                .and_then(|v| v.extract::<bool>())
                .unwrap_or(false)
        })
        .collect();
    if hook_funcs.is_empty() {
        return Ok(None);
    }

    let node = crate::python::make_node(py, item)?;
    let nextnode = match nextitem {
        Some(next) => crate::python::make_node(py, next)?,
        None => py.None(),
    };

    // Publish the engine state for the re-entrant runtestprotocol, and a
    // fresh capture buffer for the reports the plugin logs.
    PROTOCOL_CTX.with(|stack| {
        stack.borrow_mut().push(ProtocolCtx {
            plugins,
            session,
            config,
            item,
        });
    });
    CAPTURE.with(|stack| stack.borrow_mut().push(Vec::new()));

    let mut handled = false;
    let mut error = None;
    for func in &hook_funcs {
        match crate::python::call_py_hook_raw(
            py,
            func,
            &[
                ("item", node.clone_ref(py)),
                ("nextitem", nextnode.clone_ref(py)),
            ],
        ) {
            Ok(result) => {
                if !result.bind(py).is_none() {
                    handled = true;
                    break;
                }
            }
            Err(err) => {
                error = Some(err);
                break;
            }
        }
    }

    PROTOCOL_CTX.with(|stack| {
        stack.borrow_mut().pop();
    });
    let captured = CAPTURE
        .with(|stack| stack.borrow_mut().pop())
        .unwrap_or_default();

    if let Some(err) = error {
        return Err(err);
    }
    // The shim TerminalReporter (driven by the plugin's ihook.logreport)
    // already rendered these reports; tell run_items to count without
    // re-rendering.
    session.delegated_render = handled;
    Ok(handled.then_some(captured))
}
