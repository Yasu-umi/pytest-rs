use std::io::Write as _;
use std::time::{Duration, Instant};

use pyo3::prelude::*;

use crate::collect::TestItem;
use crate::config::Config;
use crate::engine::Engine;
use crate::fixture::Scope;
use crate::hooks::{HookContext, Plugin};
use crate::python;
use crate::report::{Outcome, Phase, TestReport};
use crate::session::{Finalizer, PendingFinalizer, Session};

mod fixtures;
mod hooks;
mod marks;
mod progress;

pub(crate) use fixtures::*;
pub(crate) use hooks::*;
pub(crate) use marks::*;
pub use progress::*;

impl Engine {
    pub(crate) fn run_items(&mut self, py: Python<'_>) {
        let Self {
            plugins,
            session,
            config,
            ..
        } = self;

        let items = std::mem::take(&mut session.items);
        let total = items.len().max(1);
        // Per-test progress display level (pytest's VERBOSITY_TEST_CASES):
        // >=1 → a line per test, ==0 → chars grouped under a file header,
        // <0 → bare chars with no file header.
        let tc = config.test_case_verbosity();
        // console_output_style progress field, and the per-file call-duration
        // accumulator that feeds it under the "times" style.
        let pkind = config.progress_kind();
        let mut file_dur = std::time::Duration::ZERO;
        let mut done = 0usize;
        let mut prev_module: Option<String> = None;
        let mut prev_class: Option<String> = None;
        let mut current_file = String::new();
        let mut line = String::new();
        // --setup-only prints no progress chars at all; pytest then also
        // omits the closing "[100%]" fill on the narration line.
        let mut any_char = false;
        let maxfail = config.maxfail();
        // --stepwise stops after the first failing item (--stepwise-skip
        // ignores the first one); the resume point persists via the cache.
        let stepwise = (config.get_flag("sw") || config.get_flag("sw-skip")) && maxfail.is_none();
        let sw_skip = config.get_flag("sw-skip");
        let mut sw_failed_items = 0usize;
        // Collection errors (--continue-on-collection-errors) already count
        // toward the --maxfail budget, like pytest's session.testsfailed.
        let mut failed = session
            .reports
            .iter()
            .filter(|r| r.outcome == Outcome::Failed)
            .count();

        // The last completed item: deferred scope-teardown failures report
        // under it, like pytest where those finalizers run inside it.
        let mut last_nodeid: Option<String> = None;
        // A failing deferred teardown becomes an ERROR report: count it
        // toward --maxfail and join its E to the previous progress chars.
        macro_rules! report_scope_teardown {
            ($scope:expr, $prev:expr, $item:expr) => {
                if let Some(report) = teardown_scope_reported(
                    py,
                    plugins,
                    session,
                    config,
                    $scope,
                    $prev,
                    $item,
                    last_nodeid.as_deref(),
                ) {
                    fire_logreport_hooks(py, session, &report, None);
                    failed += 1;
                    if !config.no_terminal() && tc <= 0 && !session.live_logging && !line.is_empty()
                    {
                        print!("E");
                        let _ = std::io::stdout().flush();
                        line.push('E');
                    }
                    session.reports.push(report);
                }
            };
        }

        for item in &items {
            if let Some(m) = maxfail
                && failed >= m
            {
                break;
            }
            // pytest.exit / Ctrl-C inside a test aborts the session.
            if session.exit_code_override.is_some() {
                break;
            }

            let class_instance = item.class_instance();
            if let Some(prev) = &prev_class
                && prev != &class_instance
            {
                report_scope_teardown!(Scope::Class, prev, item);
            }
            prev_class = Some(class_instance);

            let module_instance = item.module_instance();
            if let Some(prev) = &prev_module
                && prev != &module_instance
            {
                report_scope_teardown!(Scope::Module, prev, item);
                // Package-scoped fixtures are keyed per module instance.
                report_scope_teardown!(Scope::Package, prev, item);
            }
            prev_module = Some(module_instance);

            let file = item
                .nodeid
                .split_once("::")
                .map(|(f, _)| f.to_string())
                .unwrap_or_else(|| item.nodeid.clone());
            if tc == 0 && !config.no_terminal() && !session.live_logging && file != current_file {
                if !current_file.is_empty() {
                    let msg = progress_message(pkind, done, total, file_dur);
                    println!(
                        "{}",
                        progress_suffix(&line, &msg, fill_color(py, session, false))
                    );
                }
                file_dur = std::time::Duration::ZERO;
                line = format!("{file} ");
                print!("{line}");
                let _ = std::io::stdout().flush();
                current_file = file;
            }

            // log_cli: the item header prints on its own line up front so
            // live log records appear under it; pytest_runtest_logstart
            // hooks log under a "live log start" section.
            if session.live_logging {
                if !config.no_terminal() && !config.quiet && config.verbose == 0 {
                    println!("{} ", item.nodeid);
                    let _ = std::io::stdout().flush();
                }
                session.live_progress = Some((done + 1, total));
                python::log_set_live_when(py, "start");
            }
            let _ = fire_runtest_py_hooks(py, session, item, "pytest_runtest_logstart");
            if session.custom_reporter.is_some() && !config.is_worker() {
                python::reporter_logstart(py, item);
            }

            // Failed subtests share the --maxfail budget: tell the fixture
            // how many failures remain before it must stop swallowing.
            python::set_subtest_fail_budget(py, maxfail.map(|m| m.saturating_sub(failed)));
            session.live_printed = 0;
            session.streamed_chars = 0;
            let reports = run_one(py, plugins, session, config, item);
            live_flush(session, config, &reports);
            done += 1;
            last_nodeid = Some(item.nodeid.clone());
            let mut item_failed = false;
            for (i, report) in reports.into_iter().enumerate() {
                fire_logreport_hooks(py, session, &report, Some(item.lineno));
                if report.outcome == Outcome::Failed {
                    failed += 1;
                    item_failed = true;
                }
                // Accumulate call-phase time for the current file's "times"
                // progress field.
                if report.phase == Phase::Call {
                    file_dur += report.duration;
                }
                if report.progress_char().is_some() {
                    any_char = true;
                }
                if config.no_terminal() {
                    // -p no:terminal: no progress output at all.
                } else if tc >= 1 {
                    if report.phase == Phase::Call || report.outcome != Outcome::Passed {
                        // A pytest_report_teststatus hook may override the
                        // verbose word and its markup; otherwise use the
                        // built-in outcome word/color.
                        let status =
                            report_teststatus(py, session, &report, Some(item.lineno));
                        let word = status
                            .as_ref()
                            .map(|s| s.word.clone())
                            .unwrap_or_else(|| outcome_word(&report));
                        let codes = status
                            .as_ref()
                            .and_then(|s| s.markup.clone())
                            .unwrap_or_else(|| outcome_codes(&report).to_vec());
                        let plain = format!("{} {}", item.nodeid, word);
                        let rendered = format!(
                            "{} {}",
                            item.nodeid,
                            crate::tw::markup(&word, &codes)
                        );
                        // "times" in verbose mode reports each test's own
                        // duration (pytest's per-item showlongtestinfo).
                        let msg = progress_message(pkind, done, total, report.duration);
                        println!(
                            "{rendered}{}",
                            progress_suffix(&plain, &msg, fill_color(py, session, done == total))
                        );
                        let _ = std::io::stdout().flush();
                    }
                } else if session.live_logging && !config.quiet {
                    // log_cli: outcome words print via live_flush (between
                    // the call phase and teardown logs).
                } else if i < session.streamed_chars {
                    // --setup-show already streamed this report's char
                    // (between the item line and the TEARDOWN narration).
                } else if tc <= 0
                    && let Some(c) = report.progress_char()
                {
                    print!(
                        "{}",
                        crate::tw::markup(&c.to_string(), outcome_codes(&report))
                    );
                    let _ = std::io::stdout().flush();
                    line.push(c);
                }
                session.reports.push(report);
            }
            if session.live_logging {
                python::log_set_live_when(py, "finish");
            }
            let _ = fire_runtest_py_hooks(py, session, item, "pytest_runtest_logfinish");
            if session.custom_reporter.is_some() && !config.is_worker() {
                python::reporter_logfinish(py, item);
            }
            if stepwise && item_failed {
                sw_failed_items += 1;
                if !(sw_skip && sw_failed_items == 1) {
                    // Publish a truthy session.shouldstop so a conftest
                    // pytest_sessionfinish sees it (and cannot unset it).
                    python::set_session_shouldstop(py, "stepwise: stopping after first failure");
                    break;
                }
            }
            // Plugin-set session.shouldfail (pytest-timeout's session
            // deadline) aborts the run with its message banner.
            if let Some(msg) = python::session_shouldfail(py) {
                session.shouldfail = Some(msg);
                break;
            }
        }
        // Final scope teardowns, before the progress line closes so a
        // failing teardown's E joins the last test's progress chars.
        if let Some(prev) = &prev_class.clone()
            && let Some(last) = items.last()
        {
            report_scope_teardown!(Scope::Class, prev, last);
        }
        if let Some(prev) = &prev_module.clone()
            && let Some(last) = items.last()
        {
            report_scope_teardown!(Scope::Module, prev, last);
            report_scope_teardown!(Scope::Package, prev, last);
        }
        if let Some(last) = items.last() {
            report_scope_teardown!(Scope::Session, "", last);
        }
        if tc <= 0
            && !config.no_terminal()
            && !session.live_logging
            && !line.is_empty()
            && (!setup_show_active(config) || any_char)
        {
            let msg = progress_message(pkind, done, total, file_dur);
            println!(
                "{}",
                progress_suffix(&line, &msg, fill_color(py, session, true))
            );
        }
        // pytest prints the banner even when the budget was spent on the
        // very last test, so check the final count rather than the break.
        if let Some(m) = maxfail
            && failed >= m
        {
            session.stopped_after = Some(failed);
            // Publish a truthy session.shouldfail for conftest
            // pytest_sessionfinish (upstream sets the stop message here).
            python::set_session_shouldfail(py, &format!("stopping after {failed} failures"));
        }

        session.items = items;
    }
}

