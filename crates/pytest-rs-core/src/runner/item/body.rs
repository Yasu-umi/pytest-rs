use std::time::Duration;

use pyo3::prelude::*;

use super::super::*;
use super::setup::{ItemPrelude, evaluate_item_prelude};
use crate::collect::TestItem;
use crate::config::Config;
use crate::hooks::{HookContext, Plugin};
use crate::python;
use crate::report::{Outcome, Phase, TestReport};
use crate::session::{Finalizer, PendingFinalizer, Session};

/// Record a pytest.exit()/Ctrl-C abort during a test's setup or call.
/// `abort_banner` is always set (an xdist worker forwards it verbatim to the
/// controller over the WorkerMsg::Interrupted IPC message, which has no
/// field for the richer repr below). A KeyboardInterrupt additionally gets
/// `keyboard_interrupt_repr`, rendered as its own end-of-summary
/// "!!! KeyboardInterrupt !!!" block (upstream's `_report_keyboardinterrupt`,
/// with a crash line and, under --fulltrace, the full traceback) instead of
/// `abort_banner`'s immediate pre-summary print — `finish_session` skips that
/// early print whenever `keyboard_interrupt_repr` is set, to avoid double
/// (and wrongly-ordered) output in the local, non-xdist case.
fn record_abort(py: Python<'_>, session: &mut Session, err: &PyErr, code: i32) {
    session.exit_code_override = Some(code);
    session.abort_banner = python::session_abort_banner(py, err);
    if err.is_instance_of::<pyo3::exceptions::PyKeyboardInterrupt>(py) {
        let short = python::format_test_failure(py, err, "line");
        let long = python::format_test_failure(py, err, "long");
        session.keyboard_interrupt_repr = Some((short, long));
    }
}

pub(crate) fn run_custom_item(py: Python<'_>, config: &Config, item: &TestItem) -> Vec<TestReport> {
    let started = TimeMark::now();
    // reportinfo()[2] is the failure-section heading (pytest-mypy's
    // test_name_formatter); empty means "use the nodeid domain" (default Item).
    let head_line = item
        .func
        .bind(py)
        .call_method0("reportinfo")
        .ok()
        .and_then(|info| info.get_item(2).ok())
        .and_then(|name| name.extract::<String>().ok())
        .filter(|name| !name.is_empty());
    let result = py
        .import("pytest._node")
        .and_then(|m| m.getattr("run_custom_item"))
        .and_then(|f| f.call1((item.func.bind(py),)));
    let triples = match result {
        Ok(r) => r,
        Err(err) => {
            return vec![report_from_err(
                py,
                config,
                item,
                Phase::Setup,
                started,
                &err,
            )];
        }
    };
    let mut reports = Vec::new();
    let iter = match triples.try_iter() {
        Ok(it) => it,
        Err(err) => {
            return vec![report_from_err(
                py,
                config,
                item,
                Phase::Setup,
                started,
                &err,
            )];
        }
    };
    for entry in iter {
        let Ok(entry) = entry else { continue };
        let (when, outcome, longrepr): (String, String, Option<String>) = match entry.extract() {
            Ok(t) => t,
            Err(_) => continue,
        };
        let phase = match when.as_str() {
            "setup" => Phase::Setup,
            "teardown" => Phase::Teardown,
            _ => Phase::Call,
        };
        let oc = match outcome.as_str() {
            "passed" => Outcome::Passed,
            "skipped" => Outcome::Skipped,
            // pytest-mypy's --mypy-xfail adds an xfail marker mid-runtest;
            // _node.run_custom_item evaluates it and reports the outcome.
            "xfailed" => Outcome::XFailed,
            "xpassed" => Outcome::XPassed,
            _ => Outcome::Failed,
        };
        let location =
            matches!(oc, Outcome::Skipped | Outcome::XFailed).then(|| item.nodeid.clone());
        reports.push(TestReport {
            nodeid: item.nodeid.clone(),
            phase,
            outcome: oc,
            duration: started.elapsed(),
            longrepr,
            location,
            subtest_desc: None,
            sections: Vec::new(),
            rerun: false,
            xfail_longrepr: None,
            reprcrash_message: None,
            head_line: head_line.clone(),
        });
    }
    reports
}

