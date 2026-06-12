//! Python-side collection: modules, classes, TestCases, doctests, parametrize.

#[allow(unused_imports)]
use super::*;
use crate::collect::{MarkData, TestItem, file_nodeid, module_name_for};
use crate::fixture::FixtureRegistry;
use pyo3::types::{PyList, PyModule};
use std::path::Path;

/// The 1-based first line of a callable's definition (0 if unknown).
pub(crate) fn first_lineno(py: Python<'_>, func: &Bound<'_, PyAny>) -> u32 {
    let _ = py;
    func.getattr("__code__")
        .and_then(|code| code.getattr("co_firstlineno"))
        .and_then(|line| line.extract::<u32>())
        .unwrap_or(0)
}

/// Names of the fixture-requesting parameters of a Python callable, in
/// order: positional/keyword params without defaults (defaulted params and
/// *args/**kwargs are not fixture requests, matching pytest).
pub fn param_names(py: Python<'_>, func: &Bound<'_, PyAny>) -> PyResult<Vec<String>> {
    let inspect = py.import("inspect")?;
    let signature = inspect.getattr("signature")?.call1((func,))?;
    let parameters = signature.getattr("parameters")?;
    let empty = inspect.getattr("Parameter")?.getattr("empty")?;
    let mut names = Vec::new();
    for value in parameters.call_method0("values")?.try_iter()? {
        let parameter = value?;
        let kind = parameter.getattr("kind")?;
        let kind_name: String = kind.getattr("name")?.extract()?;
        if kind_name != "POSITIONAL_OR_KEYWORD" && kind_name != "KEYWORD_ONLY" {
            continue;
        }
        if !parameter.getattr("default")?.is(&empty) {
            continue;
        }
        names.push(parameter.getattr("name")?.extract()?);
    }
    Ok(names)
}

/// pytest compat.num_mock_patch_args: how many leading parameters are
/// injected by stacked @unittest.mock.patch decorators (their `patchings`
/// entries with no attribute_name and new=DEFAULT). Those are mock-filled
/// positionally at call time, not fixture requests.
pub fn num_mock_patch_args(py: Python<'_>, func: &Bound<'_, PyAny>) -> usize {
    let Ok(patchings) = func.getattr("patchings") else {
        return 0;
    };
    let Ok(iter) = patchings.try_iter() else {
        return 0;
    };
    // Both the stdlib and the rolling-backport `mock` define the sentinel;
    // like pytest, only consult already-imported modules (sys.modules).
    let modules = py.import("sys").and_then(|sys| sys.getattr("modules")).ok();
    let sentinels: Vec<Bound<'_, PyAny>> = ["unittest.mock", "mock"]
        .iter()
        .filter_map(|name| {
            modules
                .as_ref()?
                .get_item(name)
                .ok()?
                .getattr("DEFAULT")
                .ok()
        })
        .collect();
    iter.flatten()
        .filter(|p| {
            let no_attribute_name = p
                .getattr("attribute_name")
                .map(|v| !v.is_truthy().unwrap_or(true))
                .unwrap_or(false);
            let new_is_default = p
                .getattr("new")
                .map(|new| sentinels.iter().any(|s| new.is(s)))
                .unwrap_or(false);
            no_attribute_name && new_is_default
        })
        .count()
}

pub struct AsyncFlags {
    pub is_coroutine: bool,
    pub is_generator: bool,
    pub is_async_gen: bool,
}

pub fn async_flags(py: Python<'_>, func: &Bound<'_, PyAny>) -> PyResult<AsyncFlags> {
    let inspect = py.import("inspect")?;
    Ok(AsyncFlags {
        is_coroutine: inspect
            .getattr("iscoroutinefunction")?
            .call1((func,))?
            .extract()?,
        is_generator: inspect
            .getattr("isgeneratorfunction")?
            .call1((func,))?
            .extract()?,
        is_async_gen: inspect
            .getattr("isasyncgenfunction")?
            .call1((func,))?
            .extract()?,
    })
}

/// Import one test module and introspect it: append discovered test items
/// and fixture definitions (objects carrying recorded shim metadata).
/// Custom collectors: fire `pytest_collect_file(file_path, parent)` for each
/// candidate file; a plugin (pytest-ruff/pytest-mypy) may return a
/// `pytest.File` whose `.collect()` yields `pytest.Item`s. Each item becomes a
/// TestItem whose `func` is the item object itself (run via item.runtest()).
/// True when a pytest_collect_file hook exists (module-level in py_hooks, or
/// on a pluginmanager-registered plugin like pytest-mypy's MypyCollectionPlugin).
pub fn has_collect_file_hook(py: Python<'_>, hooks: &[crate::session::PyHook]) -> bool {
    if hooks.iter().any(|h| h.name == "pytest_collect_file") {
        return true;
    }
    py.import("pytest._pluginmanager")
        .and_then(|m| m.getattr("pluginmanager"))
        .and_then(|pm| {
            let plugins = pm.getattr("_plugins")?;
            for plugin in plugins.try_iter()? {
                if plugin?.hasattr("pytest_collect_file")? {
                    return Ok(true);
                }
            }
            Ok(false)
        })
        .unwrap_or(false)
}

