//! The only module allowed to touch `Python<'py>` / `Bound`.
//!
//! Engine structs store GIL-independent `Py<PyAny>` handles and re-bind
//! them here per GIL session.

use std::path::{Path, PathBuf};

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyModule, PyTuple};

use crate::collect::{MarkData, TestItem, file_nodeid, module_name_for};
use crate::fixture::{FixtureDef, FixtureRegistry, Scope};

const PYTEST_SHIM: &str = include_str!("../py/pytest/__init__.py");

/// Set up the embedded interpreter for a run: write the pytest shim package
/// to a temp dir and prepend it to sys.path so `import pytest` resolves to us.
pub fn install_shim(py: Python<'_>) -> PyResult<PathBuf> {
    let shim_root = std::env::temp_dir().join(format!("pytest-rs-{}", std::process::id()));
    let pkg_dir = shim_root.join("pytest");
    std::fs::create_dir_all(&pkg_dir)
        .map_err(|e| pyo3::exceptions::PyOSError::new_err(e.to_string()))?;
    std::fs::write(pkg_dir.join("__init__.py"), PYTEST_SHIM)
        .map_err(|e| pyo3::exceptions::PyOSError::new_err(e.to_string()))?;
    sys_path_prepend(py, &shim_root)?;
    Ok(shim_root)
}

pub fn sys_path_prepend(py: Python<'_>, path: &Path) -> PyResult<()> {
    let sys_path = py.import("sys")?.getattr("path")?;
    let sys_path = sys_path.cast::<PyList>().map_err(PyErr::from)?;
    let entry = path.to_string_lossy().to_string();
    if !sys_path.contains(&entry)? {
        sys_path.insert(0, entry)?;
    }
    Ok(())
}