/// Test-item setup: run pytest_runtest_setup hooks, build the (fresh)
/// class instance, resolve autouse + usefixtures + signature fixtures, and
/// assemble the call kwargs. Returns (callable, kwargs, the test's own
/// `request` if it takes one).
pub(crate) type SetupOk = (
    Py<PyAny>,
    Vec<(String, Py<PyAny>)>,
    Option<Py<crate::request::PyRequest>>,
);
#[allow(clippy::type_complexity)]
pub(crate) fn run_one_body(
    py: Python<'_>,
    plugins: &[Box<dyn Plugin>],
    session: &mut Session,
    config: &Config,
    item: &TestItem,
    // Called with setup+call reports before teardown (worker mode: sends them
    // to the controller so a teardown crash doesn't swallow the call outcome).
    pre_teardown: Option<&dyn Fn(&[TestReport])>,
) -> Vec<TestReport> {
    // Custom collector items (pytest-ruff/pytest-mypy): the func IS a
    // pytest.Item with setup()/runtest()/teardown(); run that protocol with
    // no fixtures instead of calling it as a test function. Detect precisely
    // via isinstance(pytest.Item) — a hasattr("runtest") probe would also
    // match Mocks and other auto-attr objects used as test funcs.
    let (xfailed, runxfail) = match evaluate_item_prelude(py, session, config, item) {
        ItemPrelude::Done(reports) => return reports,
        ItemPrelude::Run { xfailed, runxfail } => (xfailed, runxfail),
    };
    let mut reports = Vec::new();
    let xfail = xfailed.is_some() && !runxfail;

    // request.getfixturevalue() support: expose this item's engine state to
    // Python for the duration of the run (popped when the guard drops).
    let _resolve_ctx = push_resolve_ctx(plugins, session, config, item);

    // Warnings emitted from here on are attributed to this item in the
    // warnings summary. The path also lets _rewrite._format_assert scope
    // conftest pytest_assertrepr_compare hooks to this item's directory.
    let _ = py.import("pytest._wcapture").and_then(|m| {
        m.call_method1(
            "set_current_test",
            (item.nodeid.as_str(), item.path.to_string_lossy().as_ref()),
        )
    });

    // One contextvars context per async item: fixtures + test share it,
    // and context changes stay isolated between async tests. Sync tests run
    // unisolated in the root context (pytest behavior), so their
    // contextvar mutations are visible to later tests.
    if item.is_coroutine
        && let Err(err) = python::begin_item_context(py)
    {
        reports.push(report_from_err(
            py,
            config,
            item,
            Phase::Setup,
            TimeMark::now(),
            &err,
        ));
        return reports;
    }

    // @pytest.mark.filterwarnings: per-item filters inside a
    // catch_warnings block (farthest mark applied first, closest wins).
    let mark_filter_specs: Vec<String> = item
        .marks
        .iter()
        .rev()
        .filter(|mark| mark.name == "filterwarnings")
        .flat_map(|mark| {
            mark.obj
                .bind(py)
                .getattr("args")
                .ok()
                .and_then(|args| args.extract::<Vec<String>>().ok())
                .unwrap_or_default()
        })
        .collect();
    // Entered for every item (even without marks) so the "default" warning
    // action's once-per-location registry resets per test, like pytest's
    // per-item catch_warnings block.
    let item_filters = match python::begin_item_filters(py, &mark_filter_specs) {
        Ok(ctx) => Some(ctx),
        Err(err) => {
            reports.push(report_from_err(
                py,
                config,
                item,
                Phase::Setup,
                TimeMark::now(),
                &err,
            ));
            python::end_item_context(py);
            return reports;
        }
    };
    let close_item_filters = |py: Python<'_>| {
        if let Some(ctx) = &item_filters {
            python::end_item_filters(py, ctx);
        }
        // Warnings emitted between items (config/collect phases) carry no
        // nodeid in the summary, like pytest.
        let _ = py
            .import("pytest._wcapture")
            .and_then(|m| m.call_method1("set_current_test", (py.None(),)));
    };

    let teardown_xfail = run_item_body(
        py,
        plugins,
        session,
        config,
        item,
        &mut reports,
        xfailed,
        runxfail,
        xfail,
    );
    // Stream setup+call reports before teardown runs so a crash in teardown
    // doesn't swallow the call outcome (worker mode only, None otherwise).
    // Pytest's capfd capture redirects fd 1 during tests; suspend it so the
    // send() writes reach the real IPC pipe rather than the capture buffer.
    if let Some(f) = pre_teardown {
        python::capture_suspend(py);
        f(&reports);
        let _ = std::io::stdout().flush();
        python::capture_resume(py);
    }
    teardown_one(
        py,
        plugins,
        session,
        config,
        item,
        teardown_xfail,
        &mut reports,
    );
    close_item_filters(py);
    python::end_item_context(py);
    reports
}

