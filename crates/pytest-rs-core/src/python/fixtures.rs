//! Fixture registration/expansion and the xunit setup hooks.

#[allow(unused_imports)]
use super::*;
use crate::collect::{MarkData, TestItem};
use crate::fixture::{FixtureDef, FixtureRegistry, Scope};
use pyo3::types::PyModule;

/// Register the shim's builtin fixtures (tmp_path, monkeypatch, pytester,
/// doctest_namespace) with global visibility.
pub fn register_builtin_fixtures(py: Python<'_>, registry: &mut FixtureRegistry) -> PyResult<()> {
    let pytest_module = py.import("pytest")?;
    register_fixtures_from(py, &pytest_module, "", registry)?;
    let doctest_module = py.import("_pytest.doctest")?;
    register_fixtures_from(py, &doctest_module, "", registry)?;
    Ok(())
}

/// xunit setup state per module/class: None = setup succeeded; Some(exc) =
/// it raised, and every later test of that scope re-raises the same error
/// (pytest's cached-failure semantics).
#[derive(Default)]
pub(crate) struct XunitState {
    pub(crate) modules: std::collections::HashMap<String, Option<Py<PyAny>>>,
    pub(crate) classes: std::collections::HashMap<String, Option<Py<PyAny>>>,
}

/// The cached setup status for a key: None = never ran, Some(None) = ok,
/// Some(Some(exc)) = failed with exc.
pub(crate) fn xunit_status(
    py: Python<'_>,
    session: &crate::session::Session,
    class_scope: bool,
    key: &str,
) -> Option<Option<Py<PyAny>>> {
    let state = session.stash_get::<XunitState>()?;
    let map = if class_scope {
        &state.classes
    } else {
        &state.modules
    };
    map.get(key)
        .map(|status| status.as_ref().map(|exc| exc.clone_ref(py)))
}

pub(crate) fn xunit_record(
    py: Python<'_>,
    session: &mut crate::session::Session,
    class_scope: bool,
    key: String,
    error: Option<&PyErr>,
) {
    let state = session
        .stash_get_mut::<XunitState>()
        .expect("xunit state inserted");
    let map = if class_scope {
        &mut state.classes
    } else {
        &mut state.modules
    };
    map.insert(
        key,
        error.map(|err| err.value(py).clone().unbind().into_any()),
    );
}

