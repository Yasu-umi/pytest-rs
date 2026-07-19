use std::time::Duration;

use pyo3::prelude::*;

use super::super::*;
use super::body::{SetupOk, run_custom_item};
use crate::collect::TestItem;
use crate::config::Config;
use crate::hooks::{HookContext, Plugin};
use crate::python;
use crate::report::{Outcome, Phase, TestReport};
use crate::session::Session;

pub(crate) fn build_test_setup(
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
        (Some(_), Some(instance)) => instance.bind(py).getattr(item.func_name.as_str())?.unbind(),
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

    let mut stack = Vec::new();
    // Higher-scoped autouse fixtures (session/package/module/class) set up
    // before the xunit hooks, so e.g. a session autouse fixture is available to
    // setup_module/setup_method — pytest orders by scope, with those fixtures
    // preceding the module/class/function xunit setups.
    for def in session.registry.autouse_for(&item.nodeid) {
        if def.scope == crate::fixture::Scope::Function {
            continue;
        }
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

    // xunit-style setup_module/setup_class/setup_method/setup_function.
    python::ensure_xunit_setup(py, session, item, instance.as_ref())?;

    // @pytest.mark.usefixtures (and the usefixtures ini option): named
    // fixtures are set up before the test's own, values not passed in.
    // Mark order follows iter_markers (upstream _getusefixturesnames):
    // function marks, class marks base-class-first, module marks.
    // Warn about empty usefixtures() marks (the conformance suite's
    // _getusefixturesnames would, but the Rust engine processes marks
    // directly and must replicate the warning).
    for mark in item.marks.iter().filter(|m| m.name == "usefixtures") {
        let args: Vec<String> = mark
            .obj
            .bind(py)
            .getattr("args")
            .ok()
            .and_then(|a| a.extract::<Vec<String>>().ok())
            .unwrap_or_default();
        if args.is_empty() {
            let _ = python::warn_explicit_at(
                py,
                "PytestWarning",
                &format!(
                    "usefixtures() in {} without arguments has no effect",
                    item.nodeid
                ),
                &item.path.to_string_lossy(),
                item.lineno,
            );
        }
    }
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
    // Set up the test's fixtures in pytest's scope-sorted closure order
    // (getfixtureclosure: highest scope first, dependencies resolved on demand),
    // so the *execution* order matches upstream even when it differs from the
    // test's argument order — e.g. a dependency runs right before its dependent
    // rather than at the dependent's argument position. Autouse/usefixtures
    // already set up above reappear here as cache hits.
    let ignore: std::collections::HashSet<String> =
        item.callspec.iter().map(|(name, _)| name.clone()).collect();
    // Seed the closure with autouse + usefixtures + argnames (mirroring
    // upstream's initialnames = deduplicate(autousenames, usefixturesnames,
    // argnames)) so getfixtureclosure's scope sort orders a module-scope
    // usefixtures fixture ahead of a function-scope autouse one. setup is a
    // single closure walk; higher-scope autouse set up before xunit above
    // reappear here as cache hits.
    let mut requested: Vec<String> = usefixtures.clone();
    requested.extend(item.fixture_names.iter().cloned());
    // Names a pytest_collection_modifyitems hook injected into
    // node.fixturenames (e.g. pytest-order's --error-on-failed-ordering)
    // are attempted here too, erroring if unregistered — matching upstream,
    // where item.fixturenames itself drives fixture setup.
    requested.extend(item.injected_fixture_names.iter().cloned());
    let initialnames = session.registry.initial_names(&item.nodeid, &requested);
    let closure = session
        .registry
        .getfixtureclosure(&item.nodeid, &initialnames, &ignore);
    for name in &closure {
        if name == "request" || ignore.contains(name) {
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
    // The values passed to the test, in its signature order (now cached).
    let mut kwargs = Vec::new();
    let mut test_request: Option<Py<crate::request::PyRequest>> = None;
    for name in &item.fixture_names {
        if ignore.contains(name) {
            continue;
        }
        if name == "request" {
            let node = crate::runner::item_node(py, item)?;
            let req = Py::new(
                py,
                crate::request::PyRequest::new(
                    None,
                    node.clone_ref(py),
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
    let node = crate::runner::item_node(py, item)?;
    let request_for_node = match &test_request {
        Some(req) => req.clone_ref(py).into_any(),
        None => Py::new(
            py,
            crate::request::PyRequest::new(
                None,
                node.clone_ref(py),
                node.clone_ref(py),
                None,
                crate::fixture::Scope::Function,
            ),
        )?
        .into_any(),
    };
    node.bind(py).setattr("_request", request_for_node)?;
    Ok((callable, kwargs, test_request))
}

/// What @pytest.mark.skip/skipif/xfail and the custom-item check decide
/// before setup runs: either a finished outcome (`Done`) or the xfail
/// state to carry into setup/call (`Run`).
pub(crate) enum ItemPrelude {
    Done(Vec<TestReport>),
    Run {
        xfailed: Option<XfailEval>,
        runxfail: bool,
    },
}
pub(crate) fn evaluate_item_prelude(
    py: Python<'_>,
    session: &Session,
    config: &Config,
    item: &TestItem,
) -> ItemPrelude {
    if python::is_custom_item(py, &item.func) {
        return ItemPrelude::Done(run_custom_item(py, config, item));
    }

    let mut reports = Vec::new();

    // @pytest.mark.skip / @pytest.mark.skipif (mark-usage and condition
    // errors report as setup errors, like pytest.fail in runtest_setup).
    match evaluate_skip_marks(py, session, item) {
        Ok(Some((reason, module_level))) => {
            // Use invocation-dir-relative path so the SKIPPED summary shows
            // "tests/test_1.py:N" when rootdir is a subdirectory.
            let file = crate::collect::file_nodeid(&config.invocation_dir, &item.path, &[]);
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
                TimeMark::now(),
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
                TimeMark::now(),
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
