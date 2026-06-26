//! One-call bridges into the shim services (logging, capture, junit, ...).

#[allow(unused_imports)]
use super::*;
use crate::collect::TestItem;
use crate::report::{Outcome, Phase};

static LOGGING_START_PHASE_FN: pyo3::sync::PyOnceLock<Py<PyAny>> = pyo3::sync::PyOnceLock::new();
static LOGGING_FINISH_ITEM_FN: pyo3::sync::PyOnceLock<Py<PyAny>> = pyo3::sync::PyOnceLock::new();
static CAPTURE_START_PHASE_FN: pyo3::sync::PyOnceLock<Py<PyAny>> = pyo3::sync::PyOnceLock::new();
static CAPTURE_FINISH_ITEM_FN: pyo3::sync::PyOnceLock<Py<PyAny>> = pyo3::sync::PyOnceLock::new();
static WCAPTURE_BEGIN_FILTERS_FN: pyo3::sync::PyOnceLock<Py<PyAny>> = pyo3::sync::PyOnceLock::new();
static WCAPTURE_END_FILTERS_FN: pyo3::sync::PyOnceLock<Py<PyAny>> = pyo3::sync::PyOnceLock::new();

fn logging_start_phase_fn(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    LOGGING_START_PHASE_FN
        .get_or_try_init(py, || {
            Ok(py
                .import("pytest._logging")?
                .getattr("start_phase")?
                .unbind())
        })
        .map(|f| f.bind(py).clone())
}

fn logging_finish_item_fn(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    LOGGING_FINISH_ITEM_FN
        .get_or_try_init(py, || {
            Ok(py
                .import("pytest._logging")?
                .getattr("finish_item")?
                .unbind())
        })
        .map(|f| f.bind(py).clone())
}

fn capture_start_phase_fn(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    CAPTURE_START_PHASE_FN
        .get_or_try_init(py, || {
            Ok(py
                .import("pytest._capture")?
                .getattr("start_phase")?
                .unbind())
        })
        .map(|f| f.bind(py).clone())
}

fn capture_finish_item_fn(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    CAPTURE_FINISH_ITEM_FN
        .get_or_try_init(py, || {
            Ok(py
                .import("pytest._capture")?
                .getattr("finish_item")?
                .unbind())
        })
        .map(|f| f.bind(py).clone())
}

fn wcapture_begin_filters_fn(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    WCAPTURE_BEGIN_FILTERS_FN
        .get_or_try_init(py, || {
            Ok(py
                .import("pytest._wcapture")?
                .getattr("begin_item_filters")?
                .unbind())
        })
        .map(|f| f.bind(py).clone())
}

fn wcapture_end_filters_fn(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    WCAPTURE_END_FILTERS_FN
        .get_or_try_init(py, || {
            Ok(py
                .import("pytest._wcapture")?
                .getattr("end_item_filters")?
                .unbind())
        })
        .map(|f| f.bind(py).clone())
}

/// Wire session-wide logging handlers (log_file / log_cli) from CLI + ini
/// settings; returns whether live (log_cli) logging is enabled.
pub fn configure_logging(py: Python<'_>, config: &crate::config::Config) -> bool {
    let setting = |cli: &str, ini: &str| -> Option<String> {
        config
            .get_value(cli)
            .map(str::to_string)
            .or_else(|| config.get_ini(ini).map(str::to_string))
    };
    let result: PyResult<bool> = (|| {
        let settings = pyo3::types::PyDict::new(py);
        for (cli, ini) in [
            ("log-level", "log_level"),
            ("log-format", "log_format"),
            ("log-date-format", "log_date_format"),
            ("log-cli-level", "log_cli_level"),
            ("log-cli-format", "log_cli_format"),
            ("log-cli-date-format", "log_cli_date_format"),
            ("log-file", "log_file"),
            ("log-file-level", "log_file_level"),
            ("log-file-mode", "log_file_mode"),
            ("log-file-format", "log_file_format"),
            ("log-file-date-format", "log_file_date_format"),
            ("log-disable", "log_disable"),
        ] {
            if let Some(value) = setting(cli, ini) {
                settings.set_item(ini, value)?;
            }
        }
        // log_cli is ini-only; --log-cli-level on the command line (not the
        // log_cli_level ini) also auto-enables live logging.
        if let Some(value) = config.get_ini("log_cli") {
            settings.set_item("log_cli", value)?;
        }
        if config.get_value("log-cli-level").is_some() {
            settings.set_item("log_cli_level_from_cli", "1")?;
        }
        let module = py.import("pytest._logging")?;
        module.getattr("configure")?.call1((settings,))?;
        module.getattr("log_cli_enabled")?.call0()?.extract()
    })();
    result.unwrap_or(false)
}

