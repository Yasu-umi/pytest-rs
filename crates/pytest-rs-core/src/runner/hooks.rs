//! Per-item runtest hook firing (logreport, setup/call/teardown wrappers).

#[allow(unused_imports)]
use super::*;
use crate::collect::TestItem;
use crate::python;
use crate::report::TestReport;
use crate::session::Session;

/// Fire conftest pytest_runtest_{setup,call,teardown} hooks for an item
/// (visibility-scoped by the conftest's directory, item kwarg).
/// pytest_runtest_logreport conftest hooks, fired once per report as it is
/// produced (pytest streams reports through this hook the same way).
pub(crate) fn fire_logreport_hooks(
    py: Python<'_>,
    session: &Session,
    report: &TestReport,
    lineno: Option<u32>,
    item: Option<&TestItem>,
) {
    let funcs: Vec<Py<PyAny>> = session
        .py_hooks
        .iter()
        .filter(|hook| {
            hook.name == "pytest_runtest_logreport"
                && report.nodeid.starts_with(hook.baseid.as_str())
        })
        .map(|hook| hook.func.clone_ref(py))
        .collect();
    let delegated = session.custom_reporter.is_some();
    let proxy = match python::make_report_proxy(py, report, lineno) {
        Ok(proxy) => proxy,
        Err(err) => {
            eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
            return;
        }
    };
    // Populate keywords from the item's marks so conftest pytest_terminal_summary
    // hooks (which receive this report via terminalreporter.stats) can inspect marks.
    if let Some(item) = item {
        let _ = (|| -> PyResult<()> {
            use pyo3::types::{PyAnyMethods, PyDict, PyDictMethods};
            let kw = PyDict::new(py);
            for mark in &item.marks {
                kw.set_item(&mark.name, mark.obj.bind(py))?;
            }
            for (name, obj) in super::marks::added_marks(py) {
                kw.set_item(&name, obj.bind(py))?;
            }
            proxy.bind(py).setattr("keywords", kw)?;
            Ok(())
        })();
    }
    for func in &funcs {
        if let Err(err) = python::call_py_hook(py, func, &[("report", proxy.clone_ref(py))]) {
            eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
        }
    }
    // The replacement reporter's pytest_runtest_logreport runs after the
    // conftest impls (pluggy LIFO: the reporter registered first).
    if delegated {
        python::reporter_logreport(py, proxy.bind(py));
    } else {
        // Native mode: feed stats so conftest pytest_terminal_summary hooks
        // can access terminalreporter.stats['passed'] etc.
        python::reporter_feed_default(py, proxy.bind(py));
    }
}

/// A `pytest_report_teststatus` hook result: the verbose word and any
/// explicit markup codes the hook attached to it (a `(word, {name: True})`
/// tuple, upstream). The category/letter members of the triple are parsed
/// for shape but not yet consumed (stats stay outcome-driven here).
pub(crate) struct TestStatus {
    pub word: String,
    pub markup: Option<Vec<u8>>,
}

/// Resolve a report through the registered `pytest_report_teststatus`
/// conftest/plugin hooks (firstresult: first non-None wins, pluggy order).
/// Returns None when no hook claims the report, so the caller falls back
/// to the built-in outcome word/color.
pub(crate) fn report_teststatus(
    py: Python<'_>,
    session: &Session,
    report: &TestReport,
    lineno: Option<u32>,
) -> Option<TestStatus> {
    let funcs: Vec<Py<PyAny>> = session
        .py_hooks
        .iter()
        .filter(|hook| {
            hook.name == "pytest_report_teststatus"
                && report.nodeid.starts_with(hook.baseid.as_str())
        })
        .map(|hook| hook.func.clone_ref(py))
        .collect();
    if funcs.is_empty() {
        return None;
    }
    let proxy = python::make_report_proxy(py, report, lineno).ok()?;
    for func in funcs {
        let result = match python::call_py_hook(py, &func, &[("report", proxy.clone_ref(py))]) {
            Ok(result) => result,
            Err(err) => {
                eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
                continue;
            }
        };
        let bound = result.bind(py);
        if bound.is_none() {
            continue;
        }
        if let Some(status) = TestStatus::from_py(bound) {
            return Some(status);
        }
    }
    None
}

