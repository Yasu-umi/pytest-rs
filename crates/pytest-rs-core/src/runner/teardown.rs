//! Item and scope teardown: run finalizers (function/class/module/
//! package/session scope) and emit the resulting teardown reports.

use std::io::Write as _;
use std::time::Instant;

use pyo3::prelude::*;

use crate::collect::TestItem;
use crate::config::Config;
use crate::fixture::Scope;
use crate::hooks::{HookContext, Plugin};
use crate::python;
use crate::report::{Outcome, Phase, TestReport};
use crate::session::{Finalizer, Session};

use super::*;

pub(crate) fn teardown_one(
    py: Python<'_>,
    plugins: &[Box<dyn Plugin>],
    session: &mut Session,
    config: &Config,
    item: &TestItem,
    xfail: bool,
    reports: &mut Vec<TestReport>,
) {
    // tmp_path retention: the fixture teardown (a function finalizer below)
    // reads this item's call outcome; None means no call phase ran.
    let call_passed = reports
        .iter()
        .rev()
        .find(|r| r.phase == Phase::Call && r.subtest_desc.is_none())
        .map(|r| matches!(r.outcome, Outcome::Passed | Outcome::XPassed));
    python::tmp_path_record_call(py, &item.nodeid, call_passed);
    // log_cli: the call outcome prints before teardown records appear —
    // with the item capture paused, so the words reach the real terminal.
    if session.live_logging {
        python::capture_suspend(py);
    }
    live_flush(session, config, reports);
    if session.live_logging {
        python::capture_resume(py);
    }
    // --setup-show: the call outcome char prints before the TEARDOWN
    // narration, right after the item line (pytest's logreport timing).
    if setup_show_active(config)
        && !session.live_logging
        && config.verbose == 0
        && !config.quiet
        && !config.no_terminal()
    {
        python::capture_suspend(py);
        while session.streamed_chars < reports.len() {
            let report = &reports[session.streamed_chars];
            session.streamed_chars += 1;
            if let Some(c) = report.progress_char() {
                print!(
                    "{}",
                    crate::tw::markup(&c.to_string(), outcome_codes(report))
                );
                let _ = std::io::stdout().flush();
            }
        }
        python::capture_resume(py);
    }
    let log_level_cfg: Option<String> = config
        .get_value("log-level")
        .map(str::to_string)
        .or_else(|| config.get_ini("log_level").map(str::to_string));
    python::setenv(
        py,
        "PYTEST_CURRENT_TEST",
        &format!("{} (teardown)", item.nodeid),
    );
    python::log_start_phase(py, "teardown", log_level_cfg.as_deref());
    let teardown_started = Instant::now();
    let mut errors = teardown_scope(
        py,
        plugins,
        session,
        config,
        Scope::Function,
        &item.nodeid,
        item,
    );

    if let Err(err) = fire_runtest_py_hooks(py, session, item, "pytest_runtest_teardown") {
        errors.push(python::format_exception(py, &err));
    }
    let hook_result = (|| -> PyResult<()> {
        let mut ctx = HookContext {
            py,
            session,
            config,
        };
        for plugin in plugins {
            plugin.pytest_runtest_teardown(&mut ctx, item)?;
        }
        Ok(())
    })();
    if let Err(err) = hook_result {
        errors.push(python::format_exception(py, &err));
    }
    // Unraisable exceptions surfaced during teardown (upstream's trylast
    // pytest_runtest_teardown hookimpl): an error filter errors the item.
    if let Err(err) = python::unraisable_collect(py) {
        errors.push(python::format_test_failure(
            py,
            &err,
            config.get_value("tb").unwrap_or("long"),
        ));
    }
    // Unhandled thread exceptions surfaced during teardown (upstream's trylast
    // pytest_runtest_teardown hookimpl): an error filter errors the item.
    if let Err(err) = python::threadexception_collect(py) {
        errors.push(python::format_test_failure(
            py,
            &err,
            config.get_value("tb").unwrap_or("long"),
        ));
    }

    if errors.is_empty() {
        reports.push(TestReport {
            nodeid: item.nodeid.clone(),
            phase: Phase::Teardown,
            outcome: Outcome::Passed,
            duration: teardown_started.elapsed(),
            longrepr: None,
            location: None,
            subtest_desc: None,
            // The teardown report carries the item's full captured output
            // (pytest writes junit system-out from it).
            sections: python::log_failure_sections(py),
            rerun: false,
            xfail_longrepr: None,
            reprcrash_message: None,
            head_line: None,
        });
    } else {
        reports.push(TestReport {
            nodeid: item.nodeid.clone(),
            phase: Phase::Teardown,
            // @pytest.mark.xfail covers teardown failures too (pytest's
            // makereport hook turns any failing phase into an xfail).
            outcome: if xfail {
                Outcome::XFailed
            } else {
                Outcome::Failed
            },
            duration: teardown_started.elapsed(),
            longrepr: Some(errors.join("\n")),
            location: None,
            subtest_desc: None,
            // "Captured stdout/log {when}" report sections; the terminal
            // appends them to the longrepr at render time.
            sections: python::log_failure_sections(py),
            rerun: false,
            xfail_longrepr: None,
            reprcrash_message: None,
            head_line: None,
        });
    }
    python::unsetenv(py, "PYTEST_CURRENT_TEST");
    python::log_finish_item(py);
}