/// Relabel the live (log_cli) section header (start/finish/collection).
pub fn log_set_live_when(py: Python<'_>, when: &str) {
    let _ = (|| -> PyResult<()> {
        py.import("pytest._logging")?
            .getattr("set_live_when")?
            .call1((when,))?;
        Ok(())
    })();
}

/// Tell the subtests fixture how many failures remain in the --maxfail
/// budget (None = unlimited); exhausting it stops swallowing failures.
pub fn set_subtest_fail_budget(py: Python<'_>, budget: Option<usize>) {
    let _ = (|| -> PyResult<()> {
        py.import("pytest._subtests")?
            .getattr("set_fail_budget")?
            .call1((budget,))?;
        Ok(())
    })();
}

/// Enable/disable inline progress char printing from subtest __exit__.
pub fn set_subtest_inline_chars(py: Python<'_>, enabled: bool) {
    let _ = (|| -> PyResult<()> {
        py.import("pytest._subtests")?
            .getattr("set_inline_chars")?
            .call1((enabled,))?;
        Ok(())
    })();
}

/// How many progress chars the subtest __exit__ already printed inline.
pub fn pop_subtest_inline_count(py: Python<'_>) -> usize {
    py.import("pytest._subtests")
        .and_then(|m| m.getattr("pop_inline_count"))
        .and_then(|f| f.call0())
        .and_then(|r| r.extract())
        .unwrap_or(0)
}

/// Drain the subtests fixture accumulator into reports for this item.
/// Quiet subtest verbosity (the default) keeps only failed subtests,
/// matching upstream's pytest_report_teststatus filtering. The second
/// value counts failed fixture subtests (unittest subTest failures do
/// not fail the enclosing test upstream, so they are excluded).
pub fn pop_subtest_reports(
    py: Python<'_>,
    config: &crate::config::Config,
    item: &TestItem,
) -> (Vec<crate::report::TestReport>, usize) {
    let result: PyResult<(Vec<crate::report::TestReport>, usize)> = (|| {
        let module = py.import("pytest._subtests")?;
        let results = module.getattr("pop_results")?.call0()?;
        let style = config.get_value("tb").unwrap_or("long").to_string();
        let mut reports = Vec::new();
        let mut failed_fixture_subs = 0usize;
        for record in results.try_iter()? {
            let record = record?;
            let outcome_str: String = record.get_item("outcome")?.extract()?;
            let desc: String = record.get_item("desc")?.extract()?;
            let duration: f64 = record.get_item("duration")?.extract()?;
            let reason: String = record.get_item("reason")?.extract()?;
            let location: Option<String> = record.get_item("location")?.extract()?;
            let from_unittest: bool = record
                .call_method1("get", ("unittest", false))?
                .extract()
                .unwrap_or(false);
            let mut reprcrash_message = None;
            let (outcome, longrepr) = match outcome_str.as_str() {
                "failed" => {
                    let exc = record.get_item("exc")?;
                    let err = PyErr::from_value(exc);
                    if !from_unittest {
                        failed_fixture_subs += 1;
                    }
                    // A unittest subtest's SUBFAIL short-summary line keeps the
                    // exception type ("AssertionError: assert 4 < 4"); the
                    // `subtests` fixture path uses the tryshort form (just
                    // "assert False"), which the longrepr fallback already gives.
                    if from_unittest {
                        reprcrash_message = crate::python::crash_message_with_type(py, &err);
                    }
                    (
                        crate::report::Outcome::Failed,
                        Some(format_test_failure(py, &err, &style)),
                    )
                }
                "skipped" => (crate::report::Outcome::Skipped, Some(reason)),
                "xfailed" => (crate::report::Outcome::XFailed, Some(reason)),
                _ => (crate::report::Outcome::Passed, None),
            };
            let sections: Vec<(String, String)> = record
                .call_method1("get", ("sections", pyo3::types::PyList::empty(py)))
                .and_then(|s| s.extract())
                .unwrap_or_default();
            reports.push(crate::report::TestReport {
                nodeid: item.nodeid.clone(),
                phase: crate::report::Phase::Call,
                outcome,
                duration: std::time::Duration::from_secs_f64(duration),
                longrepr,
                location,
                subtest_desc: Some(desc),
                sections,
                rerun: false,
                xfail_longrepr: None,
                reprcrash_message,
                head_line: None,
            });
        }
        Ok((reports, failed_fixture_subs))
    })();
    result.unwrap_or_default()
}