pub(crate) fn run_one(
    py: Python<'_>,
    plugins: &[Box<dyn Plugin>],
    session: &mut Session,
    config: &Config,
    item: &TestItem,
) -> Vec<TestReport> {
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
                    Instant::now(),
                    &err,
                )];
            }
        };
    let reports = run_one_body(py, plugins, session, config, item);
    if let Err(err) = finish_runtest_py_wrappers(py, &wrappers) {
        eprintln!(
            "pytest_runtest_protocol wrapper teardown failed for {}: {}",
            item.nodeid,
            err.value(py)
        );
    }
    reports
}

fn run_one_body(
    py: Python<'_>,
    plugins: &[Box<dyn Plugin>],
    session: &mut Session,
    config: &Config,
    item: &TestItem,
) -> Vec<TestReport> {
    let mut reports = Vec::new();

    // @pytest.mark.skip / @pytest.mark.skipif (mark-usage and condition
    // errors report as setup errors, like pytest.fail in runtest_setup).
    match evaluate_skip_marks(py, session, item) {
        Ok(Some((reason, module_level))) => {
            let file = item.nodeid.split("::").next().unwrap_or("");
            // Marker skips report the item's definition site; module-level
            // pytestmark skips fold per file, without a line number.
            let location = if module_level {
                file.to_string()
            } else {
                format!("{file}:{}", item.lineno)
            };
            reports.push(TestReport {
                nodeid: item.nodeid.clone(),
                phase: Phase::Setup,
                outcome: Outcome::Skipped,
                duration: Duration::ZERO,
                longrepr: Some(reason),
                location: Some(location),
                subtest_desc: None,
                sections: Vec::new(),
            });
            return reports;
        }
        Ok(None) => {}
        Err(err) => {
            reports.push(report_from_err(
                py,
                config,
                item,
                Phase::Setup,
                Instant::now(),
                &err,
            ));
            return reports;
        }
    }
    // @pytest.mark.xfail evaluation (conditions, run/strict/raises kwargs).
    // --runxfail ignores marks; tests report their real outcomes. Dynamic
    // marks (request.applymarker / node.add_marker) start fresh per item.
    let _ = py
        .import("pytest._node")
        .and_then(|m| m.call_method0("clear_added_marks"));
    let runxfail = config.get_flag("runxfail");
    let mut xfailed = match evaluate_xfail_marks(py, session, config, item, &[]) {
        Ok(xfailed) => xfailed,
        Err(err) => {
            reports.push(report_from_err(
                py,
                config,
                item,
                Phase::Setup,
                Instant::now(),
                &err,
            ));
            return reports;
        }
    };
    // run=False: report XFAIL without setting up or calling the test.
    if let Some(xf) = &xfailed
        && !runxfail
        && !xf.run
    {
        reports.push(TestReport {
            nodeid: item.nodeid.clone(),
            phase: Phase::Setup,
            outcome: Outcome::XFailed,
            duration: Duration::ZERO,
            longrepr: Some(format!("[NOTRUN] {}", xf.reason)),
            location: None,
            subtest_desc: None,
            sections: Vec::new(),
        });
        return reports;
    }
    let xfail = xfailed.is_some() && !runxfail;

    // request.getfixturevalue() support: expose this item's engine state to
    // Python for the duration of the run (popped when the guard drops).
    let _resolve_ctx = push_resolve_ctx(plugins, session, config, item);

    // Warnings emitted from here on are attributed to this item in the
    // warnings summary.
    let _ = py
        .import("pytest._wcapture")
        .and_then(|m| m.call_method1("set_current_test", (item.nodeid.as_str(),)));

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
            Instant::now(),
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
                Instant::now(),
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

    // ---- setup -----------------------------------------------------------
    // Per-phase log capture (caplog records + "Captured log" sections).
    let log_level_cfg: Option<String> = config
        .get_value("log-level")
        .map(str::to_string)
        .or_else(|| config.get_ini("log_level").map(str::to_string));
    python::log_start_phase(py, "setup", log_level_cfg.as_deref());
    let setup_started = Instant::now();
    type SetupOk = (
        Py<PyAny>,
        Vec<(String, Py<PyAny>)>,
        Option<Py<crate::request::PyRequest>>,
    );
    let setup_result = (|| -> PyResult<SetupOk> {
        {
            let mut ctx = HookContext {
                py,
                session,
                config,
            };
            for plugin in plugins {
                plugin.pytest_runtest_setup(&mut ctx, item)?;
            }
        }
        fire_runtest_py_hooks(py, session, item, "pytest_runtest_setup")?;
        // A fresh class instance per test (pytest behavior). For
        // unittest.TestCase items the shim runner creates the case;
        // exposing it here lets @pytest.fixture METHODS on the TestCase
        // bind to the instance the test runs on (upstream item.instance).
        let instance: Option<Py<PyAny>> = match &item.cls {
            Some(cls) => Some(cls.bind(py).call0()?.unbind()),
            None => {
                let func = item.func.bind(py);
                if func.hasattr("make_case")? {
                    Some(func.call_method0("make_case")?.unbind())
                } else {
                    None
                }
            }
        };
        // unittest items keep the shim runner (setUp/tearDown/skip
        // handling) — only pytest classes rebind the method on the
        // fresh instance.
        let callable = match (&item.cls, &instance) {
            (Some(_), Some(instance)) => {
                instance.bind(py).getattr(item.func_name.as_str())?.unbind()
            }
            _ => item.func.clone_ref(py),
        };

        set_resolve_ctx_instance(py, instance.as_ref());

        // TestCase items poisoned by fixture parametrization error at setup
        // with upstream's bare nofuncargs message (no traceback).
        if let Ok(msg) = item.func.bind(py).getattr("_pytest_unsupported_fixtures") {
            let failed = py
                .import("pytest._outcomes")?
                .getattr("Failed")?
                .call1((msg,))?;
            failed.setattr("pytrace", false)?;
            return Err(PyErr::from_value(failed));
        }

        // xunit-style setup_module/setup_class/setup_method/setup_function.
        python::ensure_xunit_setup(py, session, item, instance.as_ref())?;

        // autouse fixtures run first, then the requested ones.
        let mut stack = Vec::new();
        for def in session.registry.autouse_for(&item.nodeid) {
            resolve_fixture(
                py,
                plugins,
                session,
                config,
                &def.name,
                item,
                instance.as_ref(),
                &mut stack,
            )?;
        }
        // @pytest.mark.usefixtures (and the usefixtures ini option): named
        // fixtures are set up before the test's own, values not passed in.
        // Mark order follows iter_markers (upstream _getusefixturesnames):
        // function marks, class marks base-class-first, module marks.
        let usefixtures: Vec<String> = config
            .get_ini("usefixtures")
            .map(|value| {
                value
                    .split_whitespace()
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
            .into_iter()
            .chain(
                item.marks
                    .iter()
                    .filter(|mark| mark.name == "usefixtures")
                    .flat_map(|mark| {
                        mark.obj
                            .bind(py)
                            .getattr("args")
                            .ok()
                            .and_then(|args| args.extract::<Vec<String>>().ok())
                            .unwrap_or_default()
                    }),
            )
            .collect();
        for name in &usefixtures {
            if item.callspec.iter().any(|(param, _)| param == name) {
                continue;
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
            )?;
        }
        let mut kwargs = Vec::new();
        let mut test_request: Option<Py<crate::request::PyRequest>> = None;
        for name in &item.fixture_names {
            if item.callspec.iter().any(|(param, _)| param == name) {
                continue;
            }
            if name == "request" {
                let node = python::make_node(py, item)?;
                let req = Py::new(py, crate::request::PyRequest::new(None, node, None))?;
                kwargs.push((name.clone(), req.clone_ref(py).into_any()));
                test_request = Some(req);
                continue;
            }
            let value = resolve_fixture(
                py,
                plugins,
                session,
                config,
                name,
                item,
                instance.as_ref(),
                &mut stack,
            )?;
            kwargs.push((name.clone(), value));
        }
        for (name, value) in &item.callspec {
            // A callspec name outside the signature parametrizes a closure
            // fixture (its value overrides the fixture, resolved on demand)
            // and is not passed to the test (pytest semantics).
            if item.fixture_names.iter().any(|fixture| fixture == name) {
                kwargs.push((name.clone(), value.clone_ref(py)));
            }
        }
        Ok((callable, kwargs, test_request))
    })();

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
            teardown_one(py, plugins, session, config, item, xfail, &mut reports);
            close_item_filters(py);
            python::end_item_context(py);
            return reports;
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
        teardown_one(py, plugins, session, config, item, xfail, &mut reports);
        close_item_filters(py);
        python::end_item_context(py);
        return reports;
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
        teardown_one(py, plugins, session, config, item, xfail, &mut reports);
        close_item_filters(py);
        python::end_item_context(py);
        return reports;
    }
    reports.push(TestReport {
        nodeid: item.nodeid.clone(),
        phase: Phase::Setup,
        outcome: Outcome::Passed,
        duration: setup_started.elapsed(),
        longrepr: None,
        location: None,
        subtest_desc: None,
        sections: Vec::new(),
    });

    if setup_show_active(config) {
        let mut names: Vec<String> = kwargs
            .iter()
            .map(|(name, _)| name.clone())
            .filter(|name| name != "request")
            .collect();
        for def in session.registry.autouse_for(&item.nodeid) {
            if !names.contains(&def.name) {
                names.push(def.name.clone());
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
            teardown_one(py, plugins, session, config, item, xfail, &mut reports);
            close_item_filters(py);
            python::end_item_context(py);
            return reports;
        }
    }

    // ---- call --------------------------------------------------------------
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
                });
                teardown_one(py, plugins, session, config, item, true, &mut reports);
                close_item_filters(py);
                python::end_item_context(py);
                return reports;
            }
        }
    }
    python::log_start_phase(py, "call", log_level_cfg.as_deref());
    let call_started = Instant::now();
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
            Ok(false)
        })()
    };
    // The original error wins over a wrapper-teardown one.
    let call_result = match finish_runtest_py_wrappers(py, &call_wrappers) {
        Ok(()) => call_result,
        Err(err) => call_result.and(Err(err)),
    };

    // Quiet subtest verbosity (default) hides non-failed subtest reports.
    let quiet_subtests = config
        .get_ini("verbosity_subtests")
        .map(|v| v.trim() == "0")
        .unwrap_or(config.verbose == 0);

    // pytest.exit / Ctrl-C abort the session without a test outcome.
    if let Err(err) = &call_result
        && let Some(code) = python::session_abort_code(py, err)
    {
        session.exit_code_override = Some(code);
        session.abort_banner = python::session_abort_banner(py, err);
        // Subtests recorded before the abort still report (e.g. pytest.exit
        // inside a subtest block records a failed subtest, then aborts).
        let (sub_reports, _) = python::pop_subtest_reports(py, config, item, quiet_subtests);
        reports.extend(sub_reports);
        teardown_one(py, plugins, session, config, item, xfail, &mut reports);
        close_item_filters(py);
        python::end_item_context(py);
        return reports;
    }

    let mut raises_ok = true;
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
                }
            } else {
                match python::call_with_kwargs(py, &callable, &kwargs) {
                    Ok(_) => TestReport {
                        nodeid: item.nodeid.clone(),
                        phase: Phase::Call,
                        outcome: Outcome::Passed,
                        duration: call_started.elapsed(),
                        longrepr: None,
                        location: None,
                        subtest_desc: None,
                        sections: python::log_failure_sections(py),
                    },
                    Err(err) => {
                        if let Some(code) = python::session_abort_code(py, &err) {
                            session.exit_code_override = Some(code);
                            session.abort_banner = python::session_abort_banner(py, &err);
                            let (sub_reports, _) =
                                python::pop_subtest_reports(py, config, item, quiet_subtests);
                            reports.extend(sub_reports);
                            teardown_one(py, plugins, session, config, item, xfail, &mut reports);
                            close_item_filters(py);
                            python::end_item_context(py);
                            return reports;
                        }
                        raises_ok = xfail_raises_ok(py, &xfailed, &err);
                        report_from_err(py, config, item, Phase::Call, call_started, &err)
                    }
                }
            }
        }
        Err(err) => {
            raises_ok = xfail_raises_ok(py, &xfailed, &err);
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
                longrepr: Some(xf.reason.clone()),
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
    // Subtests recorded during the call report individually before the
    // test's own report; a passed test containing failed subtests fails
    // (fixture subtests only; unittest subTest failures don't propagate).
    let (sub_reports, failed_subs) = python::pop_subtest_reports(py, config, item, quiet_subtests);
    reports.extend(sub_reports);
    let report = if failed_subs > 0 && report.outcome == Outcome::Passed {
        TestReport {
            outcome: Outcome::Failed,
            longrepr: Some(format!(
                "contains {failed_subs} failed subtest{}",
                if failed_subs > 1 { "s" } else { "" }
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
            });
        }
    }

    teardown_one(py, plugins, session, config, item, xfail, &mut reports);
    close_item_filters(py);
    python::end_item_context(py);
    reports
}

fn teardown_one(
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
        });
    }
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
        if has_finalizers {
            python::log_finish_item(py);
        }
        return None;
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

fn setup_show_active(config: &Config) -> bool {
    config.get_flag("setup-only") || config.get_flag("setup-plan") || config.get_flag("setup-show")
}
