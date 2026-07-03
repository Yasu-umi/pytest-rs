#[allow(unused_imports)]
use super::super::*;
use crate::collect::{TestItem, file_nodeid, module_name_for};
use crate::fixture::FixtureRegistry;
use std::path::Path;

use super::hooks::{
    call_pycollect_makemodule_hook, has_pycollect_makeitem_hook, has_pycollect_makemodule_hook,
};
use super::items::introspect_namespace;

/// Mirror pytest's import_path ImportPathMismatchError: after importing a test
/// module by dotted name, the cached module's `__file__` must point at the file
/// we are collecting. A mismatch means two test files share a basename, which
/// pytest reports as a collection error rather than silently re-collecting the
/// first one. Reference: _pytest/pathlib.py:import_path / python.py:importtestmodule.
fn check_import_path_mismatch(
    py: Python<'_>,
    module: &Bound<'_, PyAny>,
    module_name: &str,
    path: &Path,
) -> PyResult<()> {
    // __init__.py packages are exempt, as is PY_IGNORE_IMPORTMISMATCH=1.
    if path.file_name().and_then(|n| n.to_str()) == Some("__init__.py") {
        return Ok(());
    }
    if std::env::var("PY_IGNORE_IMPORTMISMATCH").as_deref() == Ok("1") {
        return Ok(());
    }
    let module_file: Option<String> = module
        .getattr("__file__")
        .ok()
        .and_then(|f| f.extract::<String>().ok());
    // Normalize like pytest: .pyc/.pyo -> source, and a package's __init__.py
    // collapses to its directory before comparison.
    let normalized = module_file.as_ref().map(|mf| {
        let mut mf = mf.clone();
        if mf.ends_with(".pyc") || mf.ends_with(".pyo") {
            mf.pop();
        }
        let init_suffix = format!("{}__init__.py", std::path::MAIN_SEPARATOR);
        if let Some(stripped) = mf.strip_suffix(&init_suffix) {
            mf = stripped.to_string();
        }
        mf
    });
    let is_same = match &normalized {
        // os.path.samefile(path, module_file); a missing file is not the same.
        Some(mf) => py
            .import("os")
            .and_then(|os| os.getattr("path"))
            .and_then(|p| p.call_method1("samefile", (path, mf.as_str())))
            .and_then(|r| r.extract::<bool>())
            .unwrap_or(false),
        None => false,
    };
    if is_same {
        return Ok(());
    }
    // ImportPathMismatchError carries the normalized __file__ in its args.
    let message = format!(
        "import file mismatch:\n\
         imported module '{module_name}' has this __file__ attribute:\n  \
         {}\n\
         which is not the same as the test file we want to collect:\n  \
         {}\n\
         HINT: remove __pycache__ / .pyc files and/or use a unique basename for your test file modules",
        normalized.unwrap_or_default(),
        path.display(),
    );
    Err(collect_error(py, &message))
}

pub fn collect_module(
    py: Python<'_>,
    rootdir: &Path,
    path: &Path,
    items: &mut Vec<TestItem>,
    registry: &mut FixtureRegistry,
    hooks: &mut Vec<crate::session::PyHook>,
    filters: &NameFilters,
) -> PyResult<()> {
    let (basedir, module_name) = module_name_for(path);
    sys_path_prepend(py, &basedir)?;
    let module = py.import(module_name.as_str())?;
    // pytest's import_path raises ImportPathMismatchError when a module of the
    // same dotted name was already imported from a different file (e.g. two
    // test files sharing a basename in different dirs). We import by name and
    // get the cached module back, so mirror that check explicitly.
    check_import_path_mismatch(py, &module, &module_name, path)?;
    let nodeid_base = file_nodeid(rootdir, path);

    register_pytest_plugins(py, &module, registry, hooks)?;
    // Plugin/conftest pytest_generate_tests impls (e.g. pytest-repeat) run on
    // the metafunc alongside any module-level one. Scope conftest-registered
    // hooks to this file's subtree (baseid is a directory prefix, "" for the
    // rootdir conftest) so a sibling directory's conftest doesn't also fire —
    // matches gethookproxy(fspath) upstream.
    let extra_generate_hooks: Vec<Py<PyAny>> = hooks
        .iter()
        .filter(|hook| {
            hook.name == "pytest_generate_tests" && nodeid_base.starts_with(&hook.baseid)
        })
        .map(|hook| hook.func.clone_ref(py))
        .collect();
    // pytest_pycollect_makemodule: a conftest may return a custom Module
    // subclass (e.g. `MyModule.from_parent(...)`) for this file. We honor the
    // returned node's class name for the --collect-only tree label.
    let custom_module_class = if has_pycollect_makemodule_hook(py, hooks) {
        call_pycollect_makemodule_hook(py, path, rootdir, module.as_any())
    } else {
        None
    };
    let makeitem_hook = has_pycollect_makeitem_hook(py, hooks);
    let module_items_start = items.len();
    introspect_namespace(
        py,
        &module,
        &nodeid_base,
        &module_name,
        path,
        items,
        registry,
        &extra_generate_hooks,
        makeitem_hook,
        filters,
    )?;
    if let Some(class_name) = custom_module_class {
        for item in items.iter_mut().skip(module_items_start) {
            item.collector_class = class_name.clone();
        }
    }
    Ok(())
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
            func_class: String::new(),
            py_node: None,
            max_param_scope: crate::fixture::Scope::Function,
            scope_sort_keys: Vec::new(),
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
            func_class: String::new(),
            py_node: None,
            max_param_scope: crate::fixture::Scope::Function,
            scope_sort_keys: Vec::new(),
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