/// Install session-wide warning capture (pytest default filters), then
/// apply the `filterwarnings` ini lines and -W specs on top (-W last, so
/// the command line takes precedence over the config file).
pub fn install_warning_capture(
    py: Python<'_>,
    ini_specs: &[String],
    w_specs: &[String],
) -> PyResult<()> {
    let wcapture = py.import("pytest._wcapture")?;
    wcapture.call_method0("install")?;
    let ini: Vec<String> = ini_specs.iter().map(|s| s.trim().to_string()).collect();
    let w: Vec<String> = w_specs.iter().map(|s| s.trim().to_string()).collect();
    wcapture.call_method1("apply_session_filters", (ini, w))?;
    Ok(())
}

/// The `config.cache` object (a pytest._cache.Cache) for this run.
pub(crate) fn cache_object<'py>(
    py: Python<'py>,
    config: &crate::config::Config,
) -> PyResult<Bound<'py, PyAny>> {
    let proxy = make_py_config(py, config)?;
    proxy.bind(py).getattr("cache")
}

/// Nodeids recorded as failed in cache/lastfailed (insertion order).
pub fn cache_lastfailed(py: Python<'_>, config: &crate::config::Config) -> Vec<String> {
    let read = || -> PyResult<Vec<String>> {
        let cache = cache_object(py, config)?;
        let value =
            cache.call_method1("get", ("cache/lastfailed", pyo3::types::PyDict::new(py)))?;
        // dict_keys is not extractable directly; materialize a list first.
        py.import("builtins")?
            .getattr("list")?
            .call1((value.call_method0("keys")?,))?
            .extract()
    };
    read().unwrap_or_default()
}

/// Persist cache/lastfailed and cache/nodeids at session end (done from the
/// pytest.cacheprovider shim so cache warnings carry pytest's locations).
pub fn cache_write_session(
    py: Python<'_>,
    config: &crate::config::Config,
    failed: &[String],
    nodeids: &[String],
) -> PyResult<()> {
    let dict = pyo3::types::PyDict::new(py);
    for nodeid in failed {
        dict.set_item(nodeid, true)?;
    }
    let proxy = make_py_config(py, config)?;
    py.import("pytest.cacheprovider")?
        .call_method1("write_session_cache", (proxy, dict, nodeids.to_vec()))?;
    Ok(())
}

/// The stepwise cache info: (cache_hit, error_msg).
/// cache_hit is Some((nodeid, test_count, age_str)) when a valid prior failure
/// is cached; error_msg is Some("error reading cache, ...") when the cache
/// exists but is corrupt/invalid.
#[allow(clippy::type_complexity)]
pub fn cache_stepwise(
    py: Python<'_>,
    config: &crate::config::Config,
) -> (
    Option<(String, Option<usize>, Option<String>)>,
    Option<String>,
) {
    let read = || -> PyResult<(Option<(String, Option<usize>, Option<String>)>, Option<String>)> {
        let cache = cache_object(py, config)?;
        let result = py
            .import("pytest._cache")?
            .call_method1("stepwise_info", (cache,))?;
        let last_failed: Option<String> = result.get_item(0)?.extract()?;
        let test_count: Option<usize> = result.get_item(1)?.extract()?;
        let age_str: Option<String> = result.get_item(2)?.extract()?;
        let error_msg: Option<String> = result.get_item(3)?.extract()?;
        let hit = last_failed.map(|n| (n, test_count, age_str));
        Ok((hit, error_msg))
    };
    read().unwrap_or((None, None))
}

/// Persist (or clear, with None) the --stepwise resume point.
pub fn cache_write_stepwise(
    py: Python<'_>,
    config: &crate::config::Config,
    nodeid: Option<&str>,
    test_count: Option<usize>,
) -> PyResult<()> {
    let cache = cache_object(py, config)?;
    match nodeid {
        Some(nodeid) => py
            .import("pytest._cache")?
            .call_method1("stepwise_write", (cache, nodeid, test_count))?,
        None => py
            .import("pytest._cache")?
            .call_method1("stepwise_write", (cache, py.None(), py.None()))?,
    };
    Ok(())
}

/// Nodeids seen in previous runs (cache/nodeids).
pub fn cache_nodeids(py: Python<'_>, config: &crate::config::Config) -> Vec<String> {
    let read = || -> PyResult<Vec<String>> {
        let cache = cache_object(py, config)?;
        let value = cache.call_method1("get", ("cache/nodeids", pyo3::types::PyList::empty(py)))?;
        value.extract()
    };
    read().unwrap_or_default()
}

