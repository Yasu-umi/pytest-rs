//! Per-item runtest protocol: skip/xfail gating, fixture setup, the call
//! phase, outcome classification, and teardown handoff. `run_one` wraps
//! `run_one_body`, which drives setup -> call -> outcome via run_item_body.

use std::io::Write as _;
use std::time::{Duration, Instant};

use pyo3::prelude::*;

use crate::collect::TestItem;
use crate::config::Config;
use crate::fixture::Scope;
use crate::hooks::{HookContext, Plugin};
use crate::python;
use crate::report::{Outcome, Phase, TestReport};
use crate::session::{Finalizer, PendingFinalizer, Session};

use super::*;

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
                    Instant::now(),
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
    if let Err(err) = finish_runtest_py_wrappers(py, &wrappers) {
        eprintln!(
            "pytest_runtest_protocol wrapper teardown failed for {}: {}",
            item.nodeid,
            err.value(py)
        );
    }
    reports
}

/// Run a custom collector item (pytest.Item subclass) via the shim's
/// run_custom_item, mapping its (when, outcome, longrepr) tuples to reports.
fn run_custom_item(py: Python<'_>, config: &Config, item: &TestItem) -> Vec<TestReport> {
    let started = Instant::now();
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
type SetupOk = (
    Py<PyAny>,
    Vec<(String, Py<PyAny>)>,
    Option<Py<crate::request::PyRequest>>,
);

fn build_test_setup(
    py: Python<'_>,
    plugins: &[Box<dyn Plugin>],
    session: &mut Session,
    config: &Config,
    item: &TestItem,
) -> PyResult<SetupOk> {
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
        // `request` is the always-available pseudo-fixture, never in the
        // registry; usefixtures("request") (pytest-bdd marks scenario
        // functions that declare a `request` arg this way) is a no-op.
        if name == "request" {
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
            let node = crate::runner::item_node(py, item)?;
            let req = Py::new(
                py,
                crate::request::PyRequest::new(
                    None,
                    node,
                    None,
                    crate::fixture::Scope::Function,
                ),
            )?;
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
}

/// What @pytest.mark.skip/skipif/xfail and the custom-item check decide
/// before setup runs: either a finished outcome (`Done`) or the xfail
/// state to carry into setup/call (`Run`).
enum ItemPrelude {
    Done(Vec<TestReport>),
    Run {
        xfailed: Option<XfailEval>,
        runxfail: bool,
    },
}

fn evaluate_item_prelude(
    py: Python<'_>,
    session: &Session,
    config: &Config,
    item: &TestItem,
) -> ItemPrelude {
    let is_custom_item = py
        .import("pytest._node")
        .and_then(|m| m.getattr("Item"))
        .and_then(|cls| item.func.bind(py).is_instance(&cls))
        .unwrap_or(false);
    if is_custom_item {
        return ItemPrelude::Done(run_custom_item(py, config, item));
    }

    let mut reports = Vec::new();

    // @pytest.mark.skip / @pytest.mark.skipif (mark-usage and condition
    // errors report as setup errors, like pytest.fail in runtest_setup).
    match evaluate_skip_marks(py, session, item) {
        Ok(Some((reason, module_level))) => {
            // Use invocation-dir-relative path so the SKIPPED summary shows
            // "tests/test_1.py:N" when rootdir is a subdirectory.
            let file = crate::collect::file_nodeid(&config.invocation_dir, &item.path);
            // Marker skips report the item's definition site; module-level
            // pytestmark skips fold per file, without a line number.
            let location = if module_level {
                file
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
                rerun: false,
                xfail_longrepr: None,
                reprcrash_message: None,
                head_line: None,
            });
            return ItemPrelude::Done(reports);
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
            return ItemPrelude::Done(reports);
        }
    }
    // @pytest.mark.xfail evaluation (conditions, run/strict/raises kwargs).
    // --runxfail ignores marks; tests report their real outcomes. Dynamic
    // marks (request.applymarker / node.add_marker) start fresh per item.
    let _ = py
        .import("pytest._node")
        .and_then(|m| m.call_method0("clear_added_marks"));
    let runxfail = config.get_flag("runxfail");
    let xfailed = match evaluate_xfail_marks(py, session, config, item, &[]) {
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
            return ItemPrelude::Done(reports);
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
            rerun: false,
            xfail_longrepr: None,
            reprcrash_message: None,
            head_line: None,
        });
        return ItemPrelude::Done(reports);
    }
    ItemPrelude::Run { xfailed, runxfail }
}

pub(crate) fn run_one_body(
    py: Python<'_>,
    plugins: &[Box<dyn Plugin>],
    session: &mut Session,
    config: &Config,
    item: &TestItem,
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

    let teardown_xfail =
        run_item_body(py, plugins, session, config, item, &mut reports, xfailed, runxfail, xfail);
    teardown_one(py, plugins, session, config, item, teardown_xfail, &mut reports);
    close_item_filters(py);
    python::end_item_context(py);
    reports
}

/// Run the setup -> call -> outcome phases for one item, pushing each
/// phase report into `reports`. Returns the xfail flag the caller's
/// teardown should use (a NOTRUN-at-call forces it on). Teardown and the
/// filter/context close are the caller's single trailing step.
fn run_item_body(
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
    python::log_start_phase(py, "setup", log_level_cfg.as_deref());
    let setup_started = Instant::now();
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
    reports.push(TestReport {
        nodeid: item.nodeid.clone(),
        phase: Phase::Setup,
        outcome: Outcome::Passed,
        duration: setup_started.elapsed(),
        longrepr: None,
        location: None,
        subtest_desc: None,
        sections: Vec::new(),
        rerun: false,
        xfail_longrepr: None,
        reprcrash_message: None,
        head_line: None,
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
            return xfail;
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
        return xfail;
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
                    Ok(_) => TestReport {
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
                    Err(err) => {
                        if let Some(code) = python::session_abort_code(py, &err) {
                            session.exit_code_override = Some(code);
                            session.abort_banner = python::session_abort_banner(py, &err);
                            let (sub_reports, _) =
                                python::pop_subtest_reports(py, config, item, quiet_subtests);
                            reports.extend(sub_reports);
                            return xfail;
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
    xfail
}