/// Run the setup -> call -> outcome phases for one item, pushing each
/// phase report into `reports`. Returns the xfail flag the caller's
/// teardown should use (a NOTRUN-at-call forces it on). Teardown and the
/// filter/context close are the caller's single trailing step.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_item_body(
    py: Python<'_>,
    plugins: &[Box<dyn Plugin>],
    session: &mut Session,
    config: &Config,
    item: &TestItem,
    reports: &mut Vec<TestReport>,
    mut xfailed: Option<XfailEval>,
    runxfail: bool,
    xfail: bool,
) -> bool {
    // ---- setup -----------------------------------------------------------
    // Per-phase log capture (caplog records + "Captured log" sections).
    let log_level_cfg: Option<String> = config
        .get_value("log-level")
        .map(str::to_string)
        .or_else(|| config.get_ini("log_level").map(str::to_string));
    python::setenv(
        py,
        "PYTEST_CURRENT_TEST",
        &format!("{} (setup)", item.nodeid),
    );
    python::log_start_phase(py, "setup", log_level_cfg.as_deref());
    let setup_started = TimeMark::now();
    let setup_result = build_test_setup(py, plugins, session, config, item);

    let (callable, kwargs, test_request) = match setup_result {
        Ok(setup) => setup,
        Err(err) => {
            let mut report = report_from_err(py, config, item, Phase::Setup, setup_started, &err);
            // @pytest.mark.xfail covers setup failures too.
            if let Some(xf) = &xfailed
                && xfail
                && report.outcome == Outcome::Failed
                && xfail_raises_ok(py, &xfailed, &err)
            {
                report.outcome = Outcome::XFailed;
                report.longrepr = Some(xf.reason.clone());
            }
            reports.push(report);
            return xfail;
        }
    };
    // Unraisable exceptions surfaced during setup (upstream's trylast
    // pytest_runtest_setup hookimpl): an error filter fails the setup.
    if let Err(err) = python::unraisable_collect(py) {
        reports.push(report_from_err(
            py,
            config,
            item,
            Phase::Setup,
            setup_started,
            &err,
        ));
        return xfail;
    }
    // Unhandled thread exceptions surfaced during setup (upstream's trylast
    // pytest_runtest_setup hookimpl): an error filter fails the setup.
    if let Err(err) = python::threadexception_collect(py) {
        reports.push(report_from_err(
            py,
            config,
            item,
            Phase::Setup,
            setup_started,
            &err,
        ));
        return xfail;
    }
    // Peek at setup-phase captured output for capstdout/capstderr on passing
    // setup reports (upstream pytest includes them; buffer is not drained yet
    // — _snap_section() will drain it when call phase starts).
    let setup_sections = python::log_failure_sections(py);
    reports.push(TestReport {
        nodeid: item.nodeid.clone(),
        phase: Phase::Setup,
        outcome: Outcome::Passed,
        duration: setup_started.elapsed(),
        longrepr: None,
        location: None,
        subtest_desc: None,
        sections: setup_sections,
        rerun: false,
        xfail_longrepr: None,
        reprcrash_message: None,
        head_line: None,
    });

    if setup_show_active(config) {
        // Upstream's show_test_item: sorted(item.fixturenames), the item's
        // whole closure (incl. `request` and autouse), not just its direct
        // call kwargs.
        let mut names: Vec<String> = item.fixture_names.clone();
        for extra in &item.extra_fixture_names {
            if !names.contains(extra) {
                names.push(extra.clone());
            }
        }
        names.sort_unstable();
        // Narration must reach the real terminal, not the item capture.
        // pytest's tw.line() style: a leading newline closes the current
        // line, no trailing one (the outcome char appends right after).
        python::capture_suspend(py);
        if names.is_empty() {
            print!("\n        {}", item.nodeid);
        } else {
            print!(
                "\n        {} (fixtures used: {})",
                item.nodeid,
                names.join(", ")
            );
        }
        let _ = std::io::stdout().flush();
        python::capture_resume(py);
        if config.get_flag("setup-only") || config.get_flag("setup-plan") {
            // Fixtures only: tear down without calling the test.
            return xfail;
        }
    }

    // ---- call --------------------------------------------------------------
    python::setenv(
        py,
        "PYTEST_CURRENT_TEST",
        &format!("{} (call)", item.nodeid),
    );
    // Fixtures may have applied an xfail marker dynamically during setup;
    // pytest re-evaluates at call start (including run=False NOTRUN).
    if xfailed.is_none() {
        let extra = added_marks(py);
        if !extra.is_empty() {
            xfailed = evaluate_xfail_marks(py, session, config, item, &extra).unwrap_or(None);
            if let Some(xf) = &xfailed
                && !runxfail
                && !xf.run
            {
                reports.push(TestReport {
                    nodeid: item.nodeid.clone(),
                    phase: Phase::Call,
                    outcome: Outcome::XFailed,
                    duration: Duration::ZERO,
                    longrepr: Some(format!("[NOTRUN] {}", xf.reason)),
                    location: None,
                    subtest_desc: None,
                    sections: Vec::new(),
                    rerun: false,
                    xfail_longrepr: None,
                    reprcrash_message: None,
                    head_line: None,
                });
                return true;
            }
        }
    }
    python::log_start_phase(py, "call", log_level_cfg.as_deref());
    let call_started = TimeMark::now();
    // pytest_runtest_call hookwrappers surround just the call phase; their
    // post-yield part runs after the test body, pass or fail.
    let (call_wrappers, wrapper_start_err) =
        match start_runtest_py_wrappers(py, session, item, "pytest_runtest_call", true) {
            Ok(wrappers) => (wrappers, None),
            Err(err) => (Vec::new(), Some(err)),
        };
    let call_result = if let Some(err) = wrapper_start_err {
        Err(err)
    } else {
        (|| -> PyResult<bool> {
            // Native plugins (the Rust anyio/asyncio runners) claim async test
            // calls first — their Python counterparts also expose a
            // pytest_pyfunc_call, so letting those run instead would
            // double-drive the test. Only if no native plugin handles the call
            // do conftest/plugin pytest_pyfunc_call hooks get a turn (a truthy
            // return means a hook invoked the test; a logging-only hook returns
            // None and the engine calls it natively).
            let mut ctx = HookContext {
                py,
                session,
                config,
            };
            for plugin in plugins {
                if plugin
                    .pytest_pyfunc_call(&mut ctx, item, &callable, &kwargs)?
                    .is_some()
                {
                    return Ok(true);
                }
            }
            if fire_pyfunc_call_hooks(py, session, item, &callable, &kwargs)? {
                return Ok(true);
            }
            Ok(false)
        })()
    };
    // The original error wins over a wrapper-teardown one.
    let call_result = match finish_runtest_py_wrappers(py, &call_wrappers) {
        Ok(()) => call_result,
        Err(err) => call_result.and(Err(err)),
    };

    // pytest.exit / Ctrl-C abort the session without a test outcome.
    if let Err(err) = &call_result
        && let Some(code) = python::session_abort_code(py, err)
    {
        record_abort(py, session, err, code);
        // Subtests recorded before the abort still report (e.g. pytest.exit
        // inside a subtest block records a failed subtest, then aborts).
        let (sub_reports, _) = python::pop_subtest_reports(py, config, item);
        reports.extend(sub_reports);
        return xfail;
    }

    let mut raises_ok = true;
    let mut call_err: Option<PyErr> = None;
    let report = match call_result {
        Ok(true) => TestReport {
            nodeid: item.nodeid.clone(),
            phase: Phase::Call,
            outcome: Outcome::Passed,
            duration: call_started.elapsed(),
            longrepr: None,
            location: None,
            subtest_desc: None,
            sections: python::log_failure_sections(py),
            rerun: false,
            xfail_longrepr: None,
            reprcrash_message: None,
            head_line: None,
        },
        Ok(false) => {
            if item.is_coroutine {
                // pytest 8.4+: unhandled async tests fail (without being
                // called) instead of skipping.
                TestReport {
                    nodeid: item.nodeid.clone(),
                    phase: Phase::Call,
                    outcome: Outcome::Failed,
                    duration: call_started.elapsed(),
                    longrepr: Some(
                        "async def functions are not natively supported.\n\
                         You need to install a suitable plugin for your async framework, \
                         for example:\n  - anyio\n  - pytest-asyncio\n  - pytest-tornasync\n  \
                         - pytest-trio\n  - pytest-twisted"
                            .to_string(),
                    ),
                    location: None,
                    subtest_desc: None,
                    sections: Vec::new(),
                    rerun: false,
                    xfail_longrepr: None,
                    reprcrash_message: None,
                    head_line: None,
                }
            } else {
                match python::call_with_kwargs(py, &callable, &kwargs) {
                    Ok(retval) => {
                        let is_coro = !retval.is_none()
                            && py
                                .import("inspect")
                                .and_then(|i| {
                                    let is_c: bool =
                                        i.call_method1("iscoroutine", (&retval,))?.extract()?;
                                    let is_ag: bool =
                                        i.call_method1("isasyncgen", (&retval,))?.extract()?;
                                    Ok(is_c || is_ag)
                                })
                                .unwrap_or(false);
                        if is_coro {
                            let _ = retval.call_method0("close");
                            TestReport {
                                nodeid: item.nodeid.clone(),
                                phase: Phase::Call,
                                outcome: Outcome::Failed,
                                duration: call_started.elapsed(),
                                longrepr: Some(
                                    "async def functions are not natively supported.\n\
                                     You need to install a suitable plugin for your async framework, \
                                     for example:\n  - anyio\n  - pytest-asyncio\n  - pytest-tornasync\n  \
                                     - pytest-trio\n  - pytest-twisted"
                                        .to_string(),
                                ),
                                location: None,
                                subtest_desc: None,
                                sections: Vec::new(),
                                rerun: false,
                                xfail_longrepr: None,
                                reprcrash_message: None,
                                head_line: None,
                            }
                        } else {
                            if !retval.is_none() {
                                let _ = py
                                    .import("warnings")
                                    .and_then(|w| {
                                        let msg = format!(
                                            "Expected None, but {} returned {}, which will be an error in a future version of pytest.  Did you mean to use `assert` instead of `return`?",
                                            item.nodeid,
                                            retval.get_type().name().map_or("?".to_string(), |n| n.to_string()),
                                        );
                                        let cls = py
                                            .import("pytest")?
                                            .getattr("PytestReturnNotNoneWarning")?;
                                        w.call_method1("warn", (cls.call1((msg,))?,))
                                    });
                            }
                            TestReport {
                                nodeid: item.nodeid.clone(),
                                phase: Phase::Call,
                                outcome: Outcome::Passed,
                                duration: call_started.elapsed(),
                                longrepr: None,
                                location: None,
                                subtest_desc: None,
                                sections: python::log_failure_sections(py),
                                rerun: false,
                                xfail_longrepr: None,
                                reprcrash_message: None,
                                head_line: None,
                            }
                        }
                    }
                    Err(err) => {
                        if let Some(code) = python::session_abort_code(py, &err) {
                            record_abort(py, session, &err, code);
                            let (sub_reports, _) = python::pop_subtest_reports(py, config, item);
                            reports.extend(sub_reports);
                            return xfail;
                        }
                        raises_ok = xfail_raises_ok(py, &xfailed, &err);
                        call_err = Some(err.clone_ref(py));
                        report_from_err(py, config, item, Phase::Call, call_started, &err)
                    }
                }
            }
        }
        Err(err) => {
            raises_ok = xfail_raises_ok(py, &xfailed, &err);
            call_err = Some(err.clone_ref(py));
            report_from_err(py, config, item, Phase::Call, call_started, &err)
        }
    };
    // The test body may have added an xfail marker (node.add_marker).
    if xfailed.is_none() {
        let extra = added_marks(py);
        if !extra.is_empty() {
            xfailed = evaluate_xfail_marks(py, session, config, item, &extra).unwrap_or(None);
        }
    }
    let xfail = xfailed.is_some() && !runxfail;
    // @pytest.mark.xfail: expected failures invert at the call phase (when
    // the raises= filter matches); with strict=True an unexpected pass fails.
    let report = if let (Some(xf), true) = (&xfailed, xfail) {
        match report.outcome {
            Outcome::Failed if raises_ok => TestReport {
                outcome: Outcome::XFailed,
                // Keep the failure traceback so --xfail-tb can render it in the
                // XFAILURES section; longrepr becomes the xfail reason (the
                // short summary's "XFAIL nodeid - reason").
                xfail_longrepr: report.longrepr.clone(),
                longrepr: Some(xf.reason.clone()),
                // Clear the crash message: the short summary uses longrepr
                // (the reason) for XFAIL lines, not reprcrash_message.
                reprcrash_message: None,
                ..report
            },
            Outcome::Passed => {
                if xf.strict {
                    TestReport {
                        outcome: Outcome::Failed,
                        longrepr: Some(format!("[XPASS(strict)] {}", xf.reason)),
                        ..report
                    }
                } else {
                    TestReport {
                        outcome: Outcome::XPassed,
                        longrepr: Some(xf.reason.clone()),
                        ..report
                    }
                }
            }
            _ => report,
        }
    } else {
        report
    };
    if report.outcome == Outcome::Failed
        && config.get_flag("pdb")
        && let Some(err) = &call_err
    {
        python::maybe_pdb_interact(py, item, err);
    }
    // Subtests recorded during the call report individually before the
    // test's own report.  When the main test passed but subtests failed,
    // re-label it as FAILED with "contains N failed subtest(s)" — matching
    // pytest 9.0's built-in `_pytest.subtests` aggregation.
    // Skip re-labeling when the third-party pytest-subtests plugin is
    // installed: it keeps the main test PASSED and manages its own counts.
    let (sub_reports, failed_fixture_subs) = python::pop_subtest_reports(py, config, item);
    // In xdist worker mode, also drain reports that third-party plugins
    // (e.g. pytest-subtests) emitted via ihook.pytest_runtest_logreport
    // directly. The logreport sink captures them; we forward them to the
    // controller. In non-worker mode the TerminalReporter already handles
    // them via the hook relay, so we leave them in the sink to avoid
    // double-counting in the session summary.
    let plugin_reports = if config.is_worker() {
        python::drain_plugin_reports(py)
    } else {
        Vec::new()
    };
    // Only fixture-style subtest failures fail the enclosing test: unittest
    // subTest failures do not (pop_subtest_reports excludes them from the
    // count). Third-party plugin reports are fixture-style, so they count.
    let failed_sub_count = failed_fixture_subs
        + plugin_reports
            .iter()
            .filter(|r| r.outcome == Outcome::Failed)
            .count();
    reports.extend(sub_reports);
    reports.extend(plugin_reports);
    let has_subtests_plugin = py
        .eval(
            pyo3::ffi::c_str!("'pytest_subtests' in __import__('sys').modules"),
            None,
            None,
        )
        .and_then(|v| v.extract::<bool>())
        .unwrap_or(false);
    let report =
        if report.outcome == Outcome::Passed && failed_sub_count > 0 && !has_subtests_plugin {
            let suffix = if failed_sub_count > 1 { "s" } else { "" };
            TestReport {
                outcome: Outcome::Failed,
                longrepr: Some(format!(
                    "contains {failed_sub_count} failed subtest{suffix}"
                )),
                reprcrash_message: Some(format!(
                    "contains {failed_sub_count} failed subtest{suffix}"
                )),
                ..report
            }
        } else {
            report
        };
    // Unraisable exceptions surfaced during a passed call (upstream's
    // trylast pytest_runtest_call hookimpl, which pluggy skips when the
    // test itself raised): an error filter fails the call.
    let report = if report.outcome == Outcome::Passed {
        match python::unraisable_collect(py) {
            Ok(()) => report,
            Err(err) => report_from_err(py, config, item, Phase::Call, call_started, &err),
        }
    } else {
        report
    };
    let report = if report.outcome == Outcome::Passed {
        match python::threadexception_collect(py) {
            Ok(()) => report,
            Err(err) => report_from_err(py, config, item, Phase::Call, call_started, &err),
        }
    } else {
        report
    };
    reports.push(report);

    // Finalizers added via the test's own `request` run at function teardown.
    if let Some(request) = &test_request {
        for finalizer in request.borrow(py).take_finalizers() {
            session.finalizers.push(PendingFinalizer {
                scope: Scope::Function,
                instance: item.nodeid.clone(),
                finalizer: Finalizer::Callable(finalizer),
                bindings: Vec::new(),
            });
        }
    }
    xfail
}