/// --cache-show: print cache values/directories matching the glob.
pub fn cache_show(py: Python<'_>, config: &crate::config::Config, glob: &str) -> PyResult<()> {
    let cache = cache_object(py, config)?;
    let cachedir = cache.getattr("_cachedir")?;
    py.import("pytest._cache")?
        .call_method1("cacheshow", (cachedir, glob))?;
    Ok(())
}

/// --cache-clear: drop the cache's value/directory stores at startup.
pub fn cache_clear(py: Python<'_>, config: &crate::config::Config) -> PyResult<()> {
    cache_object(py, config)?.call_method0("clear_cache")?;
    Ok(())
}

/// Return true if the Python config proxy has `workerinput` set (simulated or
/// real xdist worker). Used to skip cache writes from workers.
pub fn config_has_workerinput(py: Python<'_>, config: &crate::config::Config) -> bool {
    let Ok(proxy) = make_py_config(py, config) else {
        return false;
    };
    proxy.bind(py).hasattr("workerinput").unwrap_or(false)
}

/// Arm the shim's MarkGenerator: unknown marks (not builtin, not in the
/// `markers` ini) warn PytestUnknownMarkWarning at the use site.
pub fn configure_mark_generator(
    py: Python<'_>,
    config: &crate::config::Config,
    strict: bool,
    strict_parametrization_ids: bool,
) -> PyResult<()> {
    let proxy = make_py_config(py, config)?;
    py.import("pytest._marks")?.call_method1(
        "configure_mark_generator",
        (
            proxy,
            crate::engine::BUILTIN_MARKS.to_vec(),
            strict,
            strict_parametrization_ids,
        ),
    )?;
    Ok(())
}

/// Apply per-item @pytest.mark.filterwarnings specs inside a
/// catch_warnings block; returns the context to close at item end.
pub fn begin_item_filters(py: Python<'_>, specs: &[String]) -> PyResult<Py<PyAny>> {
    let ctx = wcapture_begin_filters_fn(py)?.call1((specs.to_vec(),))?;
    Ok(ctx.unbind())
}

pub fn end_item_filters(py: Python<'_>, ctx: &Py<PyAny>) {
    let _ = wcapture_end_filters_fn(py).and_then(|f| f.call1((ctx.bind(py),)));
}

/// Number of warnings captured so far in this session.
/// Begin a logging capture phase for the current item (pytest's
/// catching_logs around setup/call/teardown). Best-effort.
pub fn log_start_phase(py: Python<'_>, when: &str, level: Option<&str>) {
    let _ = logging_start_phase_fn(py).and_then(|f| f.call1((when, level)));
    if let Err(err) = capture_start_phase_fn(py).and_then(|f| f.call1((when,))) {
        // The capture restored the real fds before raising; surface the
        // error like pytest does (a traceback on the real stderr).
        eprintln!("{}", format_exception(py, &err));
    }
}

/// Close the current item's logging capture (end of teardown).
pub fn log_finish_item(py: Python<'_>) {
    let _ = logging_finish_item_fn(py).and_then(|f| f.call0());
    if let Err(err) = capture_finish_item_fn(py).and_then(|f| f.call0()) {
        eprintln!("{}", format_exception(py, &err));
    }
}

/// Arm capture for a deferred module/class/session scope teardown (its
/// output reports as "Captured stdout teardown", pytest parity).
pub fn capture_scope_teardown_begin(py: Python<'_>) {
    let _ = py
        .import("pytest._capture")
        .and_then(|m| m.call_method0("begin_scope_teardown"));
}

/// Pause/resume the item capture around the runner's own mid-item terminal
/// output (live-log outcome words print between the call and teardown
/// phases, while the fd redirection is still armed).
pub fn capture_suspend(py: Python<'_>) {
    let _ = py
        .import("pytest._capture")
        .and_then(|m| m.call_method0("suspend_global"));
}

pub fn capture_resume(py: Python<'_>) {
    let _ = py
        .import("pytest._capture")
        .and_then(|m| m.call_method0("resume_global"));
}

/// Capture around one file's collection (pytest wraps
/// pytest_make_collect_report the same way).
pub fn capture_collect_begin(py: Python<'_>) {
    let _ = py
        .import("pytest._capture")
        .and_then(|m| m.call_method0("collect_begin"));
}

/// End the per-file collection capture, returning its report sections.
pub fn capture_collect_end(py: Python<'_>) -> Vec<(String, String)> {
    match py
        .import("pytest._capture")
        .and_then(|m| m.call_method0("collect_end"))
        .and_then(|s| s.extract())
    {
        Ok(sections) => sections,
        Err(err) => {
            eprintln!("{}", format_exception(py, &err));
            Vec::new()
        }
    }
}