/// Names of the parameters of a Python callable, in order.
pub fn param_names(py: Python<'_>, func: &Bound<'_, PyAny>) -> PyResult<Vec<String>> {
    let signature = py.import("inspect")?.getattr("signature")?.call1((func,))?;
    let params = signature.getattr("parameters")?;
    params
        .try_iter()?
        .map(|key| key?.extract::<String>())
        .collect()
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
pub fn collect_module(
    py: Python<'_>,
    rootdir: &Path,
    path: &Path,
    items: &mut Vec<TestItem>,
    registry: &mut FixtureRegistry,
) -> PyResult<()> {
    let (basedir, module_name) = module_name_for(path);
    sys_path_prepend(py, &basedir)?;
    let module = py.import(module_name.as_str())?;
    let nodeid_base = file_nodeid(rootdir, path);

    introspect_namespace(
        py,
        &module,
        &nodeid_base,
        &module_name,
        path,
        items,
        registry,
    )
}

/// Import a conftest.py; its fixtures are visible to all items under its
/// directory.
pub fn collect_conftest(
    py: Python<'_>,
    rootdir: &Path,
    path: &Path,
    registry: &mut FixtureRegistry,
) -> PyResult<()> {
    let (basedir, module_name) = module_name_for(path);
    sys_path_prepend(py, &basedir)?;
    let module = py.import(module_name.as_str())?;
    let dir_nodeid = file_nodeid(rootdir, path.parent().unwrap_or(rootdir));
    let baseid = if dir_nodeid.is_empty() || dir_nodeid == "." {
        String::new()
    } else {
        format!("{dir_nodeid}/")
    };
    register_fixtures_from(py, &module, &baseid, registry)
}

fn introspect_namespace(
    py: Python<'_>,
    module: &Bound<'_, PyModule>,
    nodeid_base: &str,
    module_name: &str,
    path: &Path,
    items: &mut Vec<TestItem>,
    registry: &mut FixtureRegistry,
) -> PyResult<()> {
    register_fixtures_from(py, module, &format!("{nodeid_base}::"), registry)?;

    let dict = module.dict();
    let mut names: Vec<(String, Bound<'_, PyAny>)> = dict
        .iter()
        .filter_map(|(k, v)| k.extract::<String>().ok().map(|name| (name, v)))
        .collect();
    // Module dicts preserve definition order in CPython; keep it.
    for (name, value) in names.drain(..) {
        if !name.starts_with("test_") || !value.is_callable() {
            continue;
        }
        // Only collect functions defined in (or imported into) this module
        // that are not fixtures.
        if value.hasattr("_pytestfixturefunction")? {
            continue;
        }
        let flags = async_flags(py, &value)?;
        let fixture_names = param_names(py, &value)?;
        let marks = read_marks(py, &value)?;
        items.push(TestItem {
            nodeid: format!("{nodeid_base}::{name}"),
            path: path.to_path_buf(),
            module_name: module_name.to_string(),
            func_name: name,
            func: value.unbind(),
            is_coroutine: flags.is_coroutine,
            fixture_names,
            marks,
        });
    }
    Ok(())
}

fn register_fixtures_from(
    py: Python<'_>,
    module: &Bound<'_, PyModule>,
    baseid: &str,
    registry: &mut FixtureRegistry,
) -> PyResult<()> {
    for (key, value) in module.dict().iter() {
        let Ok(_name) = key.extract::<String>() else {
            continue;
        };
        if !value.is_callable() || !value.hasattr("_pytestfixturefunction")? {
            continue;
        }
        let marker = value.getattr("_pytestfixturefunction")?;
        let scope_str: String = marker.getattr("scope")?.extract()?;
        let scope = Scope::parse(&scope_str).unwrap_or(Scope::Function);
        let autouse: bool = marker.getattr("autouse")?.extract()?;
        let explicit_name: Option<String> = marker.getattr("name")?.extract()?;
        let name = explicit_name.unwrap_or(_name);
        let flags = async_flags(py, &value)?;
        let param_names = param_names(py, &value)?;
        registry.register(FixtureDef {
            name,
            func: value.unbind(),
            scope,
            autouse,
            is_coroutine: flags.is_coroutine,
            is_generator: flags.is_generator,
            is_async_gen: flags.is_async_gen,
            param_names,
            baseid: baseid.to_string(),
        });
    }
    Ok(())
}

fn read_marks(py: Python<'_>, func: &Bound<'_, PyAny>) -> PyResult<Vec<MarkData>> {
    let mut marks = Vec::new();
    if let Ok(pytestmark) = func.getattr("pytestmark") {
        for mark in pytestmark.try_iter()? {
            let mark = mark?;
            let name: String = mark.getattr("name")?.extract()?;
            marks.push(MarkData {
                name,
                obj: mark.unbind(),
            });
        }
    }
    let _ = py;
    Ok(marks)
}

/// Format a PyErr as a native-style traceback string.
pub fn format_exception(py: Python<'_>, err: &PyErr) -> String {
    let result: PyResult<String> = (|| {
        let traceback = py.import("traceback")?;
        let formatted = traceback.call_method1("format_exception", (err.value(py),))?;
        let lines: Vec<String> = formatted.extract()?;
        Ok(lines.join(""))
    })();
    result.unwrap_or_else(|_| format!("{err}"))
}

/// Is this error an instance of the shim's `Skipped` outcome?
pub fn is_skipped(py: Python<'_>, err: &PyErr) -> bool {
    err_matches_shim(py, err, "Skipped")
}

/// Is this error an instance of the shim's `XFailed` outcome?
pub fn is_xfailed(py: Python<'_>, err: &PyErr) -> bool {
    err_matches_shim(py, err, "XFailed")
}

fn err_matches_shim(py: Python<'_>, err: &PyErr, class_name: &str) -> bool {
    py.import("pytest")
        .and_then(|m| m.getattr(class_name))
        .map(|cls| err.matches(py, &cls).unwrap_or(false))
        .unwrap_or(false)
}

/// Outcome message (e.g. skip reason) from a shim OutcomeException.
pub fn outcome_msg(py: Python<'_>, err: &PyErr) -> Option<String> {
    err.value(py)
        .getattr("msg")
        .ok()
        .and_then(|m| m.extract::<Option<String>>().ok())
        .flatten()
}

/// Call a Python callable with keyword arguments resolved from fixtures.
pub fn call_with_kwargs<'py>(
    py: Python<'py>,
    func: &Py<PyAny>,
    kwargs: &[(String, Py<PyAny>)],
) -> PyResult<Bound<'py, PyAny>> {
    let dict = PyDict::new(py);
    for (name, value) in kwargs {
        dict.set_item(name, value.bind(py))?;
    }
    let empty = PyTuple::empty(py);
    func.bind(py).call(empty, Some(&dict))
}

/// Resume a suspended sync generator fixture, expecting StopIteration.
pub fn finalize_generator(py: Python<'_>, generator: &Py<PyAny>) -> PyResult<()> {
    let builtins = py.import("builtins")?;
    match builtins.getattr("next")?.call1((generator.bind(py),)) {
        Ok(_) => Err(pyo3::exceptions::PyRuntimeError::new_err(
            "fixture generator yielded more than once",
        )),
        Err(err) if err.is_instance_of::<pyo3::exceptions::PyStopIteration>(py) => Ok(()),
        Err(err) => Err(err),
    }
}

/// Advance a generator fixture to its first yield, returning the value.
pub fn next_value<'py>(
    py: Python<'py>,
    generator: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    py.import("builtins")?.getattr("next")?.call1((generator,))
}
