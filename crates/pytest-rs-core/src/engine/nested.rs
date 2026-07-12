use std::time::Instant;

use pyo3::prelude::*;

use super::super::Engine;
use crate::python;
use crate::report::exit_code;

use super::inprocess;

impl Engine {
    pub(crate) fn run_nested(&mut self, py: Python<'_>) -> i32 {
        let started = Instant::now();
        // While this guard lives, hook dispatch notifies the plugin manager's
        // call monitors (HookRecorder) with live kwargs so getcalls works.
        let _recording = inprocess::RecordingGuard::enter();
        // Shadow the process-global config proxy with one built from the nested
        // config, so getini/getoption read the nested run's tox.ini/options
        // instead of the cached outer singleton. Dropped when the run ends.
        let _config_proxy = python::push_nested_config_proxy(py, &self.config).ok();
        // Snapshot the pluginmanager so conftest registrations from this nested
        // run don't leak into subsequent runs (guard restores on drop).
        let _pm_guard = python::snapshot_pluginmanager(py).ok();
        // --debug: same trace-file behavior as the top-level run() (a nested
        // pytester.runpytest("--debug") must also write/announce the file).
        let _debug_guard = super::install_debug_guard(py, &self.config);
        // The nested config may declare its own pythonpath ini entries.
        for rel in self.config.get_ini_lines("pythonpath") {
            let abs = self.config.rootdir.join(rel);
            let _ = python::sys_path_prepend(py, &abs);
        }
        // Capture: the caller pushed a fresh CaptureState; arm it for this run.
        let capture_mode = if self.config.get_flag("capture-disable") {
            "no"
        } else {
            self.config.get_value("capture").unwrap_or("fd")
        };
        python::configure_capture(py, capture_mode);
        // Validate --log-file-mode (same check as run()).
        if let Some(mode) = self
            .config
            .get_value("log-file-mode")
            .or_else(|| self.config.get_ini("log_file_mode"))
            && !matches!(mode, "w" | "a")
        {
            eprintln!(
                "error: argument --log-file-mode: invalid choice: '{mode}' (choose from 'w', 'a')"
            );
            return exit_code::USAGE_ERROR;
        }
        // Logging: reconfigure session handlers (log_file / log_cli) for the
        // nested config. The caller snapshots/restores the logging state.
        self.session.live_logging = python::configure_logging(py, &self.config);
        // --basetemp must not be the cwd or an ancestor (same check as run()).
        if let Some(bt) = self.config.get_value("basetemp") {
            let bt_path = std::path::Path::new(bt);
            let resolved = if bt_path.is_absolute() {
                bt_path.to_path_buf()
            } else {
                self.config.invocation_dir.join(bt_path)
            };
            let resolved = std::fs::canonicalize(&resolved).unwrap_or(resolved);
            let cwd = std::fs::canonicalize(&self.config.invocation_dir)
                .unwrap_or_else(|_| self.config.invocation_dir.clone());
            if resolved == cwd || cwd.starts_with(&resolved) {
                eprintln!(
                    "ERROR: basetemp must not be the current directory or an ancestor \
                     directory. Use a relative path: {bt}"
                );
                return exit_code::USAGE_ERROR;
            }
        }
        // Reconfigure the tmp_path machinery for this nested config (its own
        // --basetemp / retention) and reset retention bookkeeping, so the
        // nested run does not inherit the outer run's basetemp or pass/fail
        // outcomes (tmp_path_retention_policy tests).
        python::configure_tmp_path(
            py,
            self.config.get_value("basetemp"),
            self.config.get_ini("tmp_path_retention_count"),
            self.config.get_ini("tmp_path_retention_policy"),
        );
        // Tests (and gc-dependent plugins) run, so gc must be on; the outer
        // run leaves it enabled post-collection, but be explicit.
        python::set_gc_enabled(py, true);
        // Re-set PYTEST_VERSION so monkeypatch overrides in the outer test don't
        // bleed into the inner run (the outer run sets it once in Engine::run).
        if let Ok(version) = python::pytest_version(py) {
            python::setenv(py, "PYTEST_VERSION", &version);
        }
        // --junitxml: arm the XML writer for this nested run (same as run()).
        if let Some(path) = self.config.get_value("junit-xml").map(str::to_string)
            && !self.config.is_worker()
        {
            if std::path::Path::new(&path).is_dir() {
                eprintln!("ERROR: --junitxml must be a filename, given: {path}");
                return exit_code::USAGE_ERROR;
            }
            if let Err(err) = python::junit_configure(py, &self.config, &path) {
                eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
                return exit_code::INTERNAL_ERROR;
            }
        }
        if !self.config.plugin_disabled("debugging") {
            python::configure_debugging(py);
        }
        python::set_assertion_verbosity(
            py,
            self.config.verbose,
            self.config.verbosity_for("verbosity_assertions"),
        );
        python::set_assertion_truncation(
            py,
            self.config.get_ini("truncation_limit_lines"),
            self.config.get_ini("truncation_limit_chars"),
        );
        if let Err(err) = python::configure_mark_generator(
            py,
            &self.config,
            self.strict_markers(),
            self.strict_parametrization_ids(),
        ) {
            eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
            return exit_code::INTERNAL_ERROR;
        }
        if self.config.get_flag("runxfail") {
            let _ = py.run(
                c"import pytest\npytest.xfail = lambda reason='': None\n",
                None,
                None,
            );
        }
        // Warning capture: install with the nested config's filterwarnings/W
        // options. The caller saves/restores the outer warning state; this
        // arms the capture for the inner session's own filter specs.
        let ini_filters: Vec<String> = self
            .config
            .get_ini_lines("filterwarnings")
            .into_iter()
            .map(str::to_string)
            .collect();
        if !self.config.plugin_disabled("warnings") {
            let _ = python::install_warning_capture(py, &ini_filters, &self.config.w_options);
        }
        let outer_color = crate::tw::enabled();
        let nested_color = crate::tw::should_colorize(self.config.get_value("color"));
        crate::tw::set_enabled(nested_color);
        python::set_tb_color(py, nested_color);
        python::set_showlocals(
            py,
            self.config.get_flag("showlocals") && !self.config.get_flag("no-showlocals"),
        );
        python::set_fulltrace(py, self.config.get_flag("full-trace"));
        python::set_truncate_args(py, self.config.global_verbosity() <= 2);
        let result = self.run_session(py, started);
        // Reset the junit state so the next nested run (or the outer run)
        // doesn't see a stale LogXML instance from this run.
        python::junit_reset(py);
        crate::tw::set_enabled(outer_color);
        python::set_tb_color(py, outer_color);
        result
    }
}