/// Stop the session-wide global capture (pytest's stop_global_capturing);
/// errors (e.g. a broken snap monkeypatch) surface on the real stderr.
pub fn capture_session_end(py: Python<'_>) {
    if let Err(err) = py
        .import("pytest._capture")
        .and_then(|m| m.call_method0("session_end"))
    {
        eprintln!("{}", format_exception(py, &err));
    }
}

/// Recreate the global capture in a forked worker (the inherited one's
/// saved fds point at the controller's terminal, not the IPC pipe).
pub fn capture_reinit_post_fork(py: Python<'_>) {
    let _ = py
        .import("pytest._capture")
        .and_then(|m| m.call_method0("reinit_post_fork"));
}

/// Close the collection-wide logging phase (pytest's catching_logs around
/// pytest_collection).
pub fn log_end_phase(py: Python<'_>) {
    let _ = py
        .import("pytest._logging")
        .and_then(|m| m.call_method0("end_phase"));
}

/// Arm the junit XML writer (--junitxml): builds the LogXML from the
/// junit_* ini settings and stamps the session start time.
pub fn junit_configure(
    py: Python<'_>,
    config: &crate::config::Config,
    xmlpath: &str,
) -> PyResult<()> {
    let settings = pyo3::types::PyDict::new(py);
    for key in [
        "junit_suite_name",
        "junit_logging",
        "junit_duration_report",
        "junit_family",
        "junit_log_passing_tests",
    ] {
        if let Some(value) = config.get_ini(key) {
            settings.set_item(key, value)?;
        }
    }
    py.import("pytest._junitxml")?.call_method1(
        "configure",
        (xmlpath, config.get_value("junit-prefix"), settings),
    )?;
    Ok(())
}

pub fn junit_reset(py: Python<'_>) {
    let _ = py
        .import("pytest._junitxml")
        .and_then(|m| m.call_method0("reset"));
}

/// Stream every report through the junit LogXML and write the file;
/// returns its absolute path for the "generated xml file" line.
pub fn junit_write(py: Python<'_>, session: &crate::session::Session) -> PyResult<String> {
    let module = py.import("pytest._junitxml")?;
    // nodeid -> 0-based definition line for the xunit1 file/line attrs.
    let lines: std::collections::HashMap<&str, u32> = session
        .items
        .iter()
        .map(|item| (item.nodeid.as_str(), item.lineno.saturating_sub(1)))
        .collect();
    for report in &session.reports {
        let data = pyo3::types::PyDict::new(py);
        data.set_item("nodeid", &report.nodeid)?;
        data.set_item(
            "when",
            match report.phase {
                Phase::Setup => "setup",
                Phase::Call => "call",
                Phase::Teardown => "teardown",
            },
        )?;
        data.set_item(
            "outcome",
            match report.outcome {
                Outcome::Passed => "passed",
                Outcome::Failed => "failed",
                Outcome::Skipped => "skipped",
                Outcome::XFailed => "xfailed",
                Outcome::XPassed => "xpassed",
            },
        )?;
        data.set_item("duration", report.duration.as_secs_f64())?;
        if let Some(longrepr) = &report.longrepr {
            data.set_item("longrepr", longrepr)?;
        }
        if let Some(location) = &report.location {
            data.set_item("skip_location", location)?;
        }
        if !report.sections.is_empty() {
            data.set_item("sections", &report.sections)?;
        }
        if let Some(line) = lines.get(report.nodeid.as_str()) {
            data.set_item("line", line)?;
        }
        // Collection-level reports: errors tracked by the session, plus
        // module-level skips (a file nodeid skipping during setup).
        let is_collect = session
            .collect_errors
            .iter()
            .any(|(nodeid, _)| nodeid == &report.nodeid)
            || (report.phase == Phase::Setup
                && report.outcome == Outcome::Skipped
                && !report.nodeid.contains("::"));
        if is_collect {
            data.set_item("collect", true)?;
        }
        module.call_method1("log_report", (data,))?;
    }
    module.call_method0("finish")?.extract()
}

/// Arm the global output capture ("fd"/"sys" capture, "no" disables).
pub fn configure_capture(py: Python<'_>, mode: &str) {
    let _ = py
        .import("pytest._capture")
        .and_then(|m| m.call_method1("configure", (mode,)));
}

pub fn configure_debugging(py: Python<'_>) {
    if let Some(config) = super::existing_py_config(py) {
        let _ = py
            .import("pytest._debugging")
            .and_then(|m| m.call_method1("configure", (config,)));
    }
}