/// Collect items via pytest_collect_file hooks.
/// Returns `(file, skip_reason)` pairs for files that were skipped via pytest.skip().
pub fn collect_custom_files(
    py: Python<'_>,
    rootdir: &Path,
    files: &[PathBuf],
    _hooks: &[crate::session::PyHook],
    items: &mut Vec<TestItem>,
) -> PyResult<Vec<(PathBuf, String)>> {
    let mut skipped: Vec<(PathBuf, String)> = Vec::new();
    let Some(config) = crate::python::proxies::existing_py_config(py) else {
        return Ok(skipped);
    };
    let config = config.bind(py);
    // pytest_collect_file impls live on the shim pluginmanager (autoloaded
    // plugin modules + objects registered at configure, e.g. pytest-mypy);
    // the hook relay reaches them all.
    let collect_file = py
        .import("pytest._pluginmanager")?
        .getattr("pluginmanager")?
        .getattr("hook")?
        .getattr("pytest_collect_file")?;
    let pathlib = py.import("pathlib")?.getattr("Path")?;
    let node_mod = py.import("pytest._node")?;
    let collector_cls = node_mod.getattr("Collector")?;
    // A session stand-in with .config (plugins read parent.session.config).
    let session = node_mod.getattr("_NodeSession")?.call1((&config,))?;
    // Custom collectors (pytest-mypy) inspect session.items mid-collection to
    // decide what to yield; start from a clean slate and publish each yielded
    // item so later files see their siblings, matching real pytest's
    // incremental `self.items.extend(self.genitems(node))`.
    node_mod.call_method0("reset_collection_items")?;
    let publish_item = node_mod.getattr("publish_collection_item")?;
    for file in files {
        let file_path = pathlib.call1((file.to_string_lossy().as_ref(),))?;
        let parent = collector_cls.call(
            (),
            Some(&{
                let kw = pyo3::types::PyDict::new(py);
                kw.set_item("config", config)?;
                kw.set_item("session", &session)?;
                kw.set_item(
                    "path",
                    pathlib.call1((rootdir.to_string_lossy().as_ref(),))?,
                )?;
                kw.set_item("nodeid", "")?;
                kw.set_item("name", "")?;
                kw
            }),
        )?;
        // The relay returns a list of every plugin's result (collector|None).
        let kwargs = pyo3::types::PyDict::new(py);
        kwargs.set_item("file_path", &file_path)?;
        kwargs.set_item("parent", &parent)?;
        let results = match collect_file.call((), Some(&kwargs)) {
            Ok(r) => r,
            // pytest.skip() in pytest_collect_file means "skip this file".
            Err(ref err)
                if err
                    .get_type(py)
                    .name()
                    .map(|n| n == "Skipped")
                    .unwrap_or(false) =>
            {
                let reason = err
                    .value(py)
                    .getattr("msg")
                    .and_then(|m| m.extract::<String>())
                    .unwrap_or_else(|_| "Skipped".to_string());
                skipped.push((file.clone(), reason));
                continue;
            }
            Err(e) => return Err(e),
        };
        let results_list: Vec<Bound<'_, PyAny>> = if results.is_none() {
            Vec::new()
        } else {
            results.try_iter()?.collect::<PyResult<_>>()?
        };
        for collector in results_list {
            if collector.is_none() {
                continue;
            }
            let collector_class: String = collector
                .getattr("__class__")
                .and_then(|c| c.getattr("__name__"))
                .and_then(|n| n.extract())
                .unwrap_or_else(|_| "Module".to_string());
            // Update already-collected items for this file to use the custom
            // collector class (e.g. MyModule replacing the default Module).
            let pre_existing: std::collections::HashSet<String> = items
                .iter_mut()
                .filter_map(|it| {
                    if it.path == *file {
                        it.collector_class = collector_class.clone();
                        Some(it.nodeid.clone())
                    } else {
                        None
                    }
                })
                .collect();
            // Call collect() for new items not already found by standard collection.
            for item_obj in collector.call_method0("collect")?.try_iter()? {
                let item_obj = item_obj?;
                // Publish to session.items immediately so a later file's
                // collect() sees this item (pytest-mypy's one-per-session
                // MypyStatusItem check).
                publish_item.call1((&item_obj,))?;
                let nodeid: String = item_obj.getattr("nodeid")?.extract()?;
                if pre_existing.contains(&nodeid) {
                    continue;
                }
                let name: String = item_obj.getattr("name")?.extract()?;
                let mut marks = Vec::new();
                if let Ok(own) = item_obj.getattr("own_markers") {
                    for mark in own.try_iter()? {
                        let mark = mark?;
                        marks.push(MarkData {
                            name: mark.getattr("name")?.extract()?,
                            obj: mark.unbind(),
                        });
                    }
                }
                items.push(TestItem {
                    nodeid,
                    path: file.clone(),
                    module_name: String::new(),
                    func_name: name,
                    func: item_obj.unbind(),
                    cls: None,
                    is_coroutine: false,
                    is_doctest: false,
                    fixture_names: Vec::new(),
                    extra_fixture_names: Vec::new(),
                    marks,
                    callspec: Vec::new(),
                    fixture_params: Vec::new(),
                    lineno: 0,
                    collector_class: collector_class.clone(),
                });
            }
        }
    }
    Ok(skipped)
}

pub fn collect_module(
    py: Python<'_>,
    rootdir: &Path,
    path: &Path,
    items: &mut Vec<TestItem>,
    registry: &mut FixtureRegistry,
    hooks: &mut Vec<crate::session::PyHook>,
) -> PyResult<()> {
    let (basedir, module_name) = module_name_for(path);
    sys_path_prepend(py, &basedir)?;
    let module = py.import(module_name.as_str())?;
    let nodeid_base = file_nodeid(rootdir, path);

    register_pytest_plugins(py, &module, registry, hooks)?;
    // Plugin/conftest pytest_generate_tests impls (e.g. pytest-repeat) run on
    // the metafunc alongside any module-level one.
    let extra_generate_hooks: Vec<Py<PyAny>> = hooks
        .iter()
        .filter(|hook| hook.name == "pytest_generate_tests")
        .map(|hook| hook.func.clone_ref(py))
        .collect();
    introspect_namespace(
        py,
        &module,
        &nodeid_base,
        &module_name,
        path,
        items,
        registry,
        &extra_generate_hooks,
    )
}

