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
        } = self;

        let items = std::mem::take(&mut session.items);
        let mut prev_module: Option<String> = None;
        let mut current_file = String::new();
        let mut failed = 0usize;

        for item in &items {
            if config.exitfirst && failed > 0 {
                break;
            }

            let module_instance = item.module_instance();
            if let Some(prev) = &prev_module
                && prev != &module_instance
            {
                teardown_scope(py, plugins, session, config, Scope::Module, prev, item);
            }
            prev_module = Some(module_instance);

            let file = item
                .nodeid
                .split_once("::")
                .map(|(f, _)| f.to_string())
                .unwrap_or_default();
            if config.verbose == 0 && !config.quiet && file != current_file {
                if !current_file.is_empty() {
                    println!();
                }
                print!("{file} ");
                current_file = file;
            }

            let reports = run_one(py, plugins, session, config, item);
            for report in reports {
                if report.outcome == Outcome::Failed {
                    failed += 1;
                }
                if config.verbose > 0 {
                    let word = match report.outcome {
                        Outcome::Passed => "PASSED",
                        Outcome::Failed => "FAILED",
                        Outcome::Skipped => "SKIPPED",
                        Outcome::XFailed => "XFAIL",
                        Outcome::XPassed => "XPASS",
                    };
                    if report.phase == Phase::Call || report.outcome != Outcome::Passed {
                        println!("{} {}", item.nodeid, word);
                    }
                } else if !config.quiet
                    && let Some(c) = report.progress_char()
                {
                    print!("{c}");
                    let _ = std::io::stdout().flush();
                }
                session.reports.push(report);
            }
        }
        if config.verbose == 0 && !config.quiet && !current_file.is_empty() {
            println!();
        }

        // Final scope teardowns.
        if let Some(prev) = &prev_module
            && let Some(last) = items.last()
        {
            teardown_scope(py, plugins, session, config, Scope::Module, prev, last);
        }
        if let Some(last) = items.last() {
            teardown_scope(py, plugins, session, config, Scope::Session, "", last);
        }

        session.items = items;
    }
}

fn run_one(
    py: Python<'_>,
    plugins: &[Box<dyn Plugin>],
    session: &mut Session,
    config: &Config,
    item: &TestItem,
) -> Vec<TestReport> {
    let mut reports = Vec::new();

    // @pytest.mark.skip / @pytest.mark.skipif
    if let Some(reason) = skip_reason(py, item) {
        reports.push(TestReport {
            nodeid: item.nodeid.clone(),
            phase: Phase::Setup,
            outcome: Outcome::Skipped,
            duration: Duration::ZERO,
            longrepr: Some(reason),
        });
        return reports;
    }
    let xfail = item.get_closest_marker("xfail").is_some();

    // ---- setup -----------------------------------------------------------
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
        // A fresh class instance per test (pytest behavior).
        let instance: Option<Py<PyAny>> = match &item.cls {
            Some(cls) => Some(cls.bind(py).call0()?.unbind()),
            None => None,
        };
        let callable = match &instance {
            Some(instance) => instance.bind(py).getattr(item.func_name.as_str())?.unbind(),
            None => item.func.clone_ref(py),
        };

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
            kwargs.push((name.clone(), value.clone_ref(py)));
        }
        Ok((callable, kwargs, test_request))
    })();

    let (callable, kwargs, test_request) = match setup_result {
        Ok(setup) => setup,
        Err(err) => {
            reports.push(report_from_err(py, item, Phase::Setup, setup_started, &err));
            teardown_one(py, plugins, session, config, item, &mut reports);
            return reports;
        }
    };
    reports.push(TestReport {
        nodeid: item.nodeid.clone(),
        phase: Phase::Setup,
        outcome: Outcome::Passed,
        duration: setup_started.elapsed(),
        longrepr: None,
    });

    // ---- call --------------------------------------------------------------
    let call_started = Instant::now();
    let call_result = (|| -> PyResult<bool> {
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
    })();

    let report = match call_result {
        Ok(true) => TestReport {
            nodeid: item.nodeid.clone(),
            phase: Phase::Call,
            outcome: Outcome::Passed,
            duration: call_started.elapsed(),
            longrepr: None,
        },
        Ok(false) => {
            if item.is_coroutine {
                TestReport {
                    nodeid: item.nodeid.clone(),
                    phase: Phase::Call,
                    outcome: Outcome::Skipped,
                    duration: call_started.elapsed(),
                    longrepr: Some("async def functions are not natively supported.".to_string()),
                }
            } else {
                match python::call_with_kwargs(py, &callable, &kwargs) {
                    Ok(_) => TestReport {
                        nodeid: item.nodeid.clone(),
                        phase: Phase::Call,
                        outcome: Outcome::Passed,
                        duration: call_started.elapsed(),
                        longrepr: None,
                    },
                    Err(err) => report_from_err(py, item, Phase::Call, call_started, &err),
                }
            }
        }
        Err(err) => report_from_err(py, item, Phase::Call, call_started, &err),
    };
    // @pytest.mark.xfail: expected failures invert at the call phase.
    let report = if xfail {
        match report.outcome {
            Outcome::Failed => TestReport {
                outcome: Outcome::XFailed,
                ..report
            },
            Outcome::Passed => TestReport {
                outcome: Outcome::XPassed,
                ..report
            },
            _ => report,
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

    teardown_one(py, plugins, session, config, item, &mut reports);
    reports
}

fn teardown_one(
    py: Python<'_>,
    plugins: &[Box<dyn Plugin>],
    session: &mut Session,
    config: &Config,
    item: &TestItem,
    reports: &mut Vec<TestReport>,
) {
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
        });
    } else {
        reports.push(TestReport {
            nodeid: item.nodeid.clone(),
            phase: Phase::Teardown,
            outcome: Outcome::Failed,
            duration: teardown_started.elapsed(),
            longrepr: Some(errors.join("\n")),
        });
    }
}