pub fn maybe_pdb_interact(py: Python<'_>, item: &crate::collect::TestItem, err: &pyo3::PyErr) {
    let _ = (|| -> pyo3::PyResult<()> {
        let m = py.import("pytest._debugging")?;
        let node = crate::runner::item_node(py, item)?;
        m.call_method1("maybe_interact", (node, err.value(py), ""))?;
        Ok(())
    })();
}

/// Tell the traceback formatter whether terminal color is on (E lines,
/// file:line markup, pygments source highlighting).
pub fn set_tb_color(py: Python<'_>, on: bool) {
    let _ = py
        .import("pytest._tb")
        .and_then(|m| m.call_method1("set_color", (on,)));
}

/// -l / --showlocals: render frame locals in tracebacks.
pub fn set_showlocals(py: Python<'_>, on: bool) {
    let _ = py
        .import("pytest._tb")
        .and_then(|m| m.call_method1("set_showlocals", (on,)));
}

/// --full-trace: render every frame (no __tracebackhide__ cutting) in long style.
pub fn set_fulltrace(py: Python<'_>, on: bool) {
    let _ = py
        .import("pytest._tb")
        .and_then(|m| m.call_method1("set_fulltrace", (on,)));
}

/// Toggle the cyclic garbage collector. Collection imports thousands of test
/// modules and their app dependencies, and those allocations trigger gc runs
/// that scan the ever-growing set of just-imported objects for cycles —
/// wasted work, since none of it is freed mid-collection. We disable gc for
/// the collection phase and re-enable it before any test runs (tests and the
/// unraisable/threadexception plugins rely on gc to surface finalizers).
pub fn set_gc_enabled(py: Python<'_>, enabled: bool) {
    let method = if enabled { "enable" } else { "disable" };
    let _ = py.import("gc").and_then(|m| m.call_method0(method));
}

/// PYTEST_THEME / PYTEST_THEME_MODE validation (color mode only): the
/// pytest startup error message, or None.
pub fn invalid_theme_message(py: Python<'_>) -> Option<String> {
    py.import("pytest._tb")
        .and_then(|m| m.call_method0("validate_theme"))
        .ok()
        .and_then(|value| value.extract::<Option<String>>().ok())
        .flatten()
}

/// Tell the tmp_path factory the explicit --basetemp directory (cleared at
/// session start, kept after the run, like pytest) and the retention inis.
pub fn configure_tmp_path(
    py: Python<'_>,
    basetemp: Option<&str>,
    retention_count: Option<&str>,
    retention_policy: Option<&str>,
) {
    let _ = py
        .import("pytest._tmp_path")
        .and_then(|m| m.call_method1("configure", (basetemp, retention_count, retention_policy)));
}

/// Report an item's call outcome (None: no call phase ran) to the tmp_path
/// retention machinery, before function-scope finalizers run.
pub fn tmp_path_record_call(py: Python<'_>, nodeid: &str, passed: Option<bool>) {
    let _ = py
        .import("pytest._tmp_path")
        .and_then(|m| m.call_method1("record_call", (nodeid, passed)));
}

/// Install the sys.unraisablehook capture (upstream unraisableexception
/// plugin's pytest_configure).
pub fn unraisable_configure(py: Python<'_>) {
    let _ = py
        .import("pytest._unraisable")
        .and_then(|m| m.call_method0("configure"));
}

/// Drain unraisable exceptions collected since the last phase. Err when the
/// warning filter turns them into errors (-W error).
pub fn unraisable_collect(py: Python<'_>) -> PyResult<()> {
    py.import("pytest._unraisable")?
        .call_method0("collect_unraisable")?;
    Ok(())
}

/// Session-end unraisable cleanup: force gc, drain leftovers, restore the
/// previous hook (upstream's config cleanup).
pub fn unraisable_session_cleanup(py: Python<'_>) -> PyResult<()> {
    py.import("pytest._unraisable")?
        .call_method0("session_cleanup")?;
    Ok(())
}

/// The config-file ini keys that are neither a registered (plugin/conftest
/// addini) option nor a core one — pytest's unknown-config-option set, sorted.
pub fn unknown_ini_keys(py: Python<'_>, keys: &[String]) -> PyResult<Vec<String>> {
    py.import("pytest._parser")?
        .call_method1("unknown_ini_keys", (keys.to_vec(),))?
        .extract()
}

/// Install the threading.excepthook capture (upstream threadexception
/// plugin's pytest_configure).
pub fn threadexception_configure(py: Python<'_>) {
    let _ = py
        .import("pytest._threadexception")
        .and_then(|m| m.call_method0("configure"));
}