/// Run (LIFO) and remove every pending finalizer of the given scope instance.
/// Returns formatted errors. Also evicts cached fixture values of that
/// instance.
/// Run a deferred (module/class/package/session) scope teardown under a
/// capture phase; failures become a teardown ERROR report attributed to
/// the last completed item, like pytest where these finalizers run inside
/// that item's teardown.
#[allow(clippy::too_many_arguments)]
pub(crate) fn teardown_scope_reported(
    py: Python<'_>,
    plugins: &[Box<dyn Plugin>],
    session: &mut Session,
    config: &Config,
    scope: Scope,
    instance: &str,
    item: &TestItem,
    report_nodeid: Option<&str>,
) -> Option<TestReport> {
    // No pending finalizers: still run teardown_scope (it evicts cached
    // fixture values) but skip the capture round-trip.
    let has_finalizers = session
        .finalizers
        .iter()
        .any(|pf| pf.scope == scope && pf.instance == instance);
    if has_finalizers {
        python::capture_scope_teardown_begin(py);
    }
    let started = Instant::now();
    let errors = teardown_scope(py, plugins, session, config, scope, instance, item);
    if errors.is_empty() {
        // A passing scope teardown that printed (e.g. teardown_module): its
        // captured output is labelled "Captured stdout teardown". Surface it
        // as a passing teardown report so the caller can merge it into the
        // item's teardown sections (an empty one yields None).
        let sections = if has_finalizers {
            python::log_failure_sections(py)
        } else {
            Vec::new()
        };
        if has_finalizers {
            python::log_finish_item(py);
        }
        if sections.is_empty() {
            return None;
        }
        return Some(TestReport {
            nodeid: report_nodeid.unwrap_or(&item.nodeid).to_string(),
            phase: Phase::Teardown,
            outcome: Outcome::Passed,
            duration: started.elapsed(),
            longrepr: None,
            location: None,
            subtest_desc: None,
            sections,
            rerun: false,
            xfail_longrepr: None,
            reprcrash_message: None,
            head_line: None,
        });
    }
    let sections = python::log_failure_sections(py);
    python::log_finish_item(py);
    Some(TestReport {
        nodeid: report_nodeid.unwrap_or(&item.nodeid).to_string(),
        phase: Phase::Teardown,
        outcome: Outcome::Failed,
        duration: started.elapsed(),
        longrepr: Some(errors.join("\n")),
        location: None,
        subtest_desc: None,
        sections,
        rerun: false,
        xfail_longrepr: None,
        reprcrash_message: None,
        head_line: None,
    })
}

pub(crate) fn teardown_scope(
    py: Python<'_>,
    _plugins: &[Box<dyn Plugin>],
    session: &mut Session,
    config: &Config,
    scope: Scope,
    instance: &str,
    _item: &TestItem,
) -> Vec<String> {
    let mut errors = Vec::new();
    let mut idx = session.finalizers.len();
    while idx > 0 {
        idx -= 1;
        let matches = {
            let pf = &session.finalizers[idx];
            pf.scope == scope && pf.instance == instance
        };
        if !matches {
            continue;
        }
        let pf = session.finalizers.remove(idx);
        let result = match &pf.finalizer {
            Finalizer::Callable(callable) => callable.bind(py).call0().map(|_| ()),
            Finalizer::GenNext(generator) => python::finalize_generator(py, generator),
        };
        if let Err(err) = result {
            // pytest-style longrepr (source lines + E markers), like any
            // other failing phase.
            errors.push(python::format_test_failure(
                py,
                &err,
                config.get_value("tb").unwrap_or("long"),
            ));
        }
    }
    session
        .fixture_cache
        .retain(|(_, _, inst, _), _| inst != instance);
    errors
}

pub(crate) fn setup_show_active(config: &Config) -> bool {
    config.get_flag("setup-only") || config.get_flag("setup-plan") || config.get_flag("setup-show")
}
