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
        let total = items.len().max(1);
        let mut done = 0usize;
        let mut prev_module: Option<String> = None;
        let mut prev_class: Option<String> = None;
        let mut current_file = String::new();
        let mut line = String::new();
        let mut failed = 0usize;

        for item in &items {
            if config.exitfirst && failed > 0 {
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
                teardown_scope(py, plugins, session, config, Scope::Class, prev, item);
            }
            prev_class = Some(class_instance);

            let module_instance = item.module_instance();
            if let Some(prev) = &prev_module
                && prev != &module_instance
            {
                teardown_scope(py, plugins, session, config, Scope::Module, prev, item);
                // Package-scoped fixtures are keyed per module instance.
                teardown_scope(py, plugins, session, config, Scope::Package, prev, item);
            }
            prev_module = Some(module_instance);

            let file = item
                .nodeid
                .split_once("::")
                .map(|(f, _)| f.to_string())
                .unwrap_or_default();
            if config.verbose == 0 && !config.quiet && !config.no_terminal() && file != current_file
            {
                if !current_file.is_empty() {
                    println!("{line} [{:>3}%]", done * 100 / total);
                }
                line = format!("{file} ");
                current_file = file;
            }

            let reports = run_one(py, plugins, session, config, item);
            done += 1;
            for report in reports {
                if report.outcome == Outcome::Failed {
                    failed += 1;
                }
                if config.no_terminal() {
                    // -p no:terminal: no progress output at all.
                } else if config.verbose > 0 {
                    let word = match report.outcome {
                        Outcome::Passed => "PASSED",
                        Outcome::Failed => "FAILED",
                        Outcome::Skipped => "SKIPPED",
                        Outcome::XFailed => "XFAIL",
                        Outcome::XPassed => "XPASS",
                    };
                    if report.phase == Phase::Call || report.outcome != Outcome::Passed {
                        println!("{} {} [{:>3}%]", item.nodeid, word, done * 100 / total);
                        let _ = std::io::stdout().flush();
                    }
                } else if !config.quiet
                    && let Some(c) = report.progress_char()
                {
                    line.push(c);
                }
                session.reports.push(report);
            }
        }
        if config.verbose == 0 && !config.quiet && !config.no_terminal() && !current_file.is_empty()
        {
            println!("{line} [{:>3}%]", done * 100 / total);
        }

        // Final scope teardowns.
        if let Some(prev) = &prev_class
            && let Some(last) = items.last()
        {
            teardown_scope(py, plugins, session, config, Scope::Class, prev, last);
        }
        if let Some(prev) = &prev_module
            && let Some(last) = items.last()
        {
            teardown_scope(py, plugins, session, config, Scope::Module, prev, last);
            teardown_scope(py, plugins, session, config, Scope::Package, prev, last);
        }
        if let Some(last) = items.last() {
            teardown_scope(py, plugins, session, config, Scope::Session, "", last);
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
    let mut reports = Vec::new();

    // @pytest.mark.skip / @pytest.mark.skipif
    if let Some(reason) = skip_reason(py, item) {
        let file = item.nodeid.split("::").next().unwrap_or("");
        reports.push(TestReport {
            nodeid: item.nodeid.clone(),
            phase: Phase::Setup,
            outcome: Outcome::Skipped,
            duration: Duration::ZERO,
            longrepr: Some(reason),
            // Marker skips report the item's definition site.
            location: Some(format!("{file}:{}", item.lineno)),
        });
        return reports;
    }
    // --runxfail: xfail marks (and imperative pytest.xfail, no-opped at
    // configure time) are ignored; tests report their real outcomes.
    let xfail = item.get_closest_marker("xfail").is_some() && !config.get_flag("runxfail");

    // Warnings emitted from here on are attributed to this item in the
    // warnings summary.
    let _ = py
        .import("pytest._wcapture")
        .and_then(|m| m.call_method1("set_current_test", (item.nodeid.as_str(),)));

    // One contextvars context per async item: fixtures + test share it,
    // and context changes stay isolated between async tests. Sync tests run
    // unisolated in the root context (pytest behavior), so their
    // contextvar mutations are visible to later tests.
    if item.is_coroutine && let Err(err) = python::begin_item_context(py) {
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
            reports.push(report_from_err(
                py,
                config,
                item,
                Phase::Setup,
                setup_started,
                &err,
            ));
            teardown_one(py, plugins, session, config, item, &mut reports);
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
        if names.is_empty() {
            println!("        {}", item.nodeid);
        } else {
            println!(
                "        {} (fixtures used: {})",
                item.nodeid,
                names.join(", ")
            );
        }
        if config.get_flag("setup-only") || config.get_flag("setup-plan") {
            // Fixtures only: tear down without calling the test.
            teardown_one(py, plugins, session, config, item, &mut reports);
            close_item_filters(py);
            python::end_item_context(py);
            return reports;
        }
    }

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

    // pytest.exit / Ctrl-C abort the session without a test outcome.
    if let Err(err) = &call_result
        && let Some(code) = python::session_abort_code(py, err)
    {
        session.exit_code_override = Some(code);
        teardown_one(py, plugins, session, config, item, &mut reports);
        close_item_filters(py);
        python::end_item_context(py);
        return reports;
    }

    let report = match call_result {
        Ok(true) => TestReport {
            nodeid: item.nodeid.clone(),
            phase: Phase::Call,
            outcome: Outcome::Passed,
            duration: call_started.elapsed(),
            longrepr: None,
            location: None,
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
                    },
                    Err(err) => {
                        if let Some(code) = python::session_abort_code(py, &err) {
                            session.exit_code_override = Some(code);
                            teardown_one(py, plugins, session, config, item, &mut reports);
                            close_item_filters(py);
                            python::end_item_context(py);
                            return reports;
                        }
                        report_from_err(py, config, item, Phase::Call, call_started, &err)
                    }
                }
            }
        }
        Err(err) => report_from_err(py, config, item, Phase::Call, call_started, &err),
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
            location: None,
        });
    } else {
        reports.push(TestReport {
            nodeid: item.nodeid.clone(),
            phase: Phase::Teardown,
            outcome: Outcome::Failed,
            duration: teardown_started.elapsed(),
            longrepr: Some(errors.join("\n")),
            location: None,
        });
    }
}

/// Run (LIFO) and remove every pending finalizer of the given scope instance.
/// Returns formatted errors. Also evicts cached fixture values of that
/// instance.
pub(crate) fn teardown_scope(
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
        .retain(|(_, _, inst, _), _| inst != instance);
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
    stack: &mut Vec<(String, String)>,
) -> PyResult<Py<PyAny>> {
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
        let display_name = match &fixture_param {
            Some((_, value)) => {
                let rendered = value
                    .bind(py)
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
        if dep_names.is_empty() {
            println!("{:indent$}SETUP    {scope_char} {display_name}", "");
        } else {
            println!(
                "{:indent$}SETUP    {scope_char} {display_name} (fixtures used: {})",
                "",
                dep_names.join(", ")
            );
        }
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
        }
    } else if python::is_skipped(py, err) {
        TestReport {
            nodeid: item.nodeid.clone(),
            phase,
            outcome: Outcome::Skipped,
            duration: started.elapsed(),
            longrepr: python::outcome_msg(py, err),
            // Imperative skips report where pytest.skip was raised.
            location: python::raise_location(py, err),
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
        }
    }
}

pub fn summary_line(reports: &[TestReport], warning_count: usize, elapsed: Duration) -> String {
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
    if warning_count > 0 {
        parts.push(format!(
            "{warning_count} warning{}",
            if warning_count == 1 { "" } else { "s" }
        ));
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