/// Drain unhandled thread exceptions collected since the last phase. Err when
/// the warning filter turns them into errors (-W error).
pub fn threadexception_collect(py: Python<'_>) -> PyResult<()> {
    py.import("pytest._threadexception")?
        .call_method0("collect_thread_exception")?;
    Ok(())
}

/// Session-end thread-exception cleanup: drain leftovers, restore the
/// previous hook (upstream's config cleanup).
pub fn threadexception_session_cleanup(py: Python<'_>) -> PyResult<()> {
    py.import("pytest._threadexception")?
        .call_method0("session_cleanup")?;
    Ok(())
}

/// Tell the assert-rewrite explainer the -v level (full iterable diffs
/// need -v, identical dict items unfold at -vv, like pytest).
pub fn set_assertion_verbosity(py: Python<'_>, global_level: u8, assertion_level: i32) {
    let _ = py
        .import("pytest._rewrite")
        .and_then(|m| m.call_method1("set_verbosity", (global_level, assertion_level)));
}

/// --assert=plain disables assertion rewriting entirely (failed asserts
/// surface as a bare AssertionError, like pytest). Any other value (or the
/// default "rewrite") keeps the rewriter installed.
pub fn set_assertion_rewrite(py: Python<'_>, mode: Option<&str>) {
    let enabled = mode != Some("plain");
    let _ = py
        .import("pytest._rewrite")
        .and_then(|m| m.call_method1("set_enabled", (enabled,)));
}

/// Pass the truncation_limit_lines / truncation_limit_chars ini values to
/// the assert-rewrite explainer (None keeps pytest's defaults: 8 lines,
/// 640 chars).
pub fn set_assertion_truncation(py: Python<'_>, lines: Option<&str>, chars: Option<&str>) {
    let parse = |value: Option<&str>| value.and_then(|s| s.trim().parse::<i64>().ok());
    let _ = py
        .import("pytest._rewrite")
        .and_then(|m| m.call_method1("set_truncation_limits", (parse(lines), parse(chars))));
}

/// Pass the python_files ini patterns to the assertion rewriter so that
/// non-standard test-file globs (e.g. "testing/python/*.py") are also
/// assertion-rewritten.  The default patterns (test_*.py / *_test.py) are
/// already handled by `_is_rewrite_target`; only non-default extras matter.
pub fn set_python_files_globs(py: Python<'_>, patterns: &[String]) {
    let default_patterns = ["test_*.py", "*_test.py"];
    let extra: Vec<&str> = patterns
        .iter()
        .map(|s| s.as_str())
        .filter(|p| !default_patterns.contains(p))
        .collect();
    if extra.is_empty() {
        return;
    }
    let _ = py
        .import("pytest._rewrite")
        .and_then(|m| m.call_method1("register_python_files_globs", (extra,)));
}

/// Upstream xdist's default auto/logical worker-count detection: the
/// PYTEST_XDIST_AUTO_NUM_WORKERS env override, psutil if installed (the
/// pytest-xdist[psutil] extra), then sched_getaffinity/cpu_count.
pub fn xdist_auto_num_workers(py: Python<'_>, logical: bool) -> usize {
    py.import("pytest._xdist_fixtures")
        .and_then(|m| m.call_method1("auto_num_workers", (logical,)))
        .and_then(|n| n.extract())
        .unwrap_or_else(|_| {
            std::thread::available_parallelism()
                .map(std::num::NonZero::get)
                .unwrap_or(1)
        })
}

/// Set the worker process title (the pytest-xdist[setproctitle] extra);
/// best-effort no-op when setproctitle is not installed. Availability is
/// probed once per process: the per-item call must stay free when the
/// extra is absent.
pub fn worker_set_title(py: Python<'_>, title: &str) {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let available = *AVAILABLE.get_or_init(|| py.import("setproctitle").is_ok());
    if !available {
        return;
    }
    let _ = py
        .import("pytest._xdist_fixtures")
        .and_then(|m| m.call_method1("set_worker_title", (title,)));
}

/// "Captured stdout/stderr {when}" then "Captured log {when}" report
/// sections accumulated for the running item.
pub fn log_failure_sections(py: Python<'_>) -> Vec<(String, String)> {
    let mut sections: Vec<(String, String)> = py
        .import("pytest._capture")
        .and_then(|m| m.call_method0("failure_sections"))
        .and_then(|s| s.extract())
        .unwrap_or_default();
    sections.extend(
        py.import("pytest._logging")
            .and_then(|m| m.call_method0("failure_sections"))
            .and_then(|s| s.extract::<Vec<(String, String)>>())
            .unwrap_or_default(),
    );
    sections
}