/// Collect doctest items from an already-imported Python module.
/// Returns items appended to `items`; `py_config` is the PyConfig proxy.
pub fn collect_doctests_from_module(
    py: Python<'_>,
    rootdir: &Path,
    path: &Path,
    py_config: &Py<PyAny>,
    items: &mut Vec<TestItem>,
) -> PyResult<()> {
    let (basedir, module_name) = module_name_for(path);
    sys_path_prepend(py, &basedir)?;
    let nodeid_base = file_nodeid(rootdir, path);
    let doctest_mod = py.import("_pytest.doctest")?;
    let results = doctest_mod.getattr("collect_module_doctests")?.call1((
        module_name.as_str(),
        path.to_string_lossy().as_ref(),
        nodeid_base.as_str(),
        py_config.bind(py),
    ))?;
    for item in results.try_iter()? {
        let tuple = item?;
        let nodeid: String = tuple.get_item(0)?.extract()?;
        let func: Py<PyAny> = tuple.get_item(1)?.extract()?;
        let lineno: u32 = tuple.get_item(2)?.extract()?;
        // Derive func_name from the last "::" component of the nodeid.
        let func_name = nodeid.rsplit("::").next().unwrap_or(&nodeid).to_string();
        items.push(TestItem {
            nodeid,
            path: path.to_path_buf(),
            module_name: module_name.clone(),
            func_name,
            func,
            cls: None,
            is_coroutine: false,
            is_doctest: true,
            fixture_names: vec!["doctest_namespace".to_string(), "request".to_string()],
            extra_fixture_names: vec![],
            marks: vec![],
            callspec: vec![],
            fixture_params: vec![],
            lineno,
            collector_class: String::new(),
        });
    }
    Ok(())
}

/// Collect doctest items from a text file (e.g. `*.rst`).
pub fn collect_doctests_from_textfile(
    py: Python<'_>,
    rootdir: &Path,
    path: &Path,
    py_config: &Py<PyAny>,
    items: &mut Vec<TestItem>,
) -> PyResult<()> {
    let nodeid_base = file_nodeid(rootdir, path);
    let doctest_mod = py.import("_pytest.doctest")?;
    let results = doctest_mod.getattr("collect_textfile_doctests")?.call1((
        path.to_string_lossy().as_ref(),
        nodeid_base.as_str(),
        py_config.bind(py),
    ))?;
    for item in results.try_iter()? {
        let tuple = item?;
        let nodeid: String = tuple.get_item(0)?.extract()?;
        let func: Py<PyAny> = tuple.get_item(1)?.extract()?;
        let lineno: u32 = tuple.get_item(2)?.extract()?;
        let func_name = nodeid.rsplit("::").next().unwrap_or(&nodeid).to_string();
        items.push(TestItem {
            nodeid,
            path: path.to_path_buf(),
            module_name: "__doctest_textfile__".to_string(),
            func_name,
            func,
            cls: None,
            is_coroutine: false,
            is_doctest: true,
            fixture_names: vec!["doctest_namespace".to_string(), "request".to_string()],
            extra_fixture_names: vec![],
            marks: vec![],
            callspec: vec![],
            fixture_params: vec![],
            lineno,
            collector_class: String::new(),
        });
    }
    Ok(())
}

