//! Per-item runtest protocol: skip/xfail gating, fixture setup, the call
//! phase, outcome classification, and teardown handoff. `run_one` wraps
//! `run_one_body`, which drives setup -> call -> outcome via run_item_body.

use pyo3::prelude::*;

use crate::collect::TestItem;
use crate::config::Config;
use crate::hooks::Plugin;
use crate::python;
use crate::report::{Phase, TestReport};
use crate::session::Session;

use super::*;

mod body;
mod setup;

pub(crate) use body::run_one_body;
pub(crate) use setup::build_test_setup;

pub(crate) fn run_one(
    py: Python<'_>,
    plugins: &[Box<dyn Plugin>],
    session: &mut Session,
    config: &Config,
    item: &TestItem,
    nextitem: Option<&TestItem>,
) -> Vec<TestReport> {
    session.delegated_render = false;
    // pytest_runtest_protocol hookwrappers (e.g. pytest-timeout's timer)
    // surround the whole setup/call/teardown protocol: their pre-yield part
    // runs now, the rest after the item finishes.
    let wrappers =
        match start_runtest_py_wrappers(py, session, item, "pytest_runtest_protocol", false) {
            Ok(wrappers) => wrappers,
            Err(err) => {
                return vec![report_from_err(
                    py,
                    config,
                    item,
                    Phase::Setup,
                    TimeMark::now(py),
                    &err,
                )];
            }
        };
    // A plain pytest_runtest_protocol impl (pytest-rerunfailures) may replace
    // the protocol; if one handles the item, use the reports it logged.
    let reports = match protocol::delegate_protocol(py, plugins, session, config, item, nextitem) {
        Ok(Some(reports)) => reports,
        Ok(None) => run_one_body(py, plugins, session, config, item),
        Err(err) => {
            let _ = finish_runtest_py_wrappers(py, &wrappers);
            if err.is_instance_of::<pyo3::exceptions::PyException>(py) {
                let is_usage = (|| -> PyResult<bool> {
                    let cls = py.import("pytest")?.getattr("UsageError")?;
                    Ok(err.is_instance(py, cls.cast()?))
                })()
                .unwrap_or(false);
                if is_usage {
                    let msg = err
                        .value(py)
                        .str()
                        .map(|s| s.to_string())
                        .unwrap_or_default();
                    eprintln!("ERROR: {msg}");
                    python::set_session_shouldstop(py, &msg);
                    return vec![];
                }
            }
            return vec![report_from_err(
                py,
                config,
                item,
                Phase::Setup,
                TimeMark::now(py),
                &err,
            )];
        }
    };
    if let Err(err) = finish_runtest_py_wrappers(py, &wrappers) {
        eprintln!(
            "pytest_runtest_protocol wrapper teardown failed for {}: {}",
            item.nodeid,
            err.value(py)
        );
    }
    reports
}