/// The verbose per-test reason suffix for a skip/xfail/xpass line: " (reason)"
/// truncated to fit (verbosity < 2) or wrapped across lines (>= 2). Empty when
/// there is no reason or it cannot fit.
pub fn format_verbose_reason(
    py: Python<'_>,
    prefix_width: usize,
    reason: &str,
    verbosity: i32,
    fullwidth: usize,
) -> String {
    py.import("_pytest.terminal")
        .and_then(|m| {
            m.call_method1(
                "format_verbose_reason",
                (prefix_width, reason, verbosity, fullwidth),
            )
        })
        .and_then(|s| s.extract())
        .unwrap_or_default()
}

pub fn warning_count(py: Python<'_>) -> usize {
    py.import("pytest._wcapture")
        .and_then(|m| m.call_method0("count"))
        .and_then(|count| count.extract())
        .unwrap_or(0)
}

/// Formatted lines for the warnings summary section, skipping the first
/// `start` warnings (already shown — for the "(final)" summary).
pub fn warning_summary_lines(py: Python<'_>, start: usize) -> Vec<String> {
    py.import("pytest._wcapture")
        .and_then(|m| m.call_method1("summary_lines", (start,)))
        .and_then(|lines| lines.extract())
        .unwrap_or_default()
}

/// Emit a warning of a pytest category attributed to an explicit
/// file/line (registry=None: never deduplicated).
pub fn warn_explicit_at(
    py: Python<'_>,
    category: &str,
    message: &str,
    filename: &str,
    lineno: u32,
) -> PyResult<()> {
    let category = py.import("pytest")?.getattr(category)?;
    py.import("warnings")?
        .call_method1("warn_explicit", (message, category, filename, lineno))?;
    Ok(())
}

/// A controller-side WorkerNode for the xdist data-exchange hooks
/// (pytest_configure_node / pytest_testnodedown). workerinput starts with
/// the base keys upstream xdist always provides.
pub fn make_worker_node(
    py: Python<'_>,
    index: usize,
    worker_count: usize,
    testrun_uid: &str,
    config: &crate::config::Config,
) -> PyResult<Py<PyAny>> {
    let workerinput = pyo3::types::PyDict::new(py);
    workerinput.set_item("workerid", format!("gw{index}"))?;
    workerinput.set_item("workercount", worker_count)?;
    workerinput.set_item("testrunuid", testrun_uid)?;
    let config_proxy = make_py_config(py, config)?;
    Ok(py
        .import("pytest._dist")?
        .getattr("WorkerNode")?
        .call1((format!("gw{index}"), config_proxy, workerinput))?
        .unbind())
}

/// node.workerinput as JSON for the worker process (None when nothing
/// beyond the base keys was added, or when a value isn't serializable).
pub fn worker_node_input_json(py: Python<'_>, node: &Py<PyAny>) -> Option<String> {
    let input = node.bind(py).getattr("workerinput").ok()?;
    py.import("json")
        .and_then(|json| json.call_method1("dumps", (input,)))
        .and_then(|s| s.extract())
        .ok()
}

/// This worker's config.workeroutput as JSON (None when empty).
pub fn worker_output_json(py: Python<'_>) -> Option<String> {
    let output = py
        .import("pytest._dist")
        .and_then(|m| m.getattr("workeroutput"))
        .ok()?;
    if output.len().ok()? == 0 {
        return None;
    }
    py.import("json")
        .and_then(|json| json.call_method1("dumps", (output,)))
        .and_then(|s| s.extract())
        .ok()
}

/// Merge a worker's streamed workeroutput JSON into its node.
pub fn worker_node_set_output(py: Python<'_>, node: &Py<PyAny>, payload: &str) {
    let _ = py
        .import("json")
        .and_then(|json| json.call_method1("loads", (payload,)))
        .and_then(|data| {
            node.bind(py)
                .getattr("workeroutput")?
                .call_method1("update", (data,))
        });
}

/// Hook impls registered on plugin instances via pluginmanager.register
/// (e.g. pytest-run-parallel's runner object); module-level impls live in
/// session.py_hooks instead.
pub fn instance_hook_funcs(py: Python<'_>, name: &str) -> Vec<Py<PyAny>> {
    py.import("pytest._pluginmanager")
        .and_then(|m| m.call_method1("instance_hook_impls", (name,)))
        .and_then(|impls| impls.extract())
        .unwrap_or_default()
}
