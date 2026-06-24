#[allow(unused_imports)]
use super::super::*;
use crate::collect::{MarkData, TestItem};
use crate::fixture::FixtureRegistry;
use pyo3::types::PyModule;
use std::path::Path;

use super::hooks::fire_pycollect_makeitem;
use super::introspect::{NameFilters, first_lineno, param_names_with_positional_only};
use super::parametrize::push_test_items;
use super::utils::{collect_error, read_marks};

#[allow(clippy::too_many_arguments)]
pub(crate) fn introspect_namespace(
    py: Python<'_>,
    module: &Bound<'_, PyModule>,
    nodeid_base: &str,
    module_name: &str,
    path: &Path,
    items: &mut Vec<TestItem>,
    registry: &mut FixtureRegistry,
    extra_generate_hooks: &[Py<PyAny>],
    makeitem_hook: bool,
    filters: &NameFilters,
) -> PyResult<()> {
    register_fixtures_from(py, module, &format!("{nodeid_base}::"), registry)?;

    // Module __test__ = False: skip test collection entirely (nose compat).
    if module
        .getattr("__test__")
        .ok()
        .is_some_and(|a| !a.extract::<bool>().unwrap_or(true))
    {
        return Ok(());
    }

    // Module-level `pytestmark` applies to every item in the module.
    let module_marks = read_marks(py, module.as_any())?;

    // pytest_generate_tests impls parametrize via metafunc: the module-level
    // one plus every plugin/conftest impl (pytest-repeat registers one). They
    // run in order on a single combined callable.
    let gen_list = pyo3::types::PyList::empty(py);
    if let Some(mod_hook) = module
        .dict()
        .get_item("pytest_generate_tests")?
        .filter(|hook| hook.is_callable())
    {
        gen_list.append(mod_hook)?;
    }
    for hook in extra_generate_hooks {
        gen_list.append(hook.bind(py))?;
    }
    let generate_hook: Option<Bound<'_, PyAny>> = if gen_list.is_empty() {
        None
    } else {
        Some(
            py.import("pytest._metafunc")?
                .getattr("combine_generate_hooks")?
                .call1((gen_list,))?,
        )
    };

    let inspect = py.import("inspect")?;
    let isclass = inspect.getattr("isclass")?;
    let dict = module.dict();
    // Module dicts preserve definition order in CPython; keep it.
    for (key, value) in dict.iter() {
        let Ok(name) = key.extract::<String>() else {
            continue;
        };
        // Builtin attributes (dunders etc.) are ignored before any matching, so
        // python_functions=* / python_classes=* don't collect or warn on them.
        if filters.is_ignored(&name) {
            continue;
        }
        // pytest_pycollect_makeitem: a conftest may claim a namespace member
        // (even a non-`test`-named one) by returning a custom node, e.g.
        // `MyFunction.from_parent(name=name, parent=collector)`. Honor it so the
        // tree renders `<MyFunction some>`; otherwise fall through to the
        // default Rust collection path.
        if makeitem_hook
            && let Some(custom) = fire_pycollect_makeitem(
                py,
                nodeid_base,
                path,
                &name,
                &value,
                filters.matches_function(&name),
            )
        {
            for (class_name, node_name, py_node) in custom {
                // Resolve fixture names from the original callobj so the
                // runner can fill fixtures even when a custom node class was
                // returned by the hook (the custom node may have an empty
                // fixturenames list at this point).
                let (fixture_names, _) =
                    param_names_with_positional_only(py, &value).unwrap_or_default();
                items.push(TestItem {
                    nodeid: format!("{nodeid_base}::{node_name}"),
                    path: path.to_path_buf(),
                    module_name: module_name.to_string(),
                    func_name: node_name,
                    func: value.clone().unbind(),
                    cls: None,
                    is_coroutine: false,
                    is_doctest: false,
                    fixture_names,
                    extra_fixture_names: Vec::new(),
                    marks: Vec::new(),
                    callspec: Vec::new(),
                    fixture_params: Vec::new(),
                    lineno: 0,
                    collector_class: String::new(),
                    func_class: class_name,
                    py_node,
                    max_param_scope: crate::fixture::Scope::Function,
                    scope_sort_keys: Vec::new(),
                });
            }
            continue;
        }
        // Wrap isclass in try-catch: objects with __class__ = property(raises)
        // cause inspect.isclass → isinstance(obj, type) to raise (#4266).
        let is_class = isclass
            .call1((&value,))
            .and_then(|r| r.extract::<bool>())
            .unwrap_or(false);
        if is_class {
            let is_testcase: bool = py
                .import("pytest._unittest")?
                .getattr("is_testcase_class")?
                .call1((&value,))?
                .extract()?;
            if is_testcase {
                // Class __test__ = False: skip (nose compat).
                let test_attr = value.getattr("__test__").ok();
                let test_false = test_attr
                    .as_ref()
                    .and_then(|a| a.extract::<bool>().ok())
                    .is_some_and(|v| !v);
                // Abstract TestCase classes are not collected (#12275).
                let is_abstract: bool =
                    inspect.getattr("isabstract")?.call1((&value,))?.extract()?;
                if !is_abstract && !test_false {
                    collect_testcase(
                        py,
                        &value,
                        &name,
                        nodeid_base,
                        module_name,
                        path,
                        &module_marks,
                        items,
                        registry,
                    )?;
                }
            } else if filters.matches_class(&name) {
                // Class __test__ = False: skip (nose compat).
                let test_attr = value.getattr("__test__").ok();
                let test_false = test_attr
                    .as_ref()
                    .and_then(|a| a.extract::<bool>().ok())
                    .is_some_and(|v| !v);
                let is_abstract: bool =
                    inspect.getattr("isabstract")?.call1((&value,))?.extract()?;
                if !is_abstract && !test_false {
                    collect_class(
                        py,
                        &value,
                        &name,
                        nodeid_base,
                        module_name,
                        path,
                        &module_marks,
                        items,
                        registry,
                        module,
                        generate_hook.as_ref(),
                        filters,
                    )?;
                }
            }
            continue;
        }
        // Test functions match the python_functions ini patterns (default
        // prefix "test"); fixtures are never test functions.
        if !filters.matches_function(&name)
            || !value.is_callable()
            || value.hasattr("_pytestfixturefunction").unwrap_or(false)
        {
            continue;
        }
        // Function __test__ = False: skip (nose compat).
        if value
            .getattr("__test__")
            .ok()
            .is_some_and(|a| !a.extract::<bool>().unwrap_or(true))
        {
            continue;
        }
        // A test-named member that is callable but not a function (an instance
        // with __call__) cannot be collected: pytest warns and skips it.
        let skip_nonfunc: bool = py
            .import("pytest._pycollect")?
            .getattr("warn_uncollectable_function")?
            .call1((&name, &value, path.to_string_lossy().as_ref()))?
            .extract()?;
        if skip_nonfunc {
            continue;
        }
        // Generator test functions fail collection (#12960).
        if inspect
            .getattr("isgeneratorfunction")?
            .call1((&value,))?
            .extract::<bool>()
            .unwrap_or(false)
        {
            return Err(collect_error(
                py,
                &format!("'yield' keyword is allowed in fixtures, but not in tests ({name})"),
            ));
        }
        let mut marks = read_marks(py, &value)?;
        marks.extend(clone_marks(py, &module_marks));
        push_test_items(
            py,
            items,
            nodeid_base,
            module_name,
            path,
            &name,
            &value,
            None,
            false,
            marks,
            module,
            generate_hook.as_ref(),
            registry,
        )?;
    }
    Ok(())
}