/// Run (LIFO) and remove every pending finalizer of the given scope instance.
/// Returns formatted errors. Also evicts cached fixture values of that
/// instance.
fn teardown_scope(
    py: Python<'_>,
    _plugins: &[Box<dyn Plugin>],
    session: &mut Session,
    _config: &Config,
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
            errors.push(python::format_exception(py, &err));
        }
    }
    session
        .fixture_cache
        .retain(|(_, inst, _), _| inst != instance);
    errors
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
    stack: &mut Vec<String>,
) -> PyResult<Py<PyAny>> {
    let Some(def) = session.registry.lookup(name, &item.nodeid) else {
        return Err(pyo3::exceptions::PyRuntimeError::new_err(format!(
            "fixture '{name}' not found for test {}",
            item.nodeid
        )));
    };
    if stack.contains(&def.name) {
        return Err(pyo3::exceptions::PyRuntimeError::new_err(format!(
            "recursive fixture dependency involving '{name}'"
        )));
    }

    let instance = match def.scope {
        Scope::Function | Scope::Class => item.nodeid.clone(),
        Scope::Module | Scope::Package => item.module_instance(),
        Scope::Session => String::new(),
    };
    // Parametrized fixtures cache per param index.
    let fixture_param: Option<(usize, Py<PyAny>)> = item
        .fixture_params
        .iter()
        .find(|(fixture, _, _)| fixture == &def.name)
        .map(|(_, index, value)| (*index, value.clone_ref(py)));
    let cache_key = (
        def.name.clone(),
        instance.clone(),
        fixture_param.as_ref().map(|(index, _)| *index),
    );
    if let Some(cached) = session.fixture_cache.get(&cache_key) {
        return Ok(cached.clone_ref(py));
    }

    stack.push(def.name.clone());
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
            let value = resolve_fixture(
                py,
                plugins,
                session,
                config,
                dep,
                item,
                class_instance,
                stack,
            )?;
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
                return Err(pyo3::exceptions::PyRuntimeError::new_err(format!(
                    "async fixture '{name}' requires an async plugin (pytest-rs-asyncio)"
                )));
            }
            if def.is_generator {
                let generator = python::call_fixture(py, &def.func, fixture_instance, &kwargs)?;
                let value = python::next_value(py, &generator)?;
                (value.unbind(), Some(Finalizer::GenNext(generator.unbind())))
            } else {
                let value = python::call_fixture(py, &def.func, fixture_instance, &kwargs)?;
                (value.unbind(), None)
            }
        }
    };

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

/// The skip reason for an item, from @pytest.mark.skip or a true
/// @pytest.mark.skipif condition.
fn skip_reason(py: Python<'_>, item: &TestItem) -> Option<String> {
    let mark_reason = |mark: &crate::collect::MarkData| -> String {
        mark.obj
            .bind(py)
            .getattr("kwargs")
            .and_then(|kwargs| kwargs.get_item("reason"))
            .and_then(|reason| reason.extract())
            .unwrap_or_default()
    };
    if let Some(mark) = item.get_closest_marker("skip") {
        return Some(mark_reason(mark));
    }
    for mark in item.marks.iter().filter(|m| m.name == "skipif") {
        let Ok(args) = mark.obj.bind(py).getattr("args") else {
            continue;
        };
        let Ok(iter) = args.try_iter() else { continue };
        for condition in iter.flatten() {
            let truthy = match condition.extract::<String>() {
                // String conditions evaluate in the test module's namespace.
                Ok(expr) => python::eval_in_module(py, &item.module_name, &expr).unwrap_or(true),
                Err(_) => condition.is_truthy().unwrap_or(false),
            };
            if truthy {
                return Some(mark_reason(mark));
            }
        }
    }
    None
}

fn report_from_err(
    py: Python<'_>,
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
        }
    } else if python::is_skipped(py, err) {
        TestReport {
            nodeid: item.nodeid.clone(),
            phase,
            outcome: Outcome::Skipped,
            duration: started.elapsed(),
            longrepr: python::outcome_msg(py, err),
        }
    } else {
        TestReport {
            nodeid: item.nodeid.clone(),
            phase,
            outcome: Outcome::Failed,
            duration: started.elapsed(),
            longrepr: Some(python::format_exception(py, err)),
        }
    }
}

pub fn summary_line(reports: &[TestReport], elapsed: Duration) -> String {
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut errors = 0usize;
    let mut skipped = 0usize;
    let mut xfailed = 0usize;
    let mut xpassed = 0usize;
    for report in reports {
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
    let mut parts = Vec::new();
    if failed > 0 {
        parts.push(format!("{failed} failed"));
    }
    if passed > 0 {
        parts.push(format!("{passed} passed"));
    }
    if skipped > 0 {
        parts.push(format!("{skipped} skipped"));
    }
    if xfailed > 0 {
        parts.push(format!("{xfailed} xfailed"));
    }
    if xpassed > 0 {
        parts.push(format!("{xpassed} xpassed"));
    }
    if errors > 0 {
        parts.push(format!(
            "{errors} error{}",
            if errors == 1 { "" } else { "s" }
        ));
    }
    if parts.is_empty() {
        parts.push("no tests ran".to_string());
    }
    let body = format!("{} in {:.2}s", parts.join(", "), elapsed.as_secs_f64());
    let color = if failed > 0 || errors > 0 {
        "\x1b[31m" // red
    } else {
        "\x1b[32m" // green
    };
    format!("{color}{}\x1b[0m", crate::engine::center_banner(&body))
}
