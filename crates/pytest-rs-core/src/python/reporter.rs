//! Replacement terminalreporter delegation (sugar/pretty).

#[allow(unused_imports)]
use super::*;
use crate::collect::TestItem;
use crate::report::{Outcome, Phase};
use pyo3::types::PyDict;

/// Register the default 'terminalreporter' plugin (the stand-in that
/// reporter-replacing plugins like pytest-sugar unregister). Must run
/// before python pytest_configure hooks fire.
pub fn reporter_setup(py: Python<'_>, config: &crate::config::Config) -> PyResult<()> {
    let config_proxy = make_py_config(py, config)?;
    py.import("pytest._reporter")?
        .getattr("setup")?
        .call1((config_proxy,))?;
    Ok(())
}

/// The plugin object that replaced 'terminalreporter' during configure,
/// or None when terminal output stays native.
pub fn reporter_replacement(py: Python<'_>) -> Option<Py<PyAny>> {
    let result = py
        .import("pytest._reporter")
        .and_then(|m| m.getattr("replacement"))
        .and_then(|f| f.call0());
    match result {
        Ok(reporter) if !reporter.is_none() => Some(reporter.unbind()),
        _ => None,
    }
}

/// Drive the replacement reporter's pytest_sessionstart (it owns the
/// session header in delegated mode).
pub fn reporter_sessionstart(py: Python<'_>, config: &crate::config::Config) {
    let result = (|| -> PyResult<()> {
        let session = make_session_proxy(py, config)?;
        py.import("pytest._reporter")?
            .getattr("sessionstart")?
            .call1((session,))?;
        Ok(())
    })();
    if let Err(err) = result {
        eprintln!("INTERNAL ERROR: {}", format_exception(py, &err));
    }
}

/// Drive the replacement reporter's pytest_collection_finish ("collected N
/// items" line). `numcollected` includes deselected items.
pub fn reporter_collection_finish(
    py: Python<'_>,
    config: &crate::config::Config,
    numcollected: usize,
) {
    let result = (|| -> PyResult<()> {
        let session = make_session_proxy(py, config)?;
        py.import("pytest._reporter")?
            .getattr("collection_finish")?
            .call1((session, numcollected))?;
        Ok(())
    })();
    if let Err(err) = result {
        eprintln!("INTERNAL ERROR: {}", format_exception(py, &err));
    }
}

/// Drive the replacement reporter's pytest_deselected (stats bookkeeping
/// behind the "X deselected" counts).
pub fn reporter_deselected(py: Python<'_>, items: &Bound<'_, PyAny>) {
    let result = (|| -> PyResult<()> {
        py.import("pytest._reporter")?
            .getattr("deselected")?
            .call1((items,))?;
        Ok(())
    })();
    if let Err(err) = result {
        eprintln!("INTERNAL ERROR: {}", format_exception(py, &err));
    }
}

/// The (file, lineno0, domain) location tuple for an item, as upstream
/// reports it to pytest_runtest_logstart/logfinish.
pub(crate) fn item_location<'py>(py: Python<'py>, item: &TestItem) -> PyResult<Bound<'py, PyAny>> {
    let file = item.nodeid.split("::").next().unwrap_or("");
    let domain = item
        .nodeid
        .split_once("::")
        .map(|(_, rest)| rest.replace("::", "."))
        .unwrap_or_default();
    Ok((file, item.lineno.saturating_sub(1), domain)
        .into_pyobject(py)?
        .into_any())
}

/// Drive the replacement reporter's pytest_runtest_logstart.
pub fn reporter_logstart(py: Python<'_>, item: &TestItem) {
    let result = (|| -> PyResult<()> {
        let location = item_location(py, item)?;
        py.import("pytest._reporter")?
            .getattr("logstart")?
            .call1((item.nodeid.as_str(), location))?;
        Ok(())
    })();
    if let Err(err) = result {
        eprintln!("INTERNAL ERROR: {}", format_exception(py, &err));
    }
}

/// Drive the replacement reporter's pytest_runtest_logreport.
pub fn reporter_logreport(py: Python<'_>, report: &Bound<'_, PyAny>) {
    let result = (|| -> PyResult<()> {
        py.import("pytest._reporter")?
            .getattr("logreport")?
            .call1((report,))?;
        Ok(())
    })();
    if let Err(err) = result {
        eprintln!("INTERNAL ERROR: {}", format_exception(py, &err));
    }
}

