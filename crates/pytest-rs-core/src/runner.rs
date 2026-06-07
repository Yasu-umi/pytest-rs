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
                    if !config.no_terminal()
                        && !config.quiet
                        && config.verbose == 0
                        && !session.live_logging
                        && !current_file.is_empty()
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
            if config.verbose == 0
                && !config.quiet
                && !config.no_terminal()
                && !session.live_logging
                && file != current_file
            {
                if !current_file.is_empty() {
                    println!(
                        "{}",
                        progress_suffix(&line, done, total, fill_color(py, session, false))
                    );
                }
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
                if report.progress_char().is_some() {
                    any_char = true;
                }
                if config.no_terminal() {
                    // -p no:terminal: no progress output at all.
                } else if config.verbose > 0 {
                    if report.phase == Phase::Call || report.outcome != Outcome::Passed {
                        let word = outcome_word(&report);
                        let plain = format!("{} {}", item.nodeid, word);
                        let rendered = format!(
                            "{} {}",
                            item.nodeid,
                            crate::tw::markup(&word, outcome_codes(&report))
                        );
                        println!(
                            "{rendered}{}",
                            progress_suffix(
                                &plain,
                                done,
                                total,
                                fill_color(py, session, done == total)
                            )
                        );
                        let _ = std::io::stdout().flush();
                    }
                } else if session.live_logging && !config.quiet {
                    // log_cli: outcome words print via live_flush (between
                    // the call phase and teardown logs).
                } else if i < session.streamed_chars {
                    // --setup-show already streamed this report's char
                    // (between the item line and the TEARDOWN narration).
                } else if !config.quiet
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
        if config.verbose == 0
            && !config.quiet
            && !config.no_terminal()
            && !session.live_logging
            && !current_file.is_empty()
            && (!setup_show_active(config) || any_char)
        {
            println!(
                "{}",
                progress_suffix(&line, done, total, fill_color(py, session, true))
            );
        }
        // pytest prints the banner even when the budget was spent on the
        // very last test, so check the final count rather than the break.
        if let Some(m) = maxfail
            && failed >= m
        {
            session.stopped_after = Some(failed);
        }

        session.items = items;
    }
}

/// log_cli live mode: print outcome words for reports not yet printed
/// (the call outcome appears between the call and teardown log sections).
fn live_flush(session: &mut Session, config: &Config, reports: &[TestReport]) {
    if !session.live_logging || config.verbose != 0 || config.quiet || config.no_terminal() {
        session.live_printed = reports.len();
        return;
    }
    let Some((done, total)) = session.live_progress else {
        session.live_printed = reports.len();
        return;
    };
    while session.live_printed < reports.len() {
        let report = &reports[session.live_printed];
        session.live_printed += 1;
        if report.phase == Phase::Call || report.outcome != Outcome::Passed {
            println!("{}", with_progress(&outcome_word(report), done, total));
            let _ = std::io::stdout().flush();
        }
    }
}

/// SGR codes for a report's outcome (progress letters, verbose words).
fn outcome_codes(report: &TestReport) -> &'static [u8] {
    use crate::tw;
    match report.outcome {
        Outcome::Passed => &[tw::GREEN],
        Outcome::Failed => &[tw::RED],
        Outcome::Skipped | Outcome::XFailed | Outcome::XPassed => &[tw::YELLOW],
    }
}

/// The progress-fill / summary main color from the session so far.
fn fill_color(py: Python<'_>, session: &Session, finished: bool) -> u8 {
    let mut failed = 0usize;
    let mut errors = 0usize;
    let mut xpassed = 0usize;
    let mut passed = 0usize;
    for report in &session.reports {
        match (report.phase, report.outcome) {
            (Phase::Call, Outcome::Passed) => passed += 1,
            (Phase::Call, Outcome::Failed) => failed += 1,
            (Phase::Setup | Phase::Teardown, Outcome::Failed) => errors += 1,
            (_, Outcome::XPassed) => xpassed += 1,
            _ => {}
        }
    }
    crate::tw::main_color(
        failed,
        errors,
        python::warning_count(py),
        xpassed,
        passed,
        finished,
    )
}

