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

/// Format a hook exception for the INTERNALERROR banner. Upstream's
/// `Config.notify_exception` uses the native traceback style unless
/// `--full-trace` is set, which forces the long style (full source per
/// frame). Used for unexpected exceptions in pytest_configure /
/// pytest_sessionstart.
pub fn format_internal_error(py: Python<'_>, err: &PyErr, fulltrace: bool) -> String {
    if fulltrace {
        format_test_failure(py, err, "long")
    } else {
        format_exception(py, err)
    }
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

/// Like `crash_message` but keeps the exception-type prefix (subtests' SUBFAIL
/// lines show "AssertionError: assert ...", not the tryshort-stripped form).
pub fn crash_message_with_type(py: Python<'_>, err: &PyErr) -> Option<String> {
    py.import("pytest._tb")
        .and_then(|m| m.call_method1("crash_message_with_type", (err.value(py),)))
        .and_then(|v| v.extract())
        .ok()
}

/// Format a conftest.py import failure pytest-style: an "ImportError while
/// loading conftest '<path>'." header followed by a short traceback with
/// the import-machinery frames dropped (upstream's print_conftest_import_error).
pub fn format_conftest_import_error(py: Python<'_>, err: &PyErr, conftest_path: &str) -> String {
    let result: PyResult<String> = (|| {
        py.import("pytest._tb")?
            .call_method1(
                "format_conftest_import_error",
                (err.value(py), conftest_path),
            )?
            .extract()
    })();
    result.unwrap_or_else(|_| format_exception(py, err))
}

pub fn format_test_failure(py: Python<'_>, err: &PyErr, style: &str) -> String {
    let result: PyResult<String> = (|| {
        py.import("pytest._tb")?
            .call_method1("format_exception", (err.value(py), style))?
            .extract()
    })();
    result.unwrap_or_else(|_| format_exception(py, err))
}

/// Re-import a failed test module through `_pytest.pathlib.import_path` so the
/// resulting traceback carries pytest's own framework frames (`_pytest/...`),
/// which pytest shows only at high verbosity (`-vv`). We import test modules
/// with a plain `py.import`, whose traceback has only user frames; this surfaces
/// the framework frames faithfully. Returns the "short"-style formatted
/// traceback, or None if import_path is unavailable or the re-import
/// unexpectedly succeeds. Only used on an already-failing collection at -vv.
pub fn format_import_traceback_verbose(
    py: Python<'_>,
    rootdir: &std::path::Path,
    file: &std::path::Path,
) -> Option<String> {
    let pathlib = py.import("pathlib").ok()?;
    let path_cls = pathlib.getattr("Path").ok()?;
    let path_obj = path_cls.call1((file.to_string_lossy().as_ref(),)).ok()?;
    let root_obj = path_cls.call1((rootdir.to_string_lossy().as_ref(),)).ok()?;
    let import_path = py
        .import("_pytest.pathlib")
        .ok()?
        .getattr("import_path")
        .ok()?;
    let kwargs = pyo3::types::PyDict::new(py);
    kwargs.set_item("mode", "prepend").ok()?;
    kwargs.set_item("root", root_obj).ok()?;
    kwargs.set_item("consider_namespace_packages", false).ok()?;
    match import_path.call((path_obj,), Some(&kwargs)) {
        // Unexpectedly succeeded on re-import: fall back to the original error.
        Ok(_) => None,
        Err(err) => Some(format_test_failure(py, &err, "short")),
    }
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

/// Upstream's `print_usage_error`: each UsageError arg on stderr as
/// "ERROR: <arg>" followed by a blank line (`tw.line(f"ERROR: {msg}\n")`).
pub fn print_usage_error(py: Python<'_>, err: &PyErr) {
    let value = err.value(py);
    let args: Vec<String> = value
        .getattr("args")
        .and_then(|args| {
            args.try_iter()?
                .map(|arg| arg?.str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    if args.is_empty() {
        eprint_usage_error(&value.to_string());
    }
    args.iter().for_each(|arg| eprint_usage_error(arg));
}

/// One "ERROR: <message>" usage-error block (see [`print_usage_error`]).
pub fn eprint_usage_error(message: &str) {
    eprintln!("ERROR: {message}\n");
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
        return Some("Interrupted: KeyboardInterrupt".to_string());
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

/// Like `session_abort_code` but distinguishes `Exit(returncode=None)` from
/// `Exit(returncode=42)`.  Returns `Some(None)` when the Exit carries no
/// explicit returncode (caller should keep its current exit code).
pub fn exit_returncode(py: Python<'_>, err: &PyErr) -> Option<Option<i32>> {
    if !err_matches_shim(py, err, "Exit") {
        return None;
    }
    let code = err.value(py).getattr("returncode").ok().and_then(|v| {
        if v.is_none() {
            None
        } else {
            v.extract::<i32>().ok()
        }
    });
    Some(code)
}

pub fn exit_msg(py: Python<'_>, err: &PyErr) -> String {
    err.value(py)
        .getattr("msg")
        .and_then(|m| m.extract::<String>())
        .unwrap_or_default()
}

/// Upstream's `Config.notify_exception` for an exception that ends the
/// session (`wrap_session`'s `except BaseException` branch): record junit's
/// "internal" testcase, fire `pytest_internalerror` (the terminal reporter's
/// impl prints the `INTERNALERROR>` lines and returns True), print them here
/// only when no impl did, and resolve the exit code: `default_code`, unless a
/// hookimpl raises `Exit` (prints "Exit: {msg}" to stderr and overrides the
/// code when one was given), or the original exception was itself
/// `SystemExit` and no `Exit` was raised (prints "mainloop: caught unexpected
/// SystemExit!").
pub fn report_internal_error(
    py: Python<'_>,
    err: &PyErr,
    fulltrace: bool,
    default_code: i32,
) -> i32 {
    let msg = format_internal_error(py, err, fulltrace);
    super::junit_internal_error(py, &msg);
    let dispatch: PyResult<bool> = (|| {
        let exc_type = err.get_type(py);
        let exc_value = err.value(py).clone();
        let exc_tb = match err.traceback(py) {
            Some(tb) => tb.into_any(),
            None => py.None().into_bound(py),
        };
        let excinfo = py
            .import("pytest._raises")?
            .getattr("ExceptionInfo")?
            .call_method1("from_exc_info", ((exc_type, exc_value, exc_tb), true))?;
        let style_kwargs = pyo3::types::PyDict::new(py);
        style_kwargs.set_item("style", if fulltrace { "long" } else { "native" })?;
        let excrepr = py
            .import("pytest._internalerror")?
            .getattr("InternalErrorRepr")?
            .call1((
                msg.trim_end_matches('\n'),
                excinfo.call_method("getrepr", (), Some(&style_kwargs))?,
            ))?;
        let hook_caller = py
            .import("pytest._pluginmanager")?
            .getattr("pluginmanager")?
            .getattr("hook")?
            .getattr("pytest_internalerror")?;
        let kwargs = pyo3::types::PyDict::new(py);
        kwargs.set_item("excrepr", &excrepr)?;
        kwargs.set_item("excinfo", &excinfo)?;
        let results = hook_caller.call((), Some(&kwargs))?;
        Ok(results
            .try_iter()
            .map(|mut it| it.any(|r| r.is_ok_and(|r| r.is_truthy().unwrap_or(false))))
            .unwrap_or_else(|_| results.is_truthy().unwrap_or(false)))
    })();
    let print_fallback = || {
        super::reporter_ensure_newline(py);
        for line in msg.lines() {
            println!("INTERNALERROR> {line}");
        }
    };
    match dispatch {
        Err(hook_err) => {
            print_fallback();
            if let Some(code) = exit_returncode(py, &hook_err) {
                eprintln!("Exit: {}", exit_msg(py, &hook_err));
                return code.unwrap_or(default_code);
            }
            default_code
        }
        Ok(handled) => {
            if !handled {
                print_fallback();
            }
            if err.is_instance_of::<pyo3::exceptions::PySystemExit>(py) {
                eprintln!("mainloop: caught unexpected SystemExit!");
            }
            default_code
        }
    }
}

/// Hand a hookimpl's exception to the run loop, which ends the session
/// with INTERNALERROR once the current item is done (see
/// `pytest/_internalerror.py`). Returns false when it is no internal error
/// (`pytest.exit()`, `KeyboardInterrupt`); the caller then keeps its own
/// handling.
pub fn defer_internal_error(py: Python<'_>, err: &PyErr) -> bool {
    py.import("pytest._internalerror")
        .and_then(|m| m.getattr("DeferredInternalError"))
        .and_then(|cls| cls.call_method1("defer", (err.value(py),)))
        .and_then(|deferred| deferred.extract::<bool>())
        .unwrap_or(false)
}

/// [`defer_internal_error`], or print the error like before when it is
/// not one.
pub fn defer_or_print_internal_error(py: Python<'_>, err: &PyErr) {
    if !defer_internal_error(py, err) {
        eprintln!("INTERNAL ERROR: {}", format_exception(py, err));
    }
}

/// Whether a hookimpl deferred an exception the run loop has not taken yet.
/// Checked after every report, so it reads the class attribute through a
/// cached class object instead of importing and calling into Python.
pub fn internal_error_pending(py: Python<'_>) -> bool {
    thread_local! {
        static DEFERRED_CLASS: std::cell::OnceCell<Option<Py<PyAny>>> =
            const { std::cell::OnceCell::new() };
    }
    DEFERRED_CLASS.with(|cell| {
        cell.get_or_init(|| {
            py.import("pytest._internalerror")
                .and_then(|m| m.getattr("DeferredInternalError"))
                .ok()
                .map(Bound::unbind)
        })
        .as_ref()
        .and_then(|cls| cls.bind(py).getattr("_pending").ok())
        .is_some_and(|pending| !pending.is_none())
    })
}

/// The exception a hookimpl deferred with [`defer_internal_error`], if any.
pub fn take_internal_error(py: Python<'_>) -> Option<PyErr> {
    let exc = py
        .import("pytest._internalerror")
        .and_then(|m| m.getattr("DeferredInternalError"))
        .and_then(|cls| cls.call_method0("take"))
        .ok()?;
    if exc.is_none() {
        return None;
    }
    Some(PyErr::from_value(exc))
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
