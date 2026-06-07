//! skip/skipif/xfail mark evaluation for the run phases.

#[allow(unused_imports)]
use super::*;
use crate::collect::TestItem;
use crate::config::Config;
use crate::python;
use crate::session::Session;

/// The item's marks as (name, mark) pairs for the pytest._skipping shim.
pub(crate) fn marks_for_eval(py: Python<'_>, item: &TestItem) -> Vec<(String, Py<PyAny>)> {
    item.marks
        .iter()
        .map(|mark| (mark.name.clone(), mark.obj.clone_ref(py)))
        .collect()
}

/// conftest pytest_markeval_namespace hook results (usually none).
pub(crate) fn markeval_namespaces(
    py: Python<'_>,
    session: &Session,
    item: &TestItem,
) -> Vec<Py<PyAny>> {
    // Only conftests on the item's path apply; closest first (upstream's
    // LIFO hook results), so the deepest conftest's names win.
    let mut hooks: Vec<&crate::session::PyHook> = session
        .py_hooks
        .iter()
        .filter(|hook| hook.name == "pytest_markeval_namespace")
        .filter(|hook| hook.baseid.is_empty() || item.nodeid.starts_with(hook.baseid.as_str()))
        .collect();
    hooks.sort_by_key(|hook| std::cmp::Reverse(hook.baseid.len()));
    if hooks.is_empty() {
        return Vec::new();
    }
    let config_obj = python::existing_py_config(py);
    hooks
        .iter()
        .filter_map(|hook| {
            let kwargs: Vec<(&str, Py<PyAny>)> = match &config_obj {
                Some(config) => vec![("config", config.clone_ref(py))],
                None => Vec::new(),
            };
            python::call_py_hook(py, &hook.func, &kwargs).ok()
        })
        .collect()
}

/// pytest evaluate_skip_marks: Some((reason, from_pytestmark)) when the item
/// should skip. Errors (bad mark usage, conditions) report as setup errors.
pub(crate) fn evaluate_skip_marks(
    py: Python<'_>,
    session: &Session,
    item: &TestItem,
) -> PyResult<Option<(String, bool)>> {
    if !item
        .marks
        .iter()
        .any(|mark| mark.name == "skip" || mark.name == "skipif")
    {
        return Ok(None);
    }
    let config_obj = python::existing_py_config(py).unwrap_or_else(|| py.None());
    py.import("pytest._skipping")?
        .call_method1(
            "evaluate_skip_marks",
            (
                marks_for_eval(py, item),
                item.module_name.as_str(),
                config_obj,
                markeval_namespaces(py, session, item),
            ),
        )?
        .extract()
}

/// Evaluated @pytest.mark.xfail data (pytest's Xfail).
pub(crate) struct XfailEval {
    pub(crate) reason: String,
    pub(crate) run: bool,
    pub(crate) strict: bool,
    pub(crate) raises: Option<Py<PyAny>>,
}

/// Marks added at runtime via node.add_marker / request.applymarker.
pub(crate) fn added_marks(py: Python<'_>) -> Vec<(String, Py<PyAny>)> {
    py.import("pytest._node")
        .and_then(|m| m.call_method0("added_marks"))
        .and_then(|marks| marks.extract())
        .unwrap_or_default()
}

/// `raises=` kwarg: only a matching exception counts as an expected failure.
pub(crate) fn xfail_raises_ok(py: Python<'_>, xfailed: &Option<XfailEval>, err: &PyErr) -> bool {
    match xfailed.as_ref().and_then(|xf| xf.raises.as_ref()) {
        Some(raises) => err.matches(py, raises.bind(py)).unwrap_or(false),
        None => true,
    }
}

/// pytest evaluate_xfail_marks: the first triggered xfail mark, if any.
/// `extra` carries dynamically added marks (closest, so they win).
pub(crate) fn evaluate_xfail_marks(
    py: Python<'_>,
    session: &Session,
    config: &Config,
    item: &TestItem,
    extra: &[(String, Py<PyAny>)],
) -> PyResult<Option<XfailEval>> {
    // Unmarked items (the common case) never enter Python.
    if !item.marks.iter().any(|mark| mark.name == "xfail")
        && !extra.iter().any(|(name, _)| name == "xfail")
    {
        return Ok(None);
    }
    // Strict default: strict_xfail, then strict, then the pre-9 xfail_strict.
    let strict_default = matches!(
        config
            .get_ini("strict_xfail")
            .or_else(|| config.get_ini("strict"))
            .or_else(|| config.get_ini("xfail_strict"))
            .map(str::trim),
        Some("true") | Some("True") | Some("1")
    );
    let config_obj = python::existing_py_config(py).unwrap_or_else(|| py.None());
    let mut marks: Vec<(String, Py<PyAny>)> = extra
        .iter()
        .map(|(name, obj)| (name.clone(), obj.clone_ref(py)))
        .collect();
    marks.extend(marks_for_eval(py, item));
    let result = py.import("pytest._skipping")?.call_method1(
        "evaluate_xfail_marks",
        (
            marks,
            item.module_name.as_str(),
            config_obj,
            strict_default,
            markeval_namespaces(py, session, item),
        ),
    )?;
    if result.is_none() {
        return Ok(None);
    }
    let (reason, run, strict, raises): (String, bool, bool, Option<Py<PyAny>>) =
        result.extract()?;
    Ok(Some(XfailEval {
        reason,
        run,
        strict,
        raises,
    }))
}