/// The verbose outcome word for a report: "PASSED", "SKIPPED (why)",
/// "SUBFAILED[desc]", ... (pytest appends reasons to skip/xfail words).
fn outcome_word(report: &TestReport) -> String {
    let reasoned = |word: &str| match report.longrepr.as_deref() {
        Some(reason) if !reason.is_empty() && !reason.contains('\n') => {
            format!("{word} ({reason})")
        }
        _ => word.to_string(),
    };
    if let Some(desc) = &report.subtest_desc {
        match report.outcome {
            Outcome::Failed => format!("SUBFAILED{desc}"),
            Outcome::Skipped => reasoned(&format!("SUBSKIPPED{desc}")),
            Outcome::XFailed => reasoned(&format!("SUBXFAIL{desc}")),
            _ => format!("SUBPASSED{desc}"),
        }
    } else {
        match report.outcome {
            Outcome::Passed => "PASSED".to_string(),
            Outcome::Failed => "FAILED".to_string(),
            Outcome::Skipped => reasoned("SKIPPED"),
            Outcome::XFailed => reasoned("XFAIL"),
            Outcome::XPassed => "XPASS".to_string(),
        }
    }
}

/// Terminal width for right-aligning the progress percentage, like
/// pytest's TerminalWriter (COLUMNS env, else 80).
fn term_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.trim().parse().ok())
        .unwrap_or(80)
}

/// The padding + percentage that completes an already-printed progress
/// line of `body`'s width (the body itself streamed char by char).
fn progress_suffix(body: &str, done: usize, total: usize, color: u8) -> String {
    let pct = format!("[{:>3}%]", done * 100 / total);
    let pad = term_width().saturating_sub(body.chars().count() + pct.len());
    let suffix = if pad > 0 {
        format!("{}{pct}", " ".repeat(pad))
    } else {
        format!(" {pct}")
    };
    crate::tw::markup(&suffix, &[color])
}