impl TestStatus {
    /// Parse a `(category, letter, word)` triple, where `word` may itself
    /// be a `(word, markup_dict)` tuple carrying explicit color markup.
    fn from_py(value: &Bound<'_, PyAny>) -> Option<TestStatus> {
        let _category: String = value.get_item(0).ok()?.extract().ok()?;
        let _letter: String = value.get_item(1).ok()?.extract().ok()?;
        let word_item = value.get_item(2).ok()?;
        let (word, markup) = if let Ok((word, markup)) =
            word_item.extract::<(String, Bound<'_, pyo3::types::PyDict>)>()
        {
            let codes: Vec<u8> = markup
                .iter()
                .filter_map(|(k, v)| {
                    let name: String = k.extract().ok()?;
                    let on: bool = v.extract().unwrap_or(false);
                    on.then(|| crate::tw::markup_code(&name)).flatten()
                })
                .collect();
            (word, Some(codes))
        } else {
            (word_item.extract::<String>().ok()?, None)
        };
        Some(TestStatus { word, markup })
    }
}

pub(crate) fn fire_runtest_py_hooks(
    py: Python<'_>,
    session: &Session,
    item: &TestItem,
    name: &str,
) -> PyResult<()> {
    let funcs: Vec<Py<PyAny>> = session
        .py_hooks
        .iter()
        .filter(|hook| hook.name == name && item.nodeid.starts_with(hook.baseid.as_str()))
        .map(|hook| hook.func.clone_ref(py))
        .collect();
    if funcs.is_empty() {
        return Ok(());
    }
    let node = python::make_node(py, item)?;
    for func in funcs {
        python::call_py_hook(py, &func, &[("item", node.clone_ref(py))])?;
    }
    Ok(())
}

/// Start `name` py hookwrappers around a phase: generator-function impls
/// (pluggy wrapper style, e.g. pytest-timeout's timer) advance to their
/// yield and are returned so the caller finishes them once the wrapped
/// phase is over. Plain impls either run immediately (`call_plain`, the
/// pytest_runtest_call behavior) or are skipped — a plain
/// pytest_runtest_protocol impl REPLACES the protocol upstream
/// (firstresult), which pytest-rs does not emulate; invoking it for side
/// effects would run foreign protocol code on top of ours.
pub(crate) fn start_runtest_py_wrappers(
    py: Python<'_>,
    session: &Session,
    item: &TestItem,
    name: &str,
    call_plain: bool,
) -> PyResult<Vec<Py<PyAny>>> {
    let funcs: Vec<Py<PyAny>> = session
        .py_hooks
        .iter()
        .filter(|hook| hook.name == name && item.nodeid.starts_with(hook.baseid.as_str()))
        .map(|hook| hook.func.clone_ref(py))
        .collect();
    if funcs.is_empty() {
        return Ok(Vec::new());
    }
    let node = python::make_node(py, item)?;
    let next_fn = py.import("builtins")?.getattr("next")?;
    let isgenfunc = py.import("inspect")?.getattr("isgeneratorfunction")?;
    let mut wrappers = Vec::new();
    for func in funcs {
        if !isgenfunc.call1((func.bind(py),))?.extract::<bool>()? {
            if call_plain {
                python::call_py_hook(py, &func, &[("item", node.clone_ref(py))])?;
            }
            continue;
        }
        let result = python::call_py_hook_raw(py, &func, &[("item", node.clone_ref(py))])?;
        let bound = result.bind(py);
        match next_fn.call1((bound,)) {
            Ok(_) => wrappers.push(result.clone_ref(py)),
            Err(err) if err.is_instance_of::<pyo3::exceptions::PyStopIteration>(py) => {}
            Err(err) => return Err(err),
        }
    }
    Ok(wrappers)
}

/// Finish started hookwrappers in reverse order (pluggy unwinds LIFO).
pub(crate) fn finish_runtest_py_wrappers(py: Python<'_>, wrappers: &[Py<PyAny>]) -> PyResult<()> {
    for wrapper in wrappers.iter().rev() {
        match wrapper.bind(py).call_method1("send", (py.None(),)) {
            Ok(_) => {
                return Err(pyo3::exceptions::PyRuntimeError::new_err(
                    "hook wrapper yielded more than once",
                ));
            }
            Err(err) if err.is_instance_of::<pyo3::exceptions::PyStopIteration>(py) => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}
