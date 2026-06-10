//! PyErr classification and traceback formatting.

#[allow(unused_imports)]
use super::*;

/// unittest.SkipTest raised by an xunit setup hook (e.g. setUpModule)
/// becomes a pytest Skipped, like upstream's makereport conversion.
pub fn map_skiptest(py: Python<'_>, err: PyErr) -> PyErr {
    let is_skiptest = py
        .import("unittest")
        .and_then(|m| m.getattr("SkipTest"))
        .map(|skiptest| err.matches(py, &skiptest).unwrap_or(false))
        .unwrap_or(false);
    if !is_skiptest {
        return err;
    }
    let msg = err.value(py).to_string();
    match py
        .import("pytest._outcomes")
        .and_then(|m| m.getattr("Skipped"))
        .and_then(|skipped| skipped.call1((msg,)))
    {
        Ok(exc) => PyErr::from_value(exc),
        Err(_) => err,
    }
}

/// Whether the error is an ImportError (ModuleNotFoundError included): a
/// test module that fails to import gets pytest's wrapped CollectError.
pub fn is_import_error(py: Python<'_>, err: &PyErr) -> bool {
    err.is_instance_of::<pyo3::exceptions::PyImportError>(py)
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

/// Format a test failure pytest-style (per --tb), falling back to the
/// native traceback.
/// Extract the short crash message from an exception (reprcrash.message equivalent).
pub fn crash_message(py: Python<'_>, err: &PyErr) -> Option<String> {
    py.import("pytest._tb")
        .and_then(|m| m.call_method1("crash_message", (err.value(py),)))
        .and_then(|v| v.extract())
        .ok()
}

pub fn format_test_failure(py: Python<'_>, err: &PyErr, style: &str) -> String {
    let result: PyResult<String> = (|| {
        py.import("pytest._tb")?
            .call_method1("format_exception", (err.value(py), style))?
            .extract()
    })();
    result.unwrap_or_else(|_| format_exception(py, err))
}

/// An explicit "file.py:line" the raiser attached to a Skipped exception
/// (`_location`), overriding traceback-derived skip locations.
pub fn skip_location_override(py: Python<'_>, err: &PyErr) -> Option<String> {
    err.value(py).getattr("_location").ok()?.extract().ok()
}

/// Construct a pytest.UsageError as a PyErr.
pub fn usage_error(py: Python<'_>, message: &str) -> PyErr {
    let result: PyResult<PyErr> = (|| {
        let cls = py.import("pytest")?.getattr("UsageError")?;
        Ok(PyErr::from_value(cls.call1((message,))?))
    })();
    result.unwrap_or_else(|_| pyo3::exceptions::PyRuntimeError::new_err(message.to_string()))
}

/// Is this error a pytest.UsageError?
pub fn is_usage_error(py: Python<'_>, err: &PyErr) -> bool {
    py.import("pytest")
        .and_then(|m| m.getattr("UsageError"))
        .map(|cls| err.matches(py, &cls).unwrap_or(false))
        .unwrap_or(false)
}

/// Is this error an instance of the shim's `Skipped` outcome?
pub fn is_skipped(py: Python<'_>, err: &PyErr) -> bool {
    err_matches_shim(py, err, "Skipped")
}

/// The session exit code this error forces, if it is a session-aborting
/// one: pytest.exit (its returncode, default INTERRUPTED) or Ctrl-C.
/// The "!!! ... !!!" banner text for a session abort (pytest.exit /
/// Ctrl-C), e.g. "_pytest.outcomes.Exit: foo".
pub fn session_abort_banner(py: Python<'_>, err: &PyErr) -> Option<String> {
    if err.is_instance_of::<pyo3::exceptions::PyKeyboardInterrupt>(py) {
        return Some("KeyboardInterrupt".to_string());
    }
    if err_matches_shim(py, err, "Exit") {
        let msg: String = err
            .value(py)
            .getattr("msg")
            .ok()
            .and_then(|msg| msg.extract().ok())
            .unwrap_or_default();
        return Some(if msg.is_empty() {
            "_pytest.outcomes.Exit".to_string()
        } else {
            format!("_pytest.outcomes.Exit: {msg}")
        });
    }
    None
}

pub fn session_abort_code(py: Python<'_>, err: &PyErr) -> Option<i32> {
    if err.is_instance_of::<pyo3::exceptions::PyKeyboardInterrupt>(py) {
        return Some(crate::report::exit_code::INTERRUPTED);
    }
    if err_matches_shim(py, err, "Exit") {
        let code = err
            .value(py)
            .getattr("returncode")
            .ok()
            .and_then(|code| code.extract::<i32>().ok())
            .unwrap_or(crate::report::exit_code::INTERRUPTED);
        return Some(code);
    }
    None
}

/// Classify a module-import error as a module-level skip.
/// Some(Ok(reason)): the whole module skips (allow_module_level=True or
/// unittest.SkipTest). Some(Err(message)): pytest.skip misused at module
/// level. None: not a skip at all.
pub fn module_level_skip(py: Python<'_>, err: &PyErr) -> Option<Result<String, String>> {
    let skiptest = py
        .import("unittest")
        .and_then(|m| m.getattr("SkipTest"))
        .ok();
    if let Some(skiptest) = skiptest
        && err.matches(py, &skiptest).unwrap_or(false)
    {
        return Some(Ok(err.value(py).to_string()));
    }
    if !is_skipped(py, err) {
        return None;
    }
    let value = err.value(py);
    let allowed = value
        .getattr("allow_module_level")
        .and_then(|allow| allow.extract::<bool>())
        .unwrap_or(false);
    if allowed {
        let reason = value
            .getattr("msg")
            .ok()
            .and_then(|msg| msg.extract::<String>().ok())
            .unwrap_or_default();
        Some(Ok(reason))
    } else {
        Some(Err(
            "Using pytest.skip outside of a test will skip the entire module. \
             If that's your intention, pass `allow_module_level=True` instead."
                .to_string(),
        ))
    }
}

/// Is this error an instance of the shim's `XFailed` outcome?
pub fn is_xfailed(py: Python<'_>, err: &PyErr) -> bool {
    err_matches_shim(py, err, "XFailed")
}

pub(crate) fn err_matches_shim(py: Python<'_>, err: &PyErr, class_name: &str) -> bool {
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

/// "relpath:lineno" of the frame that raised, for -r summary grouping.
pub fn raise_location(py: Python<'_>, err: &PyErr) -> Option<String> {
    let tb_module = py.import("pytest._tb").ok()?;
    tb_module
        .getattr("raise_location")
        .ok()?
        .call1((err.value(py),))
        .ok()?
        .extract::<Option<String>>()
        .ok()
        .flatten()
}
