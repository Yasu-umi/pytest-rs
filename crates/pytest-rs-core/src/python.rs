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

    let inspect = py.import("inspect")?;
    let isclass = inspect.getattr("isclass")?;
    let dict = module.dict();
    // Module dicts preserve definition order in CPython; keep it.
    for (key, value) in dict.iter() {
        let Ok(name) = key.extract::<String>() else {
            continue;
        };
        if isclass.call1((&value,))?.extract::<bool>()? {
            if name.starts_with("Test") {
                collect_class(
                    py,
                    &value,
                    &name,
                    nodeid_base,
                    module_name,
                    path,
                    items,
                    registry,
                )?;
            }
            continue;
        }
        if !name.starts_with("test_")
            || !value.is_callable()
            || value.hasattr("_pytestfixturefunction")?
        {
            continue;
        }
        let marks = read_marks(py, &value)?;
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
        )?;
    }
    Ok(())
}

/// Collect test methods (and class-level fixtures) from a Test* class.
#[allow(clippy::too_many_arguments)]
fn collect_class(
    py: Python<'_>,
    cls: &Bound<'_, PyAny>,
    cls_name: &str,
    nodeid_base: &str,
    module_name: &str,
    path: &Path,
    items: &mut Vec<TestItem>,
    registry: &mut FixtureRegistry,
) -> PyResult<()> {
    // Classes with a custom __init__ are not collected (pytest behavior).
    let cls_dict = cls.getattr("__dict__")?;
    if cls_dict.contains("__init__")? {
        return Ok(());
    }
    let class_nodeid = format!("{nodeid_base}::{cls_name}");
    let class_marks = read_marks(py, cls)?;

    for pair in cls_dict.call_method0("items")?.try_iter()? {
        let (name, value): (String, Bound<'_, PyAny>) = pair?.extract()?;
        if !value.is_callable() {
            continue;
        }
        if value.hasattr("_pytestfixturefunction")? {
            register_fixture_def(
                py,
                &name,
                &value,
                &format!("{class_nodeid}::"),
                true,
                registry,
            )?;
            continue;
        }
        if !name.starts_with("test_") {
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
        )?;
    }
    Ok(())
}

/// Push the (possibly parametrize-expanded) items for one test function.
#[allow(clippy::too_many_arguments)]
fn push_test_items(
    py: Python<'_>,
    items: &mut Vec<TestItem>,
    nodeid_prefix: &str,
    module_name: &str,
    path: &Path,
    name: &str,
    func: &Bound<'_, PyAny>,
    cls: Option<&Bound<'_, PyAny>>,
    marks: Vec<MarkData>,
) -> PyResult<()> {
    let flags = async_flags(py, func)?;
    let mut fixture_names = param_names(py, func)?;
    if cls.is_some() && fixture_names.first().map(String::as_str) == Some("self") {
        fixture_names.remove(0);
    }

    let variants = expand_parametrize(py, &marks)?;
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
            fixture_names: fixture_names.clone(),
            marks: item_marks,
            callspec: variant.params,
        });
    }
    Ok(())
}

struct ParamVariant {
    /// The "[...]" id suffix; None for unparametrized tests.
    id: Option<String>,
    params: Vec<(String, Py<PyAny>)>,
    /// Marks attached via pytest.param(..., marks=...).
    extra_marks: Vec<MarkData>,
}