/// "body        [ 33%]" — the percentage right-aligned at the terminal edge.
fn with_progress(body: &str, done: usize, total: usize) -> String {
    let pct = format!("[{:>3}%]", done * 100 / total);
    let pad = term_width().saturating_sub(body.chars().count() + pct.len());
    if pad > 0 {
        format!("{body}{}{pct}", " ".repeat(pad))
    } else {
        format!("{body} {pct}")
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
        // Farthest mark first (module -> class -> function), like pytest.
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
                    .rev()
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

/// State for `request.getfixturevalue()`: raw pointers to the engine state
/// of the item currently running on this thread. Only dereferenced from
/// `getfixturevalue` while Python code called by the runner is on the stack —
/// the suspended Rust frames in between never touch the session concurrently.
struct ResolveCtx {
    plugins: *const [Box<dyn Plugin>],
    session: *mut Session,
    config: *const Config,
    item: *const TestItem,
    class_instance: Option<Py<PyAny>>,
}

thread_local! {
    static RESOLVE_CTX: std::cell::RefCell<Vec<ResolveCtx>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Pops the context pushed by `push_resolve_ctx` (kept alive for the whole
/// item run, teardown included).
pub(crate) struct ResolveCtxGuard(());

impl Drop for ResolveCtxGuard {
    fn drop(&mut self) {
        RESOLVE_CTX.with(|stack| {
            stack.borrow_mut().pop();
        });
    }
}

fn push_resolve_ctx(
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
    ResolveCtxGuard(())
}

/// Record the test's class instance once setup created it, so dynamically
/// requested fixtures with needs_instance bind to the right object.
fn set_resolve_ctx_instance(py: Python<'_>, instance: Option<&Py<PyAny>>) {
    RESOLVE_CTX.with(|stack| {
        if let Some(ctx) = stack.borrow_mut().last_mut() {
            ctx.class_instance = instance.map(|obj| obj.clone_ref(py));
        }
    });
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
    let mut stack = Vec::new();
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

/// Resolve one fixture by name for an item, using the cache, recursing into
/// dependencies, and letting plugins claim setup (async fixtures).
#[allow(clippy::too_many_arguments)]
fn resolve_fixture(
    py: Python<'_>,
    plugins: &[Box<dyn Plugin>],
    session: &mut Session,
    config: &Config,
    name: &str,
    item: &TestItem,
    class_instance: Option<&Py<PyAny>>,
    stack: &mut Vec<(String, String)>,
) -> PyResult<Py<PyAny>> {
    // Direct (non-indirect) parametrize of a fixture name: the callspec
    // value replaces the fixture outright, its function never runs
    // (pytest's PseudoFixtureDef bypass).
    if let Some((_, value)) = item.callspec.iter().find(|(param, _)| param == name) {
        return Ok(value.clone_ref(py));
    }
    let Some(def) = session.registry.lookup(name, &item.nodeid) else {
        // `pytestconfig` is a builtin backed by the Rust config, not a
        // shim-defined fixture (overridable like any other fixture).
        if name == "pytestconfig" {
            return python::make_py_config(py, config);
        }
        return Err(pyo3::exceptions::PyRuntimeError::new_err(format!(
            "fixture '{name}' not found for test {}",
            item.nodeid
        )));
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

/// Resolve a specific fixture definition (override-aware entry point).
#[allow(clippy::too_many_arguments)]
fn resolve_fixture_def(
    py: Python<'_>,
    plugins: &[Box<dyn Plugin>],
    session: &mut Session,
    config: &Config,
    def: std::sync::Arc<crate::fixture::FixtureDef>,
    item: &TestItem,
    class_instance: Option<&Py<PyAny>>,
    stack: &mut Vec<(String, String)>,
) -> PyResult<Py<PyAny>> {
    let def_id = (def.name.clone(), def.baseid.clone());
    if stack.contains(&def_id) {
        return Err(pyo3::exceptions::PyRuntimeError::new_err(format!(
            "recursive fixture dependency involving '{}'",
            def.name
        )));
    }

    let instance = match def.scope {
        Scope::Function => item.nodeid.clone(),
        Scope::Class => item.class_instance(),
        Scope::Module | Scope::Package => item.module_instance(),
        Scope::Session => String::new(),
    };
    // Parametrized fixtures cache per param index.
    let fixture_param: Option<(usize, Py<PyAny>)> = item
        .fixture_params
        .iter()
        .find(|(fixture, _, _)| fixture == &def.name)
        .map(|(_, index, value)| (*index, value.clone_ref(py)));
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
        keyed_name,
        def.baseid.clone(),
        instance.clone(),
        fixture_param.as_ref().map(|(index, _)| *index),
    );
    if let Some(cached) = session.fixture_cache.get(&cache_key) {
        return Ok(cached.clone_ref(py));
    }

    stack.push(def_id);
    let mut request: Option<Py<crate::request::PyRequest>> = None;
    let deps_result = (|| -> PyResult<Vec<(String, Py<PyAny>)>> {
        let mut kwargs = Vec::new();
        for dep in &def.param_names {
            if dep == "request" {
                let node = python::make_node(py, item)?;
                let req = Py::new(
                    py,
                    crate::request::PyRequest::new(
                        fixture_param.as_ref().map(|(_, value)| value.clone_ref(py)),
                        node,
                        Some(def.name.clone()),
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
    let (value, finalizer) = match claimed {
        Some(fixture_value) => (fixture_value.value, fixture_value.finalizer),
        None => {
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
                (value.unbind(), None)
            } else if def.is_generator {
                let generator = python::call_fixture(py, &def.func, fixture_instance, &kwargs)?;
                let value = python::next_value(py, &generator)?;
                (value.unbind(), Some(Finalizer::GenNext(generator.unbind())))
            } else {
                let value = python::call_fixture(py, &def.func, fixture_instance, &kwargs)?;
                (value.unbind(), None)
            }
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
            });
        }
    }

    // Finalizers registered through request.addfinalizer run at this
    // fixture's scope teardown, LIFO.
    if let Some(request) = &request {
        for finalizer in request.borrow(py).take_finalizers() {
            session.finalizers.push(PendingFinalizer {
                scope: def.scope,
                instance: instance.clone(),
                finalizer: Finalizer::Callable(finalizer),
            });
        }
    }
    if let Some(finalizer) = finalizer {
        session.finalizers.push(PendingFinalizer {
            scope: def.scope,
            instance: instance.clone(),
            finalizer,
        });
    }
    session.fixture_cache.insert(cache_key, value.clone_ref(py));
    Ok(value)
}

/// The item's marks as (name, mark) pairs for the pytest._skipping shim.
fn marks_for_eval(py: Python<'_>, item: &TestItem) -> Vec<(String, Py<PyAny>)> {
    item.marks
        .iter()
        .map(|mark| (mark.name.clone(), mark.obj.clone_ref(py)))
        .collect()
}

/// conftest pytest_markeval_namespace hook results (usually none).
fn markeval_namespaces(py: Python<'_>, session: &Session) -> Vec<Py<PyAny>> {
    let hooks: Vec<&crate::session::PyHook> = session
        .py_hooks
        .iter()
        .filter(|hook| hook.name == "pytest_markeval_namespace")
        .collect();
    if hooks.is_empty() {
        return Vec::new();
    }
    let config_obj = python::existing_py_config(py);
    hooks
        .iter()
        .filter_map(|hook| {
            let kwargs: Vec<(&str, Py<PyAny>)> = match &config_obj {
                Some(config) => vec![("config", config.clone_ref(py))],
                None => Vec::new(),
            };
            python::call_py_hook(py, &hook.func, &kwargs).ok()
        })
        .collect()
}

/// pytest evaluate_skip_marks: Some((reason, from_pytestmark)) when the item
/// should skip. Errors (bad mark usage, conditions) report as setup errors.
fn evaluate_skip_marks(
    py: Python<'_>,
    session: &Session,
    item: &TestItem,
) -> PyResult<Option<(String, bool)>> {
    if !item
        .marks
        .iter()
        .any(|mark| mark.name == "skip" || mark.name == "skipif")
    {
        return Ok(None);
    }
    let config_obj = python::existing_py_config(py).unwrap_or_else(|| py.None());
    py.import("pytest._skipping")?
        .call_method1(
            "evaluate_skip_marks",
            (
                marks_for_eval(py, item),
                item.module_name.as_str(),
                config_obj,
                markeval_namespaces(py, session),
            ),
        )?
        .extract()
}

/// Evaluated @pytest.mark.xfail data (pytest's Xfail).
struct XfailEval {
    reason: String,
    run: bool,
    strict: bool,
    raises: Option<Py<PyAny>>,
}

/// Fire conftest pytest_runtest_{setup,call,teardown} hooks for an item
/// (visibility-scoped by the conftest's directory, item kwarg).
/// pytest_runtest_logreport conftest hooks, fired once per report as it is
/// produced (pytest streams reports through this hook the same way).
pub(crate) fn fire_logreport_hooks(
    py: Python<'_>,
    session: &Session,
    report: &TestReport,
    lineno: Option<u32>,
) {
    let funcs: Vec<Py<PyAny>> = session
        .py_hooks
        .iter()
        .filter(|hook| {
            hook.name == "pytest_runtest_logreport"
                && report.nodeid.starts_with(hook.baseid.as_str())
        })
        .map(|hook| hook.func.clone_ref(py))
        .collect();
    let delegated = session.custom_reporter.is_some();
    if funcs.is_empty() && !delegated {
        return;
    }
    let proxy = match python::make_report_proxy(py, report, lineno) {
        Ok(proxy) => proxy,
        Err(err) => {
            eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
            return;
        }
    };
    for func in funcs {
        if let Err(err) = python::call_py_hook(py, &func, &[("report", proxy.clone_ref(py))]) {
            eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
        }
    }
    // The replacement reporter's pytest_runtest_logreport runs after the
    // conftest impls (pluggy LIFO: the reporter registered first).
    if delegated {
        python::reporter_logreport(py, proxy.bind(py));
    }
}

fn fire_runtest_py_hooks(
    py: Python<'_>,
    session: &Session,
    item: &TestItem,
    name: &str,
) -> PyResult<()> {
    let funcs: Vec<Py<PyAny>> = session
        .py_hooks
        .iter()
        .filter(|hook| hook.name == name && item.nodeid.starts_with(hook.baseid.as_str()))
        .map(|hook| hook.func.clone_ref(py))
        .collect();
    if funcs.is_empty() {
        return Ok(());
    }
    let node = python::make_node(py, item)?;
    for func in funcs {
        python::call_py_hook(py, &func, &[("item", node.clone_ref(py))])?;
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
fn start_runtest_py_wrappers(
    py: Python<'_>,
    session: &Session,
    item: &TestItem,
    name: &str,
    call_plain: bool,
) -> PyResult<Vec<Py<PyAny>>> {
    let funcs: Vec<Py<PyAny>> = session
        .py_hooks
        .iter()
        .filter(|hook| hook.name == name && item.nodeid.starts_with(hook.baseid.as_str()))
        .map(|hook| hook.func.clone_ref(py))
        .collect();
    if funcs.is_empty() {
        return Ok(Vec::new());
    }
    let node = python::make_node(py, item)?;
    let next_fn = py.import("builtins")?.getattr("next")?;
    let isgenfunc = py.import("inspect")?.getattr("isgeneratorfunction")?;
    let mut wrappers = Vec::new();
    for func in funcs {
        if !isgenfunc.call1((func.bind(py),))?.extract::<bool>()? {
            if call_plain {
                python::call_py_hook(py, &func, &[("item", node.clone_ref(py))])?;
            }
            continue;
        }
        let result = python::call_py_hook_raw(py, &func, &[("item", node.clone_ref(py))])?;
        let bound = result.bind(py);
        match next_fn.call1((bound,)) {
            Ok(_) => wrappers.push(result.clone_ref(py)),
            Err(err) if err.is_instance_of::<pyo3::exceptions::PyStopIteration>(py) => {}
            Err(err) => return Err(err),
        }
    }
    Ok(wrappers)
}

/// Finish started hookwrappers in reverse order (pluggy unwinds LIFO).
fn finish_runtest_py_wrappers(py: Python<'_>, wrappers: &[Py<PyAny>]) -> PyResult<()> {
    for wrapper in wrappers.iter().rev() {
        match wrapper.bind(py).call_method1("send", (py.None(),)) {
            Ok(_) => {
                return Err(pyo3::exceptions::PyRuntimeError::new_err(
                    "hook wrapper yielded more than once",
                ));
            }
            Err(err) if err.is_instance_of::<pyo3::exceptions::PyStopIteration>(py) => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

/// Marks added at runtime via node.add_marker / request.applymarker.
fn added_marks(py: Python<'_>) -> Vec<(String, Py<PyAny>)> {
    py.import("pytest._node")
        .and_then(|m| m.call_method0("added_marks"))
        .and_then(|marks| marks.extract())
        .unwrap_or_default()
}

/// `raises=` kwarg: only a matching exception counts as an expected failure.
fn xfail_raises_ok(py: Python<'_>, xfailed: &Option<XfailEval>, err: &PyErr) -> bool {
    match xfailed.as_ref().and_then(|xf| xf.raises.as_ref()) {
        Some(raises) => err.matches(py, raises.bind(py)).unwrap_or(false),
        None => true,
    }
}

/// pytest evaluate_xfail_marks: the first triggered xfail mark, if any.
/// `extra` carries dynamically added marks (closest, so they win).
fn evaluate_xfail_marks(
    py: Python<'_>,
    session: &Session,
    config: &Config,
    item: &TestItem,
    extra: &[(String, Py<PyAny>)],
) -> PyResult<Option<XfailEval>> {
    // Unmarked items (the common case) never enter Python.
    if !item.marks.iter().any(|mark| mark.name == "xfail")
        && !extra.iter().any(|(name, _)| name == "xfail")
    {
        return Ok(None);
    }
    // Strict default: strict_xfail, then strict, then the pre-9 xfail_strict.
    let strict_default = matches!(
        config
            .get_ini("strict_xfail")
            .or_else(|| config.get_ini("strict"))
            .or_else(|| config.get_ini("xfail_strict"))
            .map(str::trim),
        Some("true") | Some("True") | Some("1")
    );
    let config_obj = python::existing_py_config(py).unwrap_or_else(|| py.None());
    let mut marks: Vec<(String, Py<PyAny>)> = extra
        .iter()
        .map(|(name, obj)| (name.clone(), obj.clone_ref(py)))
        .collect();
    marks.extend(marks_for_eval(py, item));
    let result = py.import("pytest._skipping")?.call_method1(
        "evaluate_xfail_marks",
        (
            marks,
            item.module_name.as_str(),
            config_obj,
            strict_default,
            markeval_namespaces(py, session),
        ),
    )?;
    if result.is_none() {
        return Ok(None);
    }
    let (reason, run, strict, raises): (String, bool, bool, Option<Py<PyAny>>) =
        result.extract()?;
    Ok(Some(XfailEval {
        reason,
        run,
        strict,
        raises,
    }))
}

fn report_from_err(
    py: Python<'_>,
    config: &Config,
    item: &TestItem,
    phase: Phase,
    started: Instant,
    err: &PyErr,
) -> TestReport {
    if python::is_xfailed(py, err) {
        TestReport {
            nodeid: item.nodeid.clone(),
            phase,
            outcome: Outcome::XFailed,
            duration: started.elapsed(),
            longrepr: python::outcome_msg(py, err),
            location: None,
            subtest_desc: None,
            sections: Vec::new(),
        }
    } else if python::is_skipped(py, err) {
        // Imperative skips report where pytest.skip was raised; skips out
        // of fixtures/xunit setup report the item's definition site instead
        // (pytest's _use_item_location), so the user knows which test. An
        // explicit `_location` on the exception (unittest decorators) wins.
        let location = python::skip_location_override(py, err).or_else(|| {
            if phase == Phase::Setup {
                let file = item.nodeid.split("::").next().unwrap_or("");
                Some(format!("{file}:{}", item.lineno))
            } else {
                python::raise_location(py, err)
            }
        });
        TestReport {
            nodeid: item.nodeid.clone(),
            phase,
            outcome: Outcome::Skipped,
            duration: started.elapsed(),
            longrepr: python::outcome_msg(py, err),
            location,
            subtest_desc: None,
            sections: Vec::new(),
        }
    } else {
        TestReport {
            nodeid: item.nodeid.clone(),
            phase,
            outcome: Outcome::Failed,
            duration: started.elapsed(),
            longrepr: Some(python::format_test_failure(
                py,
                err,
                config.get_value("tb").unwrap_or("long"),
            )),
            location: None,
            subtest_desc: None,
            // "Captured stdout/log {when}" report sections; the terminal
            // appends them to the longrepr at render time.
            sections: python::log_failure_sections(py),
        }
    }
}

pub fn summary_line(
    reports: &[TestReport],
    deselected: usize,
    warning_count: usize,
    elapsed: Duration,
) -> String {
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut errors = 0usize;
    let mut skipped = 0usize;
    let mut xfailed = 0usize;
    let mut xpassed = 0usize;
    let mut subtests_passed = 0usize;
    for report in reports {
        // Passed subtests count their own category; other subtest outcomes
        // fold into the regular buckets (upstream report_teststatus).
        if report.subtest_desc.is_some() && report.outcome == Outcome::Passed {
            subtests_passed += 1;
            continue;
        }
        match (report.phase, report.outcome) {
            (Phase::Call, Outcome::Passed) => passed += 1,
            (Phase::Call, Outcome::Failed) => failed += 1,
            (Phase::Call, Outcome::Skipped) | (Phase::Setup, Outcome::Skipped) => skipped += 1,
            (Phase::Setup, Outcome::Failed) | (Phase::Teardown, Outcome::Failed) => errors += 1,
            (_, Outcome::XFailed) => xfailed += 1,
            (_, Outcome::XPassed) => xpassed += 1,
            _ => {}
        }
    }
    use crate::tw;
    let mut parts: Vec<(String, u8)> = Vec::new();
    if failed > 0 {
        parts.push((format!("{failed} failed"), tw::RED));
    }
    if passed > 0 {
        parts.push((format!("{passed} passed"), tw::GREEN));
    }
    if skipped > 0 {
        parts.push((format!("{skipped} skipped"), tw::YELLOW));
    }
    if subtests_passed > 0 {
        parts.push((format!("{subtests_passed} subtests passed"), tw::GREEN));
    }
    if deselected > 0 {
        parts.push((format!("{deselected} deselected"), tw::YELLOW));
    }
    if xfailed > 0 {
        parts.push((format!("{xfailed} xfailed"), tw::YELLOW));
    }
    if xpassed > 0 {
        parts.push((format!("{xpassed} xpassed"), tw::YELLOW));
    }
    if warning_count > 0 {
        parts.push((
            format!(
                "{warning_count} warning{}",
                if warning_count == 1 { "" } else { "s" }
            ),
            tw::YELLOW,
        ));
    }
    if errors > 0 {
        parts.push((
            format!("{errors} error{}", if errors == 1 { "" } else { "s" }),
            tw::RED,
        ));
    }
    if parts.is_empty() {
        parts.push(("no tests ran".to_string(), tw::YELLOW));
    }
    let main = tw::main_color(
        failed,
        errors,
        warning_count,
        xpassed,
        passed + subtests_passed,
        true,
    );
    let plain_parts: Vec<&str> = parts.iter().map(|(text, _)| text.as_str()).collect();
    let plain_body = format!(
        "{} in {:.2}s",
        plain_parts.join(", "),
        elapsed.as_secs_f64()
    );
    let banner = crate::engine::center_banner(&plain_body);
    if !tw::enabled() {
        return banner;
    }
    // pytest's nesting: the left banner segment opens the main color
    // without a reset, each count carries its own color (bold when it
    // matches the main color), the tail segments re-open the main color.
    let (left, right) = banner.split_once(&plain_body).unwrap_or_default();
    let mut out = String::new();
    out.push_str(&tw::open(&[main]));
    out.push_str(left);
    for (i, (text, color)) in parts.iter().enumerate() {
        if i > 0 {
            out.push_str(&tw::open(&[main]));
            out.push_str(", ");
        }
        let codes: &[u8] = if *color == main {
            &[*color, tw::BOLD]
        } else {
            &[*color]
        };
        out.push_str(&tw::markup(text, codes));
    }
    out.push_str(&tw::markup(
        &format!(" in {:.2}s", elapsed.as_secs_f64()),
        &[main],
    ));
    out.push_str(&tw::markup(right, &[main]));
    out
}

/// --setup-show display attributes: (scope letter, indent width).
fn scope_display(scope: Scope) -> (char, usize) {
    match scope {
        Scope::Session => ('S', 0),
        Scope::Package => ('P', 2),
        Scope::Module => ('M', 4),
        Scope::Class => ('C', 6),
        Scope::Function => ('F', 8),
    }
}

fn setup_show_active(config: &Config) -> bool {
    config.get_flag("setup-only") || config.get_flag("setup-plan") || config.get_flag("setup-show")
}