/// xunit-style setup hooks (setup_module / setup_function for plain
/// functions, setup_class / setup_method for Test classes). Teardowns are
/// pushed onto the session finalizer stack at the matching scope, so they
/// drain LIFO with the fixture finalizers (class scope drains when the
/// runner moves to the next class).
pub fn ensure_xunit_setup(
    py: Python<'_>,
    session: &mut crate::session::Session,
    item: &TestItem,
    instance: Option<&Py<PyAny>>,
) -> PyResult<()> {
    // Doctest text file items have no module to import; skip xunit hooks.
    if item.module_name == "__doctest_textfile__" {
        return Ok(());
    }
    let xunit = py.import("pytest._xunit")?;
    let call_optional = xunit.getattr("call_optional")?;
    let bind = xunit.getattr("bind")?;
    let module = py.import(item.module_name.as_str())?;
    let module_instance = item.module_instance();

    if session.stash_get::<XunitState>().is_none() {
        session.stash_insert(XunitState::default());
    }

    match xunit_status(py, session, false, &module_instance) {
        Some(Some(exc)) => {
            // setup_module already failed: every test re-raises that error.
            return Err(PyErr::from_value(exc.bind(py).clone()));
        }
        Some(None) => {}
        None => {
            // unittest's module-level aliases take priority (upstream's
            // ("setUpModule", "setup_module") first-non-fixture lookup).
            let setup_fn = ["setUpModule", "setup_module"]
                .iter()
                .find_map(|name| module.getattr(name).ok());
            let setup_result: PyResult<()> = match setup_fn {
                Some(setup) => call_optional.call1((setup, &module)).map(|_| ()),
                None => Ok(()),
            };
            if let Err(err) = setup_result {
                let err = map_skiptest(py, err);
                xunit_record(py, session, false, module_instance, Some(&err));
                return Err(err);
            }
            xunit_record(py, session, false, module_instance.clone(), None);
            let teardown_fn = ["tearDownModule", "teardown_module"]
                .iter()
                .find_map(|name| module.getattr(name).ok());
            if let Some(teardown) = teardown_fn {
                let finalizer = bind.call1((teardown, &module))?;
                session.finalizers.push(crate::session::PendingFinalizer {
                    scope: Scope::Module,
                    instance: module_instance.clone(),
                    finalizer: crate::session::Finalizer::Callable(finalizer.unbind()),
                    bindings: Vec::new(),
                });
            }
        }
    }

    match (&item.cls, instance) {
        (Some(cls), Some(instance)) => {
            let cls = cls.bind(py);
            let class_key = item.class_instance();
            match xunit_status(py, session, true, &class_key) {
                Some(Some(exc)) => {
                    return Err(PyErr::from_value(exc.bind(py).clone()));
                }
                Some(None) => {}
                None => {
                    let setup_result: PyResult<()> = match cls.getattr("setup_class") {
                        Ok(setup) => call_optional.call1((setup, cls)).map(|_| ()),
                        Err(_) => Ok(()),
                    };
                    if let Err(err) = setup_result {
                        xunit_record(py, session, true, class_key, Some(&err));
                        return Err(err);
                    }
                    xunit_record(py, session, true, class_key, None);
                    if let Ok(teardown) = cls.getattr("teardown_class") {
                        let finalizer = bind.call1((teardown, cls))?;
                        session.finalizers.push(crate::session::PendingFinalizer {
                            scope: Scope::Class,
                            instance: item.class_instance(),
                            finalizer: crate::session::Finalizer::Callable(finalizer.unbind()),
                            bindings: Vec::new(),
                        });
                    }
                }
            }
            let instance = instance.bind(py);
            // pytest passes the *bound* method object to setup/teardown_method.
            let bound_method = instance.getattr(item.func_name.as_str())?;
            if let Ok(setup) = instance.getattr("setup_method") {
                call_optional.call1((setup, &bound_method))?;
            }
            if let Ok(teardown) = instance.getattr("teardown_method") {
                let finalizer = bind.call1((teardown, &bound_method))?;
                session.finalizers.push(crate::session::PendingFinalizer {
                    scope: Scope::Function,
                    instance: item.nodeid.clone(),
                    finalizer: crate::session::Finalizer::Callable(finalizer.unbind()),
                    bindings: Vec::new(),
                });
            }
        }
        _ => {
            if let Ok(setup) = module.getattr("setup_function") {
                call_optional.call1((setup, item.func.bind(py)))?;
            }
            if let Ok(teardown) = module.getattr("teardown_function") {
                let finalizer = bind.call1((teardown, item.func.bind(py)))?;
                session.finalizers.push(crate::session::PendingFinalizer {
                    scope: Scope::Function,
                    instance: item.nodeid.clone(),
                    finalizer: crate::session::Finalizer::Callable(finalizer.unbind()),
                    bindings: Vec::new(),
                });
            }
        }
    }
    Ok(())
}

/// Register fixtures defined by a plugin-provided Python module (e.g. the
/// pytest_mock shim) with global visibility.
pub fn register_plugin_fixtures(
    py: Python<'_>,
    module: &Bound<'_, PyModule>,
    registry: &mut FixtureRegistry,
) -> PyResult<()> {
    register_fixtures_from(py, module, "", registry)
}

pub(crate) fn register_fixtures_from(
    py: Python<'_>,
    module: &Bound<'_, PyModule>,
    baseid: &str,
    registry: &mut FixtureRegistry,
) -> PyResult<()> {
    register_fixtures_from_skip(py, module, baseid, registry, &[])
}

pub(crate) fn register_fixtures_from_skip(
    py: Python<'_>,
    module: &Bound<'_, PyModule>,
    baseid: &str,
    registry: &mut FixtureRegistry,
    skip_names: &[&str],
) -> PyResult<()> {
    for (key, value) in module.dict().iter() {
        let Ok(attr_name) = key.extract::<String>() else {
            continue;
        };
        if skip_names.contains(&attr_name.as_str()) {
            continue;
        }
        if !value.is_callable() || !value.hasattr("_pytestfixturefunction")? {
            continue;
        }
        register_fixture_def(py, &attr_name, &value, baseid, false, registry)?;
    }
    Ok(())
}

