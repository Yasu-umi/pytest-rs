//! Replacement terminalreporter delegation (sugar/pretty).

#[allow(unused_imports)]
use super::*;
use crate::collect::TestItem;
use crate::report::{Outcome, Phase};
use pyo3::types::PyDict;

static REPORTS_TEST_REPORT_CLS: pyo3::sync::PyOnceLock<Py<PyAny>> = pyo3::sync::PyOnceLock::new();
static REPORTER_LOGREPORT_FN: pyo3::sync::PyOnceLock<Py<PyAny>> = pyo3::sync::PyOnceLock::new();
static REPORTER_FEED_DEFAULT_FN: pyo3::sync::PyOnceLock<Py<PyAny>> = pyo3::sync::PyOnceLock::new();

fn reports_test_report_cls(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    REPORTS_TEST_REPORT_CLS
        .get_or_try_init(py, || {
            Ok(py
                .import("_pytest.reports")?
                .getattr("TestReport")?
                .unbind())
        })
        .map(|f| f.bind(py).clone())
}

fn reporter_logreport_fn(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    REPORTER_LOGREPORT_FN
        .get_or_try_init(py, || {
            Ok(py
                .import("pytest._reporter")?
                .getattr("logreport")?
                .unbind())
        })
        .map(|f| f.bind(py).clone())
}

fn reporter_feed_default_fn(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    REPORTER_FEED_DEFAULT_FN
        .get_or_try_init(py, || {
            Ok(py
                .import("pytest._reporter")?
                .getattr("feed_default")?
                .unbind())
        })
        .map(|f| f.bind(py).clone())
}

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
        reporter_logreport_fn(py)?.call1((report,))?;
        Ok(())
    })();
    if let Err(err) = result {
        eprintln!("INTERNAL ERROR: {}", format_exception(py, &err));
    }
}

/// Feed a report to the default reporter's stats (native mode only).
/// Populates terminalreporter.stats so conftest pytest_terminal_summary hooks
/// can access stats['passed'] etc. without the default reporter printing.
pub fn reporter_feed_default(py: Python<'_>, report: &Bound<'_, PyAny>) {
    let result = (|| -> PyResult<()> {
        reporter_feed_default_fn(py)?.call1((report,))?;
        Ok(())
    })();
    if let Err(err) = result {
        eprintln!("INTERNAL ERROR: {}", format_exception(py, &err));
    }
}

/// Ensure the default reporter has a trailing newline (plugins may
/// have written partial lines through the hook relay).
pub fn reporter_ensure_newline(py: Python<'_>) {
    let _ = py
        .import("pytest._reporter")
        .and_then(|m| m.getattr("ensure_newline"))
        .and_then(|f| f.call0());
}

/// Drain plugin-emitted reports (e.g., subtests) from the `_logreport_sink`.
/// These are reports that plugins logged via `ihook.pytest_runtest_logreport`
/// during normal (non-delegated) runs. Returns Rust `TestReport`s for the
/// engine to count and include in the session.
pub fn drain_plugin_reports(py: Python<'_>) -> Vec<crate::report::TestReport> {
    let result = (|| -> PyResult<Vec<crate::report::TestReport>> {
        let sink = py.import("_pytest.runner")?.getattr("_logreport_sink")?;
        let reports: Vec<Bound<'_, PyAny>> =
            sink.call_method0("drain_plugin_reports")?.extract()?;
        let mut out = Vec::new();
        for report in &reports {
            match crate::runner::report_from_proxy(py, report) {
                Ok(r) => out.push(r),
                Err(err) => {
                    eprintln!("INTERNAL ERROR: {}", format_exception(py, &err));
                }
            }
        }
        Ok(out)
    })();
    result.unwrap_or_default()
}