/// Expand stacked @pytest.mark.parametrize marks into the cartesian product
/// of parameter sets. Marks appear in pytestmark order (bottom decorator
/// first); ids join in that order and later marks vary fastest.
fn expand_parametrize(py: Python<'_>, marks: &[MarkData]) -> PyResult<Vec<ParamVariant>> {
    struct Dim {
        /// (id_part, params, extra_marks) per value set.
        sets: Vec<(String, Vec<(String, Py<PyAny>)>, Vec<MarkData>)>,
    }

    let param_spec_cls = py.import("pytest")?.getattr("ParamSpec")?;
    let mut dims: Vec<Dim> = Vec::new();

    for mark in marks.iter().filter(|m| m.name == "parametrize") {
        let args = mark.obj.bind(py).getattr("args")?;
        let argnames_obj = args.get_item(0)?;
        let argvalues = args.get_item(1)?;
        let argnames: Vec<String> = match argnames_obj.extract::<String>() {
            Ok(joined) => joined.split(',').map(|s| s.trim().to_string()).collect(),
            Err(_) => argnames_obj.extract()?,
        };
        let explicit_ids: Option<Vec<Option<String>>> = mark
            .obj
            .bind(py)
            .getattr("kwargs")?
            .get_item("ids")
            .ok()
            .and_then(|ids| ids.extract().ok());

        let mut sets = Vec::new();
        for (index, value_set) in argvalues.try_iter()?.enumerate() {
            let value_set = value_set?;
            let (values, spec_id, extra_marks) = if value_set.is_instance(&param_spec_cls)? {
                let values: Vec<Bound<'_, PyAny>> = value_set
                    .getattr("values")?
                    .try_iter()?
                    .collect::<PyResult<_>>()?;
                let spec_id: Option<String> = value_set.getattr("id")?.extract()?;
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
                (values, spec_id, extra_marks)
            } else if argnames.len() > 1 {
                let values: Vec<Bound<'_, PyAny>> =
                    value_set.try_iter()?.collect::<PyResult<_>>()?;
                (values, None, Vec::new())
            } else {
                (vec![value_set], None, Vec::new())
            };

            if values.len() != argnames.len() {
                return Err(pyo3::exceptions::PyValueError::new_err(format!(
                    "parametrize: {} argnames but {} values",
                    argnames.len(),
                    values.len()
                )));
            }

            let id_part = spec_id
                .or_else(|| {
                    explicit_ids
                        .as_ref()
                        .and_then(|ids| ids.get(index).cloned().flatten())
                })
                .unwrap_or_else(|| {
                    let parts: Vec<String> = argnames
                        .iter()
                        .zip(values.iter())
                        .map(|(argname, value)| id_for_value(value, argname, index))
                        .collect();
                    parts.join("-")
                });
            let params: Vec<(String, Py<PyAny>)> = argnames
                .iter()
                .cloned()
                .zip(values.into_iter().map(Bound::unbind))
                .collect();
            sets.push((id_part, params, extra_marks));
        }
        dims.push(Dim { sets });
    }

    if dims.is_empty() {
        return Ok(vec![ParamVariant {
            id: None,
            params: Vec::new(),
            extra_marks: Vec::new(),
        }]);
    }

    // Odometer over dims; the last dim varies fastest, ids join in dim order.
    let mut variants = Vec::new();
    let mut indices = vec![0usize; dims.len()];
    'outer: loop {
        let mut id_parts = Vec::new();
        let mut params = Vec::new();
        let mut extra_marks = Vec::new();
        for (dim, &index) in dims.iter().zip(indices.iter()) {
            let (id_part, set_params, set_marks) = &dim.sets[index];
            id_parts.push(id_part.clone());
            for (name, value) in set_params {
                params.push((name.clone(), value.clone_ref(py)));
            }
            for mark in set_marks {
                extra_marks.push(MarkData {
                    name: mark.name.clone(),
                    obj: mark.obj.clone_ref(py),
                });
            }
        }
        variants.push(ParamVariant {
            id: Some(id_parts.join("-")),
            params,
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
    Ok(variants)
}

/// pytest-style id for one parameter value.
fn id_for_value(value: &Bound<'_, PyAny>, argname: &str, index: usize) -> String {
    if value.is_none() {
        return "None".to_string();
    }
    if let Ok(b) = value.cast::<pyo3::types::PyBool>() {
        return if b.is_true() { "True" } else { "False" }.to_string();
    }
    if let Ok(s) = value.extract::<String>() {
        return s;
    }
    if value.cast::<pyo3::types::PyInt>().is_ok() || value.cast::<pyo3::types::PyFloat>().is_ok() {
        if let Ok(repr) = value.repr() {
            return repr.to_string();
        }
    }
    format!("{argname}{index}")
}

fn register_fixtures_from(
    py: Python<'_>,
    module: &Bound<'_, PyModule>,
    baseid: &str,
    registry: &mut FixtureRegistry,
) -> PyResult<()> {
    for (key, value) in module.dict().iter() {
        let Ok(attr_name) = key.extract::<String>() else {
            continue;
        };
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
    let scope_str: String = marker.getattr("scope")?.extract()?;
    let scope = Scope::parse(&scope_str).unwrap_or(Scope::Function);
    let autouse: bool = marker.getattr("autouse")?.extract()?;
    let explicit_name: Option<String> = marker.getattr("name")?.extract()?;
    let name = explicit_name.unwrap_or_else(|| attr_name.to_string());
    let flags = async_flags(py, value)?;
    let mut param_names = param_names(py, value)?;
    if needs_instance && param_names.first().map(String::as_str) == Some("self") {
        param_names.remove(0);
    }
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
    });
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
    call_fixture(py, func, None, kwargs)
}

/// Call a fixture/test function, prepending the test class instance as
/// `self` when the definition lives inside a Test* class.
pub fn call_fixture<'py>(
    py: Python<'py>,
    func: &Py<PyAny>,
    instance: Option<&Py<PyAny>>,
    kwargs: &[(String, Py<PyAny>)],
) -> PyResult<Bound<'py, PyAny>> {
    let dict = PyDict::new(py);
    for (name, value) in kwargs {
        dict.set_item(name, value.bind(py))?;
    }
    match instance {
        Some(instance) => func.bind(py).call((instance.bind(py),), Some(&dict)),
        None => func.bind(py).call(PyTuple::empty(py), Some(&dict)),
    }
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