pub(crate) fn register_fixture_def(
    py: Python<'_>,
    attr_name: &str,
    value: &Bound<'_, PyAny>,
    baseid: &str,
    needs_instance: bool,
    registry: &mut FixtureRegistry,
) -> PyResult<()> {
    let marker = value.getattr("_pytestfixturefunction")?;
    // Defensive: objects faking the marker attribute (stubs, mocks) are
    // skipped rather than failing collection.
    let Ok(scope_str) = marker
        .getattr("scope")
        .and_then(|scope| scope.extract::<String>())
    else {
        return Ok(());
    };
    let scope_str: String = scope_str;
    // An invalid scope name (e.g. scope="functions") fails in pytest's
    // Scope.from_user at FixtureDef construction. We keep a placeholder scope
    // and defer the failure to resolution (so collection still proceeds),
    // matching pytest's message: "Fixture 'NAME' from WHERE got an unexpected
    // scope value 'SCOPE'".
    let (scope, scope_error) = match Scope::parse(&scope_str) {
        Some(scope) => (scope, None),
        None => {
            let func_name = value
                .getattr("__name__")
                .and_then(|n| n.extract::<String>())
                .unwrap_or_else(|_| attr_name.to_string());
            let where_ = baseid.trim_end_matches("::");
            let from = if where_.is_empty() {
                String::new()
            } else {
                format!("from {where_} ")
            };
            (
                Scope::Function,
                Some(format!(
                    "Fixture '{func_name}' {from}got an unexpected scope value '{scope_str}'"
                )),
            )
        }
    };
    // autouse is used for its truthiness (pytest accepts e.g. autouse="True").
    let autouse: bool = marker.getattr("autouse")?.is_truthy()?;
    let explicit_name: Option<String> = marker.getattr("name")?.extract()?;
    let name = explicit_name.unwrap_or_else(|| attr_name.to_string());
    let flags = async_flags(py, value)?;
    let mut param_names = param_names(py, value)?;
    // Binding to the test instance consumes the first parameter whatever
    // its name (upstream fixtures occasionally spell it `cls`).
    if needs_instance && !param_names.is_empty() {
        param_names.remove(0);
    }
    let params_obj = marker.getattr("params")?;
    let params = if params_obj.is_none() {
        None
    } else {
        Some(params_obj.unbind())
    };
    let ids = match marker.getattr("ids") {
        Ok(ids_obj) if !ids_obj.is_none() => Some(ids_obj.unbind()),
        _ => None,
    };
    registry.register(FixtureDef {
        name,
        func: value.clone().unbind(),
        scope,
        autouse,
        is_coroutine: flags.is_coroutine,
        is_generator: flags.is_generator,
        is_async_gen: flags.is_async_gen,
        param_names,
        baseid: baseid.to_string(),
        needs_instance,
        params,
        ids,
        scope_error,
    });
    Ok(())
}

/// Unwrap a pytest.param(...) entry in @pytest.fixture(params=[...]) into
/// (value, explicit id, extra item marks). Plain values pass through.
pub fn unwrap_fixture_param(
    py: Python<'_>,
    value: &Bound<'_, PyAny>,
) -> PyResult<(Py<PyAny>, Option<String>, Vec<MarkData>)> {
    let param_spec_cls = py.import("pytest")?.getattr("ParamSpec")?;
    if !value.is_instance(&param_spec_cls)? {
        return Ok((value.clone().unbind(), None, Vec::new()));
    }
    let values: Vec<Bound<'_, PyAny>> = value
        .getattr("values")?
        .try_iter()?
        .collect::<PyResult<_>>()?;
    let inner = match values.as_slice() {
        [single] => single.clone().unbind(),
        many => pyo3::types::PyTuple::new(py, many)?.into_any().unbind(),
    };
    // HIDDEN_PARAM (or any non-string id) falls back to value derivation.
    let id = value
        .getattr("id")?
        .extract::<Option<String>>()
        .unwrap_or(None);
    let mut marks = Vec::new();
    for mark in value.getattr("marks")?.try_iter()? {
        let mark = mark?;
        // pytest.param normalizes decorators to Marks; stay defensive.
        let mark = match mark.getattr("mark") {
            Ok(inner) if inner.hasattr("name")? => inner,
            _ => mark,
        };
        if let Ok(name) = mark.getattr("name").and_then(|n| n.extract::<String>()) {
            marks.push(MarkData {
                name,
                obj: mark.unbind(),
            });
        }
    }
    Ok((inner, id, marks))
}