/// Drive the replacement reporter's pytest_runtest_logfinish.
pub fn reporter_logfinish(py: Python<'_>, item: &TestItem) {
    let result = (|| -> PyResult<()> {
        let location = item_location(py, item)?;
        py.import("pytest._reporter")?
            .getattr("logfinish")?
            .call1((item.nodeid.as_str(), location))?;
        Ok(())
    })();
    if let Err(err) = result {
        eprintln!("INTERNAL ERROR: {}", format_exception(py, &err));
    }
}

/// Feed a collection error to the replacement reporter as a failed
/// CollectReport (sugar prints these instantly; the base records stats).
pub fn reporter_collect_error(py: Python<'_>, nodeid: &str, longrepr: &str) {
    let result = (|| -> PyResult<()> {
        let kwargs = PyDict::new(py);
        kwargs.set_item("nodeid", nodeid)?;
        kwargs.set_item("outcome", "failed")?;
        kwargs.set_item("longrepr", longrepr)?;
        let file = nodeid.split("::").next().unwrap_or(nodeid);
        kwargs.set_item("location", (file, py.None(), file))?;
        kwargs.set_item("result", pyo3::types::PyList::empty(py))?;
        kwargs.set_item("sections", pyo3::types::PyList::empty(py))?;
        let report = py
            .import("_pytest.reports")?
            .getattr("CollectReport")?
            .call((), Some(&kwargs))?;
        py.import("pytest._reporter")?
            .getattr("collectreport")?
            .call1((report,))?;
        Ok(())
    })();
    if let Err(err) = result {
        eprintln!("INTERNAL ERROR: {}", format_exception(py, &err));
    }
}

/// End-of-run summaries through the replacement reporter (upstream's
/// pytest_terminal_summary / sessionfinish wrapper order).
pub fn reporter_finish(
    py: Python<'_>,
    config: &crate::config::Config,
    exitstatus: i32,
    shouldfail: Option<&str>,
) {
    let result = (|| -> PyResult<()> {
        let session = make_session_proxy(py, config)?;
        py.import("pytest._reporter")?
            .getattr("finish")?
            .call1((session, exitstatus, shouldfail))?;
        Ok(())
    })();
    if let Err(err) = result {
        eprintln!("INTERNAL ERROR: {}", format_exception(py, &err));
    }
}

/// Build the Python TestReport proxy handed to pytest_runtest_logreport
/// conftest hooks: a _pytest.reports.TestReport shim instance, whose
/// passed/failed/skipped flags and capstdout/capstderr/caplog properties
/// derive from the outcome and sections set here.
pub fn make_report_proxy(
    py: Python<'_>,
    report: &crate::report::TestReport,
    lineno: Option<u32>,
) -> PyResult<Py<PyAny>> {
    let (outcome, wasxfail) = match report.outcome {
        Outcome::Passed => ("passed", None),
        Outcome::Failed => ("failed", None),
        Outcome::Skipped => ("skipped", None),
        // pytest: expected failures are skipped/passed reports + .wasxfail.
        Outcome::XFailed => ("skipped", Some(report.longrepr.clone().unwrap_or_default())),
        Outcome::XPassed => ("passed", Some(String::new())),
    };
    let kwargs = PyDict::new(py);
    kwargs.set_item("nodeid", &report.nodeid)?;
    kwargs.set_item(
        "when",
        match report.phase {
            Phase::Setup => "setup",
            Phase::Call => "call",
            Phase::Teardown => "teardown",
        },
    )?;
    kwargs.set_item("outcome", outcome)?;
    kwargs.set_item("duration", report.duration.as_secs_f64())?;
    kwargs.set_item("longrepr", report.longrepr.as_deref())?;
    kwargs.set_item("sections", &report.sections)?;
    let file = report.nodeid.split("::").next().unwrap_or("");
    let domain = report
        .nodeid
        .split_once("::")
        .map(|(_, rest)| rest.replace("::", "."))
        .unwrap_or_default();
    kwargs.set_item(
        "location",
        (file, lineno.map(|l| l.saturating_sub(1)), domain),
    )?;
    kwargs.set_item("keywords", PyDict::new(py))?;
    kwargs.set_item("user_properties", pyo3::types::PyList::empty(py))?;
    if let Some(reason) = wasxfail {
        kwargs.set_item("wasxfail", reason)?;
    }
    let cls = py.import("_pytest.reports")?.getattr("TestReport")?;
    Ok(cls.call((), Some(&kwargs))?.unbind())
}