/// Subtest stat counts from the default reporter (populated by the
/// pytest-subtests plugin via the hook relay). Returns a map like
/// {"subtests passed": 3, "subtests failed": 2}.
pub fn reporter_subtest_stats(py: Python<'_>) -> std::collections::HashMap<String, usize> {
    py.import("pytest._reporter")
        .and_then(|m| m.getattr("subtest_stats"))
        .and_then(|f| f.call0())
        .and_then(|r| r.extract())
        .unwrap_or_default()
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
        // In a nested run, surface the failed collection to the HookRecorder
        // (getfailedcollections / assertoutcome read pytest_collectreport).
        crate::python::record_hook(
            py,
            "pytest_collectreport",
            &[("report", report.clone().unbind())],
        );
        py.import("pytest._reporter")?
            .getattr("collectreport")?
            .call1((report,))?;
        Ok(())
    })();
    if let Err(err) = result {
        eprintln!("INTERNAL ERROR: {}", format_exception(py, &err));
    }
}

/// In a nested run, surface a skipped collection (e.g. a DoctestModule that
/// could not import under --doctest-ignore-import-errors) to the HookRecorder
/// as a skipped CollectReport, matching upstream's collect-time Skipped.
/// No-op on the outer run.
pub fn record_collect_skip(py: Python<'_>, nodeid: &str, longrepr: &str) {
    if !crate::engine::inprocess::recording() {
        return;
    }
    let result = (|| -> PyResult<()> {
        let kwargs = PyDict::new(py);
        kwargs.set_item("nodeid", nodeid)?;
        kwargs.set_item("outcome", "skipped")?;
        kwargs.set_item("longrepr", longrepr)?;
        let file = nodeid.split("::").next().unwrap_or(nodeid);
        kwargs.set_item("location", (file, py.None(), file))?;
        kwargs.set_item("result", pyo3::types::PyList::empty(py))?;
        kwargs.set_item("sections", pyo3::types::PyList::empty(py))?;
        let report = py
            .import("_pytest.reports")?
            .getattr("CollectReport")?
            .call((), Some(&kwargs))?;
        crate::python::record_hook(py, "pytest_collectreport", &[("report", report.unbind())]);
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
    let cls = reports_test_report_cls(py)?;
    let proxy = cls.call((), Some(&kwargs))?;
    // For subtest reports, reclassify the proxy as a SubtestReport so
    // Python hooks (pytest_report_teststatus) see the correct type and
    // call _sub_test_description() for the format word.
    if let Some(desc) = &report.subtest_desc {
        let sub_cls = py
            .import("pytest_subtests.plugin")
            .and_then(|m| m.getattr("SubTestReport"))
            .or_else(|_| {
                py.import("_pytest.subtests")
                    .and_then(|m| m.getattr("SubtestReport"))
            });
        if let Ok(sub_cls) = sub_cls {
            let is_passed = report.outcome == Outcome::Passed;
            let is_failed = report.outcome == Outcome::Failed;
            let is_skipped = report.outcome == Outcome::Skipped;
            let _ = proxy.setattr("passed", is_passed);
            let _ = proxy.setattr("failed", is_failed);
            let _ = proxy.setattr("skipped", is_skipped);
            let _ = proxy.setattr("__class__", &sub_cls);
            let desc_copy = desc.clone();
            let desc_fn =
                pyo3::types::PyCFunction::new_closure(py, None, None, move |_args, _kwargs| {
                    Ok::<String, PyErr>(desc_copy.clone())
                })?;
            let _ = proxy.setattr("_sub_test_description", &desc_fn);
            let _ = proxy.setattr("sub_test_description", desc_fn);
            let ctx_cls = py
                .import("pytest_subtests.plugin")
                .and_then(|m| m.getattr("SubTestContext"))
                .or_else(|_| {
                    py.import("_pytest.subtests")
                        .and_then(|m| m.getattr("SubtestContext"))
                });
            if let Ok(ctx_cls) = ctx_cls {
                let msg = desc
                    .strip_prefix('[')
                    .and_then(|s| s.find(']').map(|i| &s[..i]));
                let ctx_kwargs = PyDict::new(py);
                match msg {
                    Some(m) => ctx_kwargs.set_item("msg", m)?,
                    None => ctx_kwargs.set_item("msg", pyo3::types::PyNone::get(py))?,
                }
                ctx_kwargs.set_item("kwargs", PyDict::new(py))?;
                if let Ok(ctx) = ctx_cls.call((), Some(&ctx_kwargs)) {
                    let _ = proxy.setattr("context", ctx);
                }
            }
        }
    }
    Ok(proxy.unbind())
}