/// Expand items over parametrized fixtures in their closure: an item using
/// a fixture with `params=[...]` becomes one item per param value, with the
/// param id appended to the nodeid.
pub fn expand_fixture_params(
    py: Python<'_>,
    items: Vec<TestItem>,
    registry: &FixtureRegistry,
) -> PyResult<Vec<TestItem>> {
    // The function's nodeid without the parametrize suffix: consecutive
    // items sharing it are the direct-parametrize variants of one function.
    fn base(nodeid: &str) -> &str {
        nodeid.split('[').next().unwrap_or(nodeid)
    }

    let mut expanded = Vec::new();
    let mut iter = items.into_iter().peekable();
    while let Some(first) = iter.next() {
        // Group one function's direct-parametrize variants: upstream
        // parametrizes fixtures per-function (pytest_generate_tests), so
        // the fixture axis is shared by — and varies slower than — the
        // direct axis.
        let mut group = vec![first];
        while let Some(next) = iter.peek() {
            if base(&next.nodeid) == base(&group[0].nodeid) {
                group.push(iter.next().expect("peeked"));
            } else {
                break;
            }
        }
        let item = &group[0];
        // @pytest.mark.usefixtures names parametrize the item exactly like
        // signature fixtures do; pytest's closure puts them first
        // (initialnames = usefixtures + argnames), so their params expand
        // as the outer axis and lead the test ID.
        let mut requested = Vec::new();
        for mark in item.marks.iter().filter(|m| m.name == "usefixtures") {
            if let Ok(args) = mark.obj.bind(py).getattr("args")
                && let Ok(names) = args.extract::<Vec<String>>()
            {
                for name in names {
                    if !requested.contains(&name) {
                        requested.push(name);
                    }
                }
            }
        }
        for name in &item.fixture_names {
            if !requested.contains(name) {
                requested.push(name.clone());
            }
        }
        let parametrized: Vec<_> = registry
            .closure_for(&item.nodeid, &requested)
            .into_iter()
            .filter(|def| def.params.is_some())
            // indirect parametrize already assigned this fixture's param,
            // overriding the fixture's own params (pytest semantics).
            .filter(|def| {
                !item
                    .fixture_params
                    .iter()
                    .any(|(name, _, _)| name == &def.name)
            })
            // Direct parametrize of a closure fixture name replaces the
            // fixture outright (PseudoFixtureDef bypass), so its own params
            // must not add an expansion axis.
            .filter(|def| !item.callspec.iter().any(|(name, _)| name == &def.name))
            .collect();
        if parametrized.is_empty() {
            expanded.extend(group);
            continue;
        }
        // unittest.TestCase methods do not support fixture parametrization
        // (upstream TestCaseFunction is nofuncargs): the item stays
        // unexpanded and errors at setup with upstream's message.
        if item.func.bind(py).hasattr("make_case").unwrap_or(false) {
            let msg = format!(
                "{} does not support fixtures, maybe unittest.TestCase subclass?\n\
                 Node id: {}\n\
                 Function type: TestCaseFunction",
                item.func_name, item.nodeid
            );
            for item in &group {
                item.func
                    .bind(py)
                    .setattr("_pytest_unsupported_fixtures", &msg)?;
            }
            expanded.extend(group);
            continue;
        }

        // Cartesian product over each parametrized fixture's values.
        type Assignment = (String, usize, Py<PyAny>);
        type Variant = (String, Vec<Assignment>, Vec<MarkData>);
        let mut variants: Vec<Variant> = vec![(String::new(), Vec::new(), Vec::new())];
        for def in &parametrized {
            let values: Vec<Bound<'_, PyAny>> = def
                .params
                .as_ref()
                .expect("filtered to Some above")
                .bind(py)
                .try_iter()?
                .collect::<PyResult<_>>()?;
            let mut next = Vec::new();
            for (id, assignments, variant_marks) in &variants {
                for (index, wrapped) in values.iter().enumerate() {
                    // pytest.param(...) entries carry the value, an explicit
                    // id, and marks applied to the expanded item.
                    let (value, spec_id, extra_marks) = unwrap_fixture_param(py, wrapped)?;
                    let value_bound = value.bind(py);
                    let part = spec_id
                        .or_else(|| {
                            fixture_param_id(py, def.ids.as_ref(), value_bound, index)
                                .and_then(|id_obj| id_obj.bind(py).str().ok())
                                .map(|s| s.to_string())
                        })
                        .unwrap_or_else(|| id_for_value(value_bound, &def.name, index));
                    let id = if id.is_empty() {
                        part
                    } else {
                        format!("{id}-{part}")
                    };
                    let mut assignments = assignments
                        .iter()
                        .map(|(n, i, v)| (n.clone(), *i, v.clone_ref(py)))
                        .collect::<Vec<_>>();
                    assignments.push((def.name.clone(), index, value.clone_ref(py)));
                    let mut variant_marks: Vec<MarkData> = variant_marks
                        .iter()
                        .map(|m| MarkData {
                            name: m.name.clone(),
                            obj: m.obj.clone_ref(py),
                        })
                        .collect();
                    variant_marks.extend(extra_marks);
                    next.push((id, assignments, variant_marks));
                }
            }
            variants = next;
        }

        // Fixture params are the outer axis: their id parts lead and they
        // vary slower than the direct-parametrize axis (upstream parametrizes
        // fixtures before the function's own parametrize marks).
        for (id, assignments, variant_marks) in variants {
            // Fixture params are the outer (slower-varying) reorder axis, so
            // their scope keys lead the item's own (metafunc) keys. This lets
            // the item reorder group tests sharing a high-scoped fixture param
            // value, matching pytest's reorder_items.
            let fixture_keys: Vec<(String, Scope, usize)> = assignments
                .iter()
                .filter_map(|(name, index, _)| {
                    parametrized
                        .iter()
                        .find(|def| &def.name == name)
                        .filter(|def| def.scope > Scope::Function)
                        .map(|def| (name.clone(), def.scope, *index))
                })
                .collect();
            for item in &group {
                let scope_sort_keys: Vec<(String, Scope, usize)> = fixture_keys
                    .iter()
                    .cloned()
                    .chain(item.scope_sort_keys.iter().cloned())
                    .collect();
                let max_param_scope = scope_sort_keys
                    .iter()
                    .map(|(_, s, _)| *s)
                    .chain(std::iter::once(item.max_param_scope))
                    .max()
                    .unwrap_or(Scope::Function);
                let nodeid = match item.nodeid.find('[') {
                    Some(pos) => {
                        format!("{}[{id}-{}", &item.nodeid[..pos], &item.nodeid[pos + 1..])
                    }
                    None => format!("{}[{id}]", item.nodeid),
                };
                expanded.push(TestItem {
                    nodeid,
                    path: item.path.clone(),
                    module_name: item.module_name.clone(),
                    func_name: item.func_name.clone(),
                    func: item.func.clone_ref(py),
                    cls: item.cls.as_ref().map(|c| c.clone_ref(py)),
                    is_coroutine: item.is_coroutine,
                    is_doctest: item.is_doctest,
                    lineno: item.lineno,
                    fixture_names: item.fixture_names.clone(),
                    extra_fixture_names: item.extra_fixture_names.clone(),
                    marks: item
                        .marks
                        .iter()
                        .map(|m| MarkData {
                            name: m.name.clone(),
                            obj: m.obj.clone_ref(py),
                        })
                        .chain(variant_marks.iter().map(|m| MarkData {
                            name: m.name.clone(),
                            obj: m.obj.clone_ref(py),
                        }))
                        .collect(),
                    callspec: item
                        .callspec
                        .iter()
                        .map(|(n, v)| (n.clone(), v.clone_ref(py)))
                        .collect(),
                    fixture_params: assignments
                        .iter()
                        .map(|(n, i, v)| (n.clone(), *i, v.clone_ref(py)))
                        .collect(),
                    collector_class: item.collector_class.clone(),
                    func_class: item.func_class.clone(),
                    max_param_scope,
                    scope_sort_keys: scope_sort_keys.clone(),
                });
            }
        }
    }
    Ok(expanded)
}
