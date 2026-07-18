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

/// A faulthandler_timeout dump-traceback-later timer, armed for the whole
/// setup/call/teardown protocol (mirrors upstream's pytest_runtest_protocol
/// hookwrapper, which spans all three phases). Cancelling on every return
/// path — including the early-return error paths below — needs an RAII
/// guard, not a plain call before `return`.
struct FaulthandlerTimeoutGuard<'py>(Python<'py>);

impl<'py> FaulthandlerTimeoutGuard<'py> {
    /// Arms the timer only when faulthandler_timeout is actually set and the
    /// plugin isn't disabled — checked here in Rust (a cheap ini-string
    /// lookup) so the overwhelmingly common case (no timeout configured)
    /// never pays for a Python call into pytest._faulthandler at all.
    fn arm_if_active(py: Python<'py>, config: &Config) -> Option<Self> {
        if config.plugin_disabled("faulthandler") {
            return None;
        }
        let timeout: f64 = config
            .get_ini("faulthandler_timeout")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0.0);
        if timeout <= 0.0 {
            return None;
        }
        python::faulthandler_start_timeout(py);
        Some(Self(py))
    }
}

impl Drop for FaulthandlerTimeoutGuard<'_> {
    fn drop(&mut self) {
        python::faulthandler_cancel_timeout(self.0);
    }
}

#[allow(clippy::type_complexity, clippy::too_many_arguments)]
pub(crate) fn run_one(
    py: Python<'_>,
    plugins: &[Box<dyn Plugin>],
    session: &mut Session,
    config: &Config,
    item: &TestItem,
    nextitem: Option<&TestItem>,
    pre_teardown: Option<&dyn Fn(&[TestReport])>,
    on_native_start: impl FnOnce(Python<'_>, &mut Session, &Config, &TestItem),
) -> Vec<TestReport> {
    session.delegated_render = false;
    let _faulthandler_guard = FaulthandlerTimeoutGuard::arm_if_active(py, config);
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
                    TimeMark::now(),
                    &err,
                )];
            }
        };
    // A plain pytest_runtest_protocol impl (pytest-rerunfailures) may replace
    // the protocol; if one handles the item, use the reports it logged. A
    // replacing impl owns its own nodeid-print/logstart (matching upstream:
    // the builtin protocol impl is what does that, and a replacing impl that
    // skips calling it also skips it) — so on_native_start (the nodeid
    // print / live-log "start" label / pytest_runtest_logstart dispatch)
    // only runs on the native (Ok(None)) path, and runs *before*
    // run_one_body, mirroring upstream firing a conftest's plain
    // pytest_runtest_protocol impl before the builtin one.
    let reports = match protocol::delegate_protocol(py, plugins, session, config, item, nextitem) {
        Ok(Some(reports)) => reports,
        Ok(None) => {
            on_native_start(py, session, config, item);
            run_one_body(py, plugins, session, config, item, pre_teardown)
        }
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
            // A worker reports results to the controller and doesn't own the
            // process exit code — keep the pre-existing per-item error
            // behavior there; escalating a worker-side internal error into a
            // controller abort is a separate xdist-protocol change.
            if config.is_worker() {
                return vec![report_from_err(
                    py,
                    config,
                    item,
                    Phase::Setup,
                    TimeMark::now(),
                    &err,
                )];
            }
            // A replacing pytest_runtest_protocol hookimpl raised. Upstream's
            // pytest_runtestloop has no try/except around that hook call, so
            // this propagates straight to wrap_session's INTERNAL_ERROR
            // handler: banner to stdout, fire pytest_internalerror (records
            // junitxml's "internal" testcase and may raise Exit to override
            // the code), then abort the whole session.
            let msg = python::format_internal_error(py, &err, config.get_flag("full-trace"));
            for line in msg.lines() {
                println!("INTERNALERROR> {line}");
            }
            python::junit_internal_error(py, &msg);
            let code =
                python::notify_internal_error(py, &err, crate::report::exit_code::INTERNAL_ERROR);
            session.internal_error_exit_code = Some(code);
            return vec![];
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