pub(crate) fn clone_marks(py: Python<'_>, marks: &[MarkData]) -> Vec<MarkData> {
    marks
        .iter()
        .map(|m| MarkData {
            name: m.name.clone(),
            obj: m.obj.clone_ref(py),
        })
        .collect()
}

/// Collect unittest.TestCase test methods as zero-arg runner callables
/// (setUp/method/tearDown handled by the pytest._unittest shim).
#[allow(clippy::too_many_arguments)]
pub(crate) fn collect_testcase(
    py: Python<'_>,
    cls: &Bound<'_, PyAny>,
    cls_name: &str,
    nodeid_base: &str,
    module_name: &str,
    path: &Path,
    module_marks: &[MarkData],
    items: &mut Vec<TestItem>,
    registry: &mut FixtureRegistry,
) -> PyResult<()> {
    let unittest_shim = py.import("pytest._unittest")?;
    let make_runner = unittest_shim.getattr("make_runner")?;
    let class_nodeid = format!("{nodeid_base}::{cls_name}");
    let mut class_marks = read_marks(py, cls)?;
    class_marks.extend(clone_marks(py, module_marks));

    // @pytest.fixture methods on the TestCase (upstream supports them,
    // typically autouse=True stashing values on self); they bind to the
    // same instance the test runs on via the runner's make_case().
    for pair in cls.getattr("__dict__")?.call_method0("items")?.try_iter()? {
        let (name, value): (String, Bound<'_, PyAny>) = pair?.extract()?;
        if value.is_callable() && value.hasattr("_pytestfixturefunction")? {
            register_fixture_def(
                py,
                &name,
                &value,
                &format!("{class_nodeid}::"),
                true,
                registry,
            )?;
        }
    }

    // Upstream's injected autouse fixtures: setUpClass/tearDownClass
    // (+doClassCleanups), pytest-style setup_class/teardown_class and
    // setup_method/teardown_method. Skipped classes don't register them
    // (upstream gates on _is_skipped(cls)).
    let class_skipped: bool = cls
        .getattr("__unittest_skip__")
        .and_then(|v| v.extract())
        .unwrap_or(false);
    if !class_skipped {
        for (factory, needs_instance) in [
            ("make_setup_method_fixture", true),
            ("make_class_fixture", false),
            ("make_setup_class_fixture", false),
        ] {
            let fixture = unittest_shim.getattr(factory)?.call1((cls,))?;
            if !fixture.is_none() {
                register_fixture_def(
                    py,
                    "",
                    &fixture,
                    &format!("{class_nodeid}::"),
                    needs_instance,
                    registry,
                )?;
            }
        }
    }

    // dir() includes inherited test methods, matching unittest collection.
    let mut names: Vec<String> = py
        .import("builtins")?
        .getattr("dir")?
        .call1((cls,))?
        .extract()?;
    names.sort();
    names.retain(|name| {
        name.starts_with("test")
            && cls
                .getattr(name.as_str())
                .map(|method| {
                    // Methods opting out via __test__ = False (issue1558).
                    method.is_callable()
                        && method
                            .getattr("__test__")
                            .and_then(|v| v.extract::<bool>())
                            .unwrap_or(true)
                })
                .unwrap_or(false)
    });
    // No test methods: unittest's runTest fallback collects as a single
    // item (upstream skips twisted.trial's own runTest; twisted-less here).
    if names.is_empty()
        && cls
            .getattr("runTest")
            .map(|method| method.is_callable())
            .unwrap_or(false)
    {
        names.push("runTest".to_string());
    }
    for name in names {
        let Ok(method) = cls.getattr(name.as_str()) else {
            continue;
        };
        if !method.is_callable() {
            continue;
        }
        let mut marks = read_marks(py, &method)?;
        marks.extend(clone_marks(py, &class_marks));
        let runner = make_runner.call1((cls, name.as_str()))?;
        items.push(TestItem {
            nodeid: format!("{class_nodeid}::{name}"),
            path: path.to_path_buf(),
            module_name: module_name.to_string(),
            func_name: name,
            func: runner.unbind(),
            // cls stays None: the runner drives the unittest instance via
            // make_case (item.cls Some would make the engine instantiate and
            // rebind, bypassing setUp/tearDown). The class is still exposed
            // for node.cls introspection via the runner's `cls` attribute.
            cls: None,
            is_coroutine: false,
            is_doctest: false,
            fixture_names: Vec::new(),
            extra_fixture_names: Vec::new(),
            marks,
            callspec: Vec::new(),
            fixture_params: Vec::new(),
            lineno: first_lineno(py, &method),
            collector_class: String::new(),
            func_class: String::new(),
            py_node: None,
            max_param_scope: crate::fixture::Scope::Function,
            scope_sort_keys: Vec::new(),
        });
    }
    Ok(())
}

/// Collect test methods (and class-level fixtures) from a Test* class.
#[allow(clippy::too_many_arguments)]
pub(crate) fn collect_class(
    py: Python<'_>,
    cls: &Bound<'_, PyAny>,
    cls_name: &str,
    nodeid_base: &str,
    module_name: &str,
    path: &Path,
    module_marks: &[MarkData],
    items: &mut Vec<TestItem>,
    registry: &mut FixtureRegistry,
    module: &Bound<'_, PyModule>,
    generate_hook: Option<&Bound<'_, PyAny>>,
    filters: &NameFilters,
) -> PyResult<()> {
    // Test classes with a custom __init__/__new__ can't be instantiated for
    // collection: pytest warns and skips them (handled in the Python shim so
    // the PytestCollectionWarning is captured for the warnings summary).
    let skip_class: bool = py
        .import("pytest._pycollect")?
        .getattr("warn_uncollectable_class")?
        .call1((cls, nodeid_base))?
        .extract()?;
    if skip_class {
        return Ok(());
    }
    let class_nodeid = format!("{nodeid_base}::{cls_name}");
    let mut class_marks = read_marks(py, cls)?;
    class_marks.extend(clone_marks(py, module_marks));

    // Class-level pytest_generate_tests: upstream instantiates the class and
    // calls `cls().pytest_generate_tests` as an extra hook alongside the
    // module-level and plugin hooks.
    let class_generate_hook: Option<Bound<'_, PyAny>> = if cls
        .getattr("pytest_generate_tests")
        .ok()
        .filter(|a| a.is_callable())
        .is_some()
    {
        let gen_list = pyo3::types::PyList::empty(py);
        if let Some(hook) = generate_hook {
            // The module-level combined hook already wraps module + plugin impls.
            gen_list.append(hook)?;
        }
        // Instantiate the class to get a bound method (upstream: cls().pytest_generate_tests).
        let instance = cls.call0()?;
        let cls_hook = instance.getattr("pytest_generate_tests")?;
        gen_list.append(&cls_hook)?;
        Some(
            py.import("pytest._metafunc")?
                .getattr("combine_generate_hooks")?
                .call1((gen_list,))?,
        )
    } else {
        None
    };
    let effective_generate_hook: Option<&Bound<'_, PyAny>> =
        class_generate_hook.as_ref().or(generate_hook);

    let builtins = py.import("builtins")?;
    let staticmethod_type = builtins.getattr("staticmethod")?;
    let classmethod_type = builtins.getattr("classmethod")?;

    // Definition order, matching pytest's PyCollector.collect: walk the MRO
    // (most-derived first), gather each class's own __dict__ in definition
    // order (deduped by name across the MRO), then concatenate in reverse-MRO
    // order so inherited methods precede the subclass's own. This is stable
    // even when a method is aliased from a base (e.g. `test_bar = Base.test_bar`),
    // where the function's own lineno would otherwise flip the order.
    let mro: Vec<Bound<'_, PyAny>> = cls.getattr("__mro__")?.extract()?;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut per_class: Vec<Vec<(String, Bound<'_, PyAny>, bool)>> = Vec::new();
    for base in &mro {
        let base_dict = base.getattr("__dict__")?;
        let mut values: Vec<(String, Bound<'_, PyAny>, bool)> = Vec::new();
        for pair in base_dict.call_method0("items")?.try_iter()? {
            let (key, raw): (Bound<'_, PyAny>, Bound<'_, PyAny>) = pair?.extract()?;
            let Ok(name) = key.extract::<String>() else {
                continue;
            };
            // Builtin attributes (dunders) are ignored before matching, so
            // python_functions=* doesn't collect every inherited method.
            if filters.is_ignored(&name) || seen.contains(&name) {
                continue;
            }
            seen.insert(name.clone());

            let is_static = raw.is_instance(&staticmethod_type)?;
            let is_classmethod = raw.is_instance(&classmethod_type)?;
            let value = if is_static || is_classmethod {
                raw.getattr("__func__")?
            } else {
                raw
            };
            if !value.is_callable() {
                continue;
            }
            values.push((name, value, is_static));
        }
        per_class.push(values);
    }

    for (name, value, is_static) in per_class.into_iter().rev().flatten() {
        if value.hasattr("_pytestfixturefunction")? {
            register_fixture_def(
                py,
                &name,
                &value,
                &format!("{class_nodeid}::"),
                !is_static,
                registry,
            )?;
            continue;
        }
        if !filters.matches_function(&name) {
            continue;
        }
        // Method __test__ = False: skip (nose compat).
        if value
            .getattr("__test__")
            .ok()
            .is_some_and(|a| !a.extract::<bool>().unwrap_or(true))
        {
            continue;
        }
        let mut marks = read_marks(py, &value)?;
        for mark in &class_marks {
            marks.push(MarkData {
                name: mark.name.clone(),
                obj: mark.obj.clone_ref(py),
            });
        }
        push_test_items(
            py,
            items,
            &class_nodeid,
            module_name,
            path,
            &name,
            &value,
            Some(cls),
            is_static,
            marks,
            module,
            effective_generate_hook,
            registry,
        )?;
    }
    // Fire pytest_pycollect_makeitem hooks so plugins can set extra_keyword_matches.
    // Store the result on the class object for keyword_match_names to read.
    let _ = (|| -> PyResult<()> {
        let kw_set = py
            .import("pytest._node")?
            .call_method1("fire_makeitem_for_class", (cls_name,))?;
        let mut extra: Vec<String> = Vec::new();
        for item in kw_set.try_iter()? {
            if let Ok(s) = item?.extract::<String>() {
                extra.push(s);
            }
        }
        if !extra.is_empty() {
            cls.setattr("_pytest_extra_keyword_matches", extra)?;
        }
        Ok(())
    })();
    Ok(())
}