/// Check whether a file path matches --doctest-glob patterns.
pub fn is_doctest_textfile(py: Python<'_>, path: &Path, py_config: &Py<PyAny>) -> PyResult<bool> {
    let doctest_mod = py.import("_pytest.doctest")?;
    let result = doctest_mod
        .getattr("is_doctest_textfile")?
        .call1((path.to_string_lossy().as_ref(), py_config.bind(py)))?;
    result.extract()
}

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
) -> PyResult<()> {
    register_fixtures_from(py, module, &format!("{nodeid_base}::"), registry)?;

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
                // Abstract TestCase classes are not collected (#12275).
                let is_abstract: bool =
                    inspect.getattr("isabstract")?.call1((&value,))?.extract()?;
                if !is_abstract {
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
            } else if name.starts_with("Test") {
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
                )?;
            }
            continue;
        }
        // pytest default python_functions = "test*"
        if !name.starts_with("test")
            || !value.is_callable()
            || value.hasattr("_pytestfixturefunction").unwrap_or(false)
        {
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
            marks,
            module,
            generate_hook.as_ref(),
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
) -> PyResult<()> {
    // Classes with a custom __init__ are not collected (pytest behavior).
    let cls_dict = cls.getattr("__dict__")?;
    if cls_dict.contains("__init__")? {
        return Ok(());
    }
    let class_nodeid = format!("{nodeid_base}::{cls_name}");
    let mut class_marks = read_marks(py, cls)?;
    class_marks.extend(clone_marks(py, module_marks));

    let builtins = py.import("builtins")?;
    let staticmethod_type = builtins.getattr("staticmethod")?;
    let classmethod_type = builtins.getattr("classmethod")?;

    // Use dir(cls) instead of cls.__dict__ so inherited test methods are collected.
    // Walk the MRO to find the raw descriptor for each name (detects staticmethod/classmethod).
    let dir_list: Vec<String> = builtins.getattr("dir")?.call1((cls,))?.extract()?;
    let mro: Vec<Bound<'_, PyAny>> = cls.getattr("__mro__")?.extract()?;

    let mut method_entries: Vec<(u32, String, Bound<'_, PyAny>, bool)> = Vec::new();

    for name in &dir_list {
        let mut raw_opt: Option<Bound<'_, PyAny>> = None;
        for base in &mro {
            let base_dict = base.getattr("__dict__")?;
            if base_dict.contains(name.as_str())? {
                raw_opt = Some(base_dict.get_item(name.as_str())?);
                break;
            }
        }
        let Some(raw) = raw_opt else { continue };

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

        let lineno = first_lineno(py, &value);
        method_entries.push((lineno, name.clone(), value, is_static));
    }

    // Sort by source line for deterministic definition-order traversal.
    method_entries.sort_by_key(|(ln, name, ..)| (*ln, name.clone()));

    for (_, name, value, is_static) in method_entries {
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
        if !name.starts_with("test") {
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
            marks,
            module,
            generate_hook,
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

/// Push the (possibly parametrize-expanded) items for one test function.
#[allow(clippy::too_many_arguments)]
pub(crate) fn push_test_items(
    py: Python<'_>,
    items: &mut Vec<TestItem>,
    nodeid_prefix: &str,
    module_name: &str,
    path: &Path,
    name: &str,
    func: &Bound<'_, PyAny>,
    cls: Option<&Bound<'_, PyAny>>,
    marks: Vec<MarkData>,
    module: &Bound<'_, PyModule>,
    generate_hook: Option<&Bound<'_, PyAny>>,
) -> PyResult<()> {
    let flags = async_flags(py, func)?;
    let mut fixture_names = param_names(py, func)?;
    // `cls` covers classmethods, re-bound through the instance at run time.
    if cls.is_some()
        && matches!(
            fixture_names.first().map(String::as_str),
            Some("self") | Some("cls")
        )
    {
        fixture_names.remove(0);
    }
    // @unittest.mock.patch-injected leading params are not fixture requests.
    let mock_args = num_mock_patch_args(py, func).min(fixture_names.len());
    if mock_args > 0 {
        fixture_names.drain(..mock_args);
    }

    // pytest_generate_tests: metafunc.parametrize calls become parametrize
    // marks, merged after the decorator-applied ones.
    let mut marks = marks;
    let fixture_names = fixture_names;
    // Fixtures a generate hook appended (pytest-repeat's indirect step
    // fixture): set up so their request.param is consumed, but not injected
    // into the test signature.
    let mut extra_generated_fixtures: Vec<String> = Vec::new();
    if let Some(hook) = generate_hook {
        // metafunc.config (option.count etc.) and definition markers
        // (get_closest_marker) let plugin impls like pytest-repeat decide.
        let config = crate::python::proxies::existing_py_config(py).map(|c| c.into_bound(py));
        let mark_objs = pyo3::types::PyList::empty(py);
        for m in &marks {
            mark_objs.append(m.obj.bind(py))?;
        }
        let metafunc = py.import("pytest._metafunc")?.getattr("Metafunc")?.call1((
            func,
            fixture_names.clone(),
            module,
            cls.map(|c| c.clone().unbind()),
            config,
            mark_objs,
        ))?;
        hook.call1((&metafunc,))?;
        for mark in metafunc.getattr("_parametrize_marks")?.try_iter()? {
            let mark = mark?;
            marks.push(MarkData {
                name: "parametrize".to_string(),
                obj: mark.unbind(),
            });
        }
        // A hook may append fixturenames (pytest-repeat adds its indirect
        // step fixture so its request.param is set up per repeat).
        let updated: Vec<String> = metafunc.getattr("fixturenames")?.extract()?;
        for name in updated {
            if !fixture_names.contains(&name) && !extra_generated_fixtures.contains(&name) {
                extra_generated_fixtures.push(name);
            }
        }
    }

    let variants = expand_parametrize(py, &marks, &format!("{nodeid_prefix}::{name}"), Some(func))?;
    for variant in variants {
        let nodeid = match &variant.id {
            Some(id) => format!("{nodeid_prefix}::{name}[{id}]"),
            None => format!("{nodeid_prefix}::{name}"),
        };
        let mut item_marks: Vec<MarkData> = marks
            .iter()
            .map(|m| MarkData {
                name: m.name.clone(),
                obj: m.obj.clone_ref(py),
            })
            .collect();
        item_marks.extend(variant.extra_marks);
        items.push(TestItem {
            nodeid,
            path: path.to_path_buf(),
            module_name: module_name.to_string(),
            func_name: name.to_string(),
            func: func.clone().unbind(),
            cls: cls.map(|c| c.clone().unbind()),
            is_coroutine: flags.is_coroutine,
            is_doctest: false,
            fixture_names: fixture_names.clone(),
            extra_fixture_names: extra_generated_fixtures.clone(),
            marks: item_marks,
            callspec: variant.params,
            fixture_params: variant.indirect_params,
            lineno: first_lineno(py, func),
            collector_class: String::new(),
        });
    }
    Ok(())
}

pub(crate) struct ParamVariant {
    /// The "[...]" id suffix; None for unparametrized tests.
    id: Option<String>,
    params: Vec<(String, Py<PyAny>)>,
    /// indirect parametrize assignments: (fixture name, param index, value).
    indirect_params: Vec<(String, usize, Py<PyAny>)>,
    /// Marks attached via pytest.param(..., marks=...).
    extra_marks: Vec<MarkData>,
}

/// One parameter set (one `pytest.param`/value row) within a single
/// `@pytest.mark.parametrize` mark.
struct ParamSet {
    /// None hides the set from the test ID (pytest.HIDDEN_PARAM).
    id_part: Option<String>,
    params: Vec<(String, Py<PyAny>)>,
    /// indirect=True/[names]: the value parametrizes the same-named
    /// fixture (request.param) instead of being passed to the test.
    indirect_params: Vec<(String, usize, Py<PyAny>)>,
    extra_marks: Vec<MarkData>,
}

/// One `@pytest.mark.parametrize` mark's worth of parameter sets; stacked
/// marks become separate dimensions in the cartesian product.
struct Dim {
    sets: Vec<ParamSet>,
}

/// Expand stacked @pytest.mark.parametrize marks into the cartesian product
/// of parameter sets. Marks appear in pytestmark order (bottom decorator
/// first); ids join in that order and later marks vary fastest.
pub(crate) fn expand_parametrize(
    py: Python<'_>,
    marks: &[MarkData],
    nodeid: &str,
    func: Option<&Bound<'_, PyAny>>,
) -> PyResult<Vec<ParamVariant>> {
    let param_spec_cls = py.import("pytest")?.getattr("ParamSpec")?;
    let hidden_param = py.import("pytest")?.getattr("HIDDEN_PARAM")?;
    // Armed by configure_mark_generator once the session config is known.
    let strict_ids = py
        .import("pytest._marks")
        .and_then(|m| m.getattr("mark"))
        .and_then(|mark| mark.getattr("_strict_parametrization_ids"))
        .and_then(|v| v.extract::<bool>())
        .unwrap_or(false);
    let mut dims: Vec<Dim> = Vec::new();

    for mark in marks.iter().filter(|m| m.name == "parametrize") {
        let args = mark.obj.bind(py).getattr("args")?;
        let kwargs = mark.obj.bind(py).getattr("kwargs")?;
        // Both spellings are valid: positional or argnames=/argvalues= keywords.
        let argnames_obj = if args.len()? > 0 {
            args.get_item(0)?
        } else {
            kwargs.get_item("argnames")?
        };
        let argvalues = if args.len()? > 1 {
            args.get_item(1)?
        } else {
            kwargs.get_item("argvalues")?
        };
        // pytest's force_tuple: only a single argname given as a *string*
        // takes each argvalue as the bare value; a one-element list
        // (["x"]) still expects one-element value collections.
        let (argnames, force_scalar): (Vec<String>, bool) = match argnames_obj.extract::<String>() {
            Ok(joined) => {
                let names: Vec<String> = joined.split(',').map(|s| s.trim().to_string()).collect();
                let single = names.len() == 1;
                (names, single)
            }
            Err(_) => (argnames_obj.extract()?, false),
        };
        let ids_obj = mark.obj.bind(py).getattr("kwargs")?.get_item("ids").ok();
        let explicit_ids: Option<Vec<Option<String>>> =
            ids_obj.as_ref().and_then(|ids| ids.extract().ok());
        // ids=callable: idfn(val) per value, None falling through to the
        // default id for that value (upstream _idval_from_function).
        let ids_callable = ids_obj.filter(|ids| ids.is_callable());
        // indirect=True routes every argname's value to the same-named
        // fixture's request.param; indirect=["x"] only the listed ones.
        let indirect_obj = mark
            .obj
            .bind(py)
            .getattr("kwargs")?
            .get_item("indirect")
            .ok();
        let indirect_all = indirect_obj
            .as_ref()
            .and_then(|value| value.extract::<bool>().ok())
            .unwrap_or(false);
        let indirect_names: Vec<String> = indirect_obj
            .as_ref()
            .and_then(|value| value.extract::<Vec<String>>().ok())
            .unwrap_or_default();
        let is_indirect = |name: &str| indirect_all || indirect_names.iter().any(|n| n == name);

        let mut sets = Vec::new();
        for (index, value_set) in argvalues.try_iter()?.enumerate() {
            let value_set = value_set?;
            let (values, spec_id, hidden, extra_marks) =
                if value_set.is_instance(&param_spec_cls)? {
                    let values: Vec<Bound<'_, PyAny>> = value_set
                        .getattr("values")?
                        .try_iter()?
                        .collect::<PyResult<_>>()?;
                    let id_obj = value_set.getattr("id")?;
                    let (spec_id, hidden) = if id_obj.is(&hidden_param) {
                        (None, true)
                    } else {
                        (id_obj.extract::<Option<String>>()?, false)
                    };
                    let extra_marks = value_set
                        .getattr("marks")?
                        .try_iter()?
                        .map(|m| {
                            let m = m?;
                            Ok(MarkData {
                                name: m.getattr("name")?.extract()?,
                                obj: m.unbind(),
                            })
                        })
                        .collect::<PyResult<Vec<_>>>()?;
                    (values, spec_id, hidden, extra_marks)
                } else if force_scalar {
                    (vec![value_set], None, false, Vec::new())
                } else {
                    let values: Vec<Bound<'_, PyAny>> =
                        value_set.try_iter()?.collect::<PyResult<_>>()?;
                    (values, None, false, Vec::new())
                };

            if values.len() != argnames.len() {
                // Upstream ParameterSet._for_parametrize wording, raised as
                // a bare CollectError (message only, no traceback).
                let names_repr = format!(
                    "[{}]",
                    argnames
                        .iter()
                        .map(|n| format!("'{n}'"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                let values_repr: String = pyo3::types::PyTuple::new(py, values.iter())?
                    .repr()?
                    .extract()?;
                let message = format!(
                    "{nodeid}: in \"parametrize\" the number of names ({}):\n  {names_repr}\nmust be equal to the number of values ({}):\n  {values_repr}",
                    argnames.len(),
                    values.len()
                );
                return Err(collect_error(py, &message));
            }

            let id_part = if hidden {
                None
            } else {
                let from_callable = || -> PyResult<Option<String>> {
                    let Some(idfn) = ids_callable.as_ref() else {
                        return Ok(None);
                    };
                    let mut parts = Vec::new();
                    for (argname, value) in argnames.iter().zip(values.iter()) {
                        let id = idfn.call1((value,)).map_err(|err| {
                            collect_error(
                                py,
                                &format!(
                                    "{nodeid}: error raised while trying to determine id of \
                                     parameter '{argname}' at position {index}\n{}",
                                    err.value(py)
                                ),
                            )
                        })?;
                        parts.push(
                            user_id_from_value(py, &id)
                                .unwrap_or_else(|| id_for_value(value, argname, index)),
                        );
                    }
                    Ok(Some(parts.join("-")))
                };
                let callable_id = from_callable()?;
                Some(
                    spec_id
                        .or_else(|| {
                            explicit_ids
                                .as_ref()
                                .and_then(|ids| ids.get(index).cloned().flatten())
                        })
                        .or(callable_id)
                        .unwrap_or_else(|| {
                            let parts: Vec<String> = argnames
                                .iter()
                                .zip(values.iter())
                                .map(|(argname, value)| id_for_value(value, argname, index))
                                .collect();
                            parts.join("-")
                        }),
                )
            };
            let mut params: Vec<(String, Py<PyAny>)> = Vec::new();
            let mut indirect_params: Vec<(String, usize, Py<PyAny>)> = Vec::new();
            for (argname, value) in argnames.iter().cloned().zip(values) {
                if is_indirect(&argname) {
                    indirect_params.push((argname, index, value.unbind()));
                } else {
                    params.push((argname, value.unbind()));
                }
            }
            sets.push(ParamSet {
                id_part,
                params,
                indirect_params,
                extra_marks,
            });
        }
        dedup_param_ids(py, &mut sets, nodeid, &argnames, strict_ids)?;
        if sets.is_empty() {
            sets.push(notset_param_set(py, &argnames, func, indirect_all, &indirect_names)?);
        }
        dims.push(Dim { sets });
    }

    if dims.is_empty() {
        return Ok(vec![ParamVariant {
            id: None,
            params: Vec::new(),
            indirect_params: Vec::new(),
            extra_marks: Vec::new(),
        }]);
    }
    // An empty parameter set produces no items (pytest marks one skipped;
    // zero items is the closest simple behavior).
    if dims.iter().any(|dim| dim.sets.is_empty()) {
        return Ok(Vec::new());
    }

    Ok(cartesian_param_variants(py, &dims))
}

/// Cartesian product over the parametrize dimensions: the last dim varies
/// fastest and IDs join in dim order (matching stacked-decorator order).
fn cartesian_param_variants(py: Python<'_>, dims: &[Dim]) -> Vec<ParamVariant> {
    let mut variants = Vec::new();
    let mut indices = vec![0usize; dims.len()];
    'outer: loop {
        let mut id_parts = Vec::new();
        let mut params = Vec::new();
        let mut indirect_params = Vec::new();
        let mut extra_marks = Vec::new();
        for (dim, &index) in dims.iter().zip(indices.iter()) {
            let set = &dim.sets[index];
            // HIDDEN_PARAM sets contribute nothing to the test ID.
            if let Some(part) = &set.id_part {
                id_parts.push(part.clone());
            }
            for (name, value) in &set.params {
                params.push((name.clone(), value.clone_ref(py)));
            }
            for (name, param_index, value) in &set.indirect_params {
                indirect_params.push((name.clone(), *param_index, value.clone_ref(py)));
            }
            for mark in &set.extra_marks {
                extra_marks.push(MarkData {
                    name: mark.name.clone(),
                    obj: mark.obj.clone_ref(py),
                });
            }
        }
        variants.push(ParamVariant {
            // All-hidden variants keep the bare test name (no brackets).
            id: (!id_parts.is_empty()).then(|| id_parts.join("-")),
            params,
            indirect_params,
            extra_marks,
        });

        for pos in (0..dims.len()).rev() {
            indices[pos] += 1;
            if indices[pos] < dims[pos].sets.len() {
                continue 'outer;
            }
            indices[pos] = 0;
            if pos == 0 {
                break 'outer;
            }
        }
    }
    variants
}

/// Resolve duplicate parameter-set IDs within one parametrize mark:
/// under strict_parametrization_ids a duplicate is a CollectError;
/// otherwise pytest's make_unique_parameterset_ids counter suffix applies.
fn dedup_param_ids(
    py: Python<'_>,
    sets: &mut [ParamSet],
    nodeid: &str,
    argnames: &[String],
    strict_ids: bool,
) -> PyResult<()> {
    let mut counts: std::collections::HashMap<Option<String>, usize> =
        std::collections::HashMap::new();
    for set in sets.iter() {
        *counts.entry(set.id_part.clone()).or_default() += 1;
    }
    if counts.values().any(|&count| count > 1) {
        let display =
            |id: &Option<String>| id.clone().unwrap_or_else(|| "<hidden>".to_string());
        if strict_ids {
            let mut reprs = Vec::new();
            for set in sets.iter() {
                let values =
                    PyList::new(py, set.params.iter().map(|(_, value)| value.bind(py)))?;
                reprs.push(values.repr()?.to_string());
            }
            let mut seen = std::collections::HashSet::new();
            let duplicates: Vec<String> = sets
                .iter()
                .filter(|set| counts[&set.id_part] > 1)
                .filter(|set| seen.insert(set.id_part.clone()))
                .map(|set| display(&set.id_part))
                .collect();
            let ids: Vec<String> = sets.iter().map(|set| display(&set.id_part)).collect();
            let message = format!(
                "Duplicate parametrization IDs detected, but strict_parametrization_ids is set.\n\
                 \n\
                 Test name:      {nodeid}\n\
                 Parameters:     {}\n\
                 Parameter sets: {}\n\
                 IDs:            {}\n\
                 Duplicates:     {}\n\
                 \n\
                 You can fix this problem using `@pytest.mark.parametrize(..., ids=...)` or `pytest.param(..., id=...)`.",
                argnames.join(", "),
                reprs.join(", "),
                ids.join(", "),
                duplicates.join(", "),
            );
            return Err(collect_error(py, &message));
        }
        if counts.get(&None).copied().unwrap_or(0) > 1 {
            return Err(collect_error(
                py,
                &format!(
                    "In {nodeid}: multiple instances of HIDDEN_PARAM cannot be used in \
                     the same parametrize call, because the tests names need to be unique."
                ),
            ));
        }
        let mut existing: std::collections::HashSet<String> =
            sets.iter().filter_map(|set| set.id_part.clone()).collect();
        let mut suffixes: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for set in sets.iter_mut() {
            let Some(id) = set.id_part.clone() else {
                continue;
            };
            if counts[&Some(id.clone())] <= 1 {
                continue;
            }
            let sep = if id.chars().last().is_some_and(|c| c.is_ascii_digit()) {
                "_"
            } else {
                ""
            };
            let counter = suffixes.entry(id.clone()).or_insert(0);
            let mut new_id = format!("{id}{sep}{counter}");
            while existing.contains(&new_id) {
                *counter += 1;
                new_id = format!("{id}{sep}{counter}");
            }
            existing.insert(new_id.clone());
            set.id_part = Some(new_id);
            *counter += 1;
        }
    }
    Ok(())
}

/// The single NOTSET parameter set pytest collects for empty argvalues,
/// carrying the configured empty_parameter_set_mark (default: skip).
fn notset_param_set(
    py: Python<'_>,
    argnames: &[String],
    func: Option<&Bound<'_, PyAny>>,
    indirect_all: bool,
    indirect_names: &[String],
) -> PyResult<ParamSet> {
    let mark_decorator = py
        .import("_pytest.mark")?
        .getattr("get_empty_parameterset_mark")?
        .call1((
            existing_py_config(py).unwrap_or_else(|| py.None()),
            argnames.to_vec(),
            func.map(|f| f.clone().unbind()).unwrap_or_else(|| py.None()),
        ))?;
    let mark_obj = mark_decorator.getattr("mark")?;
    let mark_name: String = mark_obj.getattr("name")?.extract()?;
    let notset = py.import("_pytest.compat")?.getattr("NOTSET")?;
    let mut params: Vec<(String, Py<PyAny>)> = Vec::new();
    let mut indirect_params: Vec<(String, usize, Py<PyAny>)> = Vec::new();
    for argname in argnames.iter().cloned() {
        if indirect_all || indirect_names.iter().any(|n| n == &argname) {
            indirect_params.push((argname, 0usize, notset.clone().unbind()));
        } else {
            params.push((argname, notset.clone().unbind()));
        }
    }
    Ok(ParamSet {
        id_part: Some("NOTSET".to_string()),
        params,
        indirect_params,
        extra_marks: vec![MarkData {
            name: mark_name,
            obj: mark_obj.unbind(),
        }],
    })
}

/// A pytest.Collector.CollectError carrying `message`: collection fails
/// with the message shown bare (no traceback) in the ERRORS section.
pub(crate) fn collect_error(py: Python<'_>, message: &str) -> PyErr {
    let cls = py
        .import("pytest")
        .and_then(|m| m.getattr("Collector"))
        .and_then(|c| c.getattr("CollectError"));
    match cls {
        Ok(cls) => match cls.call1((message,)) {
            Ok(instance) => PyErr::from_value(instance),
            Err(err) => err,
        },
        Err(err) => err,
    }
}

/// Some(message) when `err` is a CollectError, shown without a traceback.
pub fn collect_error_message(py: Python<'_>, err: &PyErr) -> Option<String> {
    let cls = py
        .import("pytest")
        .and_then(|m| m.getattr("Collector"))
        .and_then(|c| c.getattr("CollectError"))
        .ok()?;
    err.matches(py, &cls)
        .unwrap_or(false)
        .then(|| err.value(py).to_string())
}

/// pytest-style id for one parameter value.
/// The id object for one fixture param when @pytest.fixture(ids=...) was
/// given: ids[index] for a list, ids(value) for a callable. None (absent
/// ids, None entry, or error) falls back to the value-derived id.
pub(crate) fn fixture_param_id(
    py: Python<'_>,
    ids: Option<&Py<PyAny>>,
    value: &Bound<'_, PyAny>,
    index: usize,
) -> Option<Py<PyAny>> {
    let ids = ids?.bind(py);
    let id_obj = if ids.is_callable() {
        ids.call1((value,)).ok()?
    } else {
        ids.get_item(index).ok()?
    };
    if id_obj.is_none() {
        return None;
    }
    Some(id_obj.unbind())
}

/// pytest's ascii_escaped for str ids ("\x00" -> "\\x00"); printable
/// ASCII passes through untouched.
/// Upstream's _idval_from_value applied to a user-supplied id (an
/// `ids=` callable or list entry): strings ascii-escape, numbers/bools
/// stringify, anything else falls through to the default id (None).
pub(crate) fn user_id_from_value(py: Python<'_>, id: &Bound<'_, PyAny>) -> Option<String> {
    let _ = py;
    if id.is_none() {
        return None;
    }
    if let Ok(text) = id.extract::<String>() {
        return Some(ascii_escaped_str(id, text));
    }
    if id.extract::<bool>().is_ok() || id.extract::<i64>().is_ok() || id.extract::<f64>().is_ok() {
        return id.str().ok().map(|s| s.to_string());
    }
    None
}

pub(crate) fn ascii_escaped_str(value: &Bound<'_, PyAny>, s: String) -> String {
    if s.chars().all(|c| matches!(c, ' '..='~')) {
        return s;
    }
    value
        .call_method1("encode", ("unicode_escape",))
        .and_then(|b| b.call_method1("decode", ("ascii",)))
        .and_then(|s| s.extract::<String>())
        .unwrap_or(s)
}

/// pytest's _idval: how one parametrize value renders in the test ID.
pub(crate) fn id_for_value(value: &Bound<'_, PyAny>, argname: &str, index: usize) -> String {
    if value.is_none() {
        return "None".to_string();
    }
    if let Ok(b) = value.cast::<pyo3::types::PyBool>() {
        return if b.is_true() { "True" } else { "False" }.to_string();
    }
    if let Ok(s) = value.extract::<String>() {
        return ascii_escaped_str(value, s);
    }
    // bytes: ascii_escaped = decode("ascii", "backslashreplace") with
    // non-printables escaped.
    if let Ok(bytes) = value.cast::<pyo3::types::PyBytes>() {
        return bytes
            .as_bytes()
            .iter()
            .map(|&b| {
                if matches!(b, 0x20..=0x7e) {
                    (b as char).to_string()
                } else {
                    format!("\\x{b:02x}")
                }
            })
            .collect();
    }
    // Numbers and enums all render via str() (upstream hits the number
    // branch first, so IntEnum is "30", plain Enum "Color.RED" — both str).
    let py = value.py();
    let is_enum = py
        .import("enum")
        .and_then(|m| m.getattr("Enum"))
        .and_then(|cls| value.is_instance(&cls))
        .unwrap_or(false);
    if (is_enum
        || value.cast::<pyo3::types::PyInt>().is_ok()
        || value.cast::<pyo3::types::PyFloat>().is_ok()
        || value.cast::<pyo3::types::PyComplex>().is_ok())
        && let Ok(s) = value.str()
    {
        return s.to_string();
    }
    // re.Pattern: the (escaped) pattern text.
    let is_pattern = py
        .import("re")
        .and_then(|m| m.getattr("Pattern"))
        .and_then(|cls| value.is_instance(&cls))
        .unwrap_or(false);
    if is_pattern
        && let Ok(pattern) = value.getattr("pattern")
        && let Ok(s) = pattern.extract::<String>()
    {
        return ascii_escaped_str(&pattern, s);
    }
    // Classes and functions render as their __name__.
    if let Ok(name) = value.getattr("__name__")
        && let Ok(s) = name.extract::<String>()
    {
        return s;
    }
    format!("{argname}{index}")
}

/// Read `pytestmark` from a function, class, or module. Accepts a single
/// mark or a list, and normalizes bare MarkDecorators (e.g.
/// `pytestmark = pytest.mark.asyncio`) to their Mark.
pub(crate) fn read_marks(py: Python<'_>, obj: &Bound<'_, PyAny>) -> PyResult<Vec<MarkData>> {
    // get_unpacked_marks (upstream): classes merge their whole MRO's own
    // pytestmark lists, base classes first; MarkDecorators unwrap to Marks.
    let mut marks = Vec::new();
    let get_unpacked = match py
        .import("pytest._marks")
        .and_then(|m| m.getattr("get_unpacked_marks"))
    {
        Ok(f) => f,
        Err(_) => return Ok(marks),
    };
    // Propagate TypeError (invalid pytestmark) so the caller can report it
    // as a collection error; swallow only import/getattr failures above.
    let entries = get_unpacked.call1((obj,))?;
    let Ok(iter) = entries.try_iter() else {
        return Ok(marks);
    };
    for mark in iter.flatten() {
        // Defensive: skip entries without a string name (stubs, mocks).
        let Ok(name) = mark.getattr("name").and_then(|n| n.extract::<String>()) else {
            continue;
        };
        marks.push(MarkData {
            name,
            obj: mark.unbind(),
        });
    }
    Ok(marks)
}

/// Expand `testpaths` ini globs against the rootdir (sorted per entry,
/// recursive ** supported), pytest's Config._decide_args.
pub fn glob_testpaths(py: Python<'_>, rootdir: &Path, entries: &[String]) -> PyResult<Vec<String>> {
    let glob = py.import("glob")?;
    let kwargs = pyo3::types::PyDict::new(py);
    kwargs.set_item("recursive", true)?;
    let mut out = Vec::new();
    for entry in entries {
        let pattern = rootdir.join(entry);
        let mut matches: Vec<String> = glob
            .call_method("glob", (pattern.to_string_lossy().as_ref(),), Some(&kwargs))?
            .extract()?;
        matches.sort();
        out.extend(matches);
    }
    Ok(out)
}

/// The -k matching name set for an item (upstream KeywordMatcher.from_item):
/// node-chain names (path components, class names, test name with params),
/// names assigned directly on the test function, and mark names.
pub fn keyword_match_names(py: Python<'_>, item: &TestItem) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let (file_part, rest) = item
        .nodeid
        .split_once("::")
        .unwrap_or((item.nodeid.as_str(), ""));
    // Subdirectory and module-file names (upstream includes every chain
    // node below the root directory).
    for component in std::path::Path::new(file_part).components() {
        names.push(component.as_os_str().to_string_lossy().to_string());
    }
    for part in rest.split("::") {
        if !part.is_empty() {
            names.push(part.to_string());
        }
    }
    // Names attached to the function through direct assignment.
    if let Ok(dict) = item.func.bind(py).getattr("__dict__")
        && let Ok(keys) = dict.call_method0("keys")
        && let Ok(iter) = keys.try_iter()
    {
        for key in iter.flatten() {
            if let Ok(name) = key.extract::<String>() {
                names.push(name);
            }
        }
    }
    for mark in &item.marks {
        names.push(mark.name.clone());
    }
    // extra_keyword_matches set by pytest_pycollect_makeitem hooks on the class.
    if let Some(cls) = &item.cls
        && let Ok(extras) = cls
            .bind(py)
            .getattr("_pytest_extra_keyword_matches")
            .and_then(|v| v.extract::<Vec<String>>())
    {
        names.extend(extras);
    }
    names
}
