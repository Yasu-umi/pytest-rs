use std::time::Instant;

use pyo3::prelude::*;

use super::super::Engine;
use crate::config::Config;
use crate::hooks::Plugin;
use crate::python;
use crate::report::exit_code;
use crate::session::Session;

impl Engine {
    pub fn new(plugins: Vec<Box<dyn Plugin>>, config: Config) -> Self {
        Self {
            plugins,
            session: Session::new(),
            config,
            cache: None,
        }
    }

    /// Run the whole test session; returns the process exit code.
    pub fn run(&mut self, py: Python<'_>) -> i32 {
        let started = Instant::now();
        // Startup + collection (shim/conftest/plugin/module imports) is
        // allocation-heavy; gc cycle scans during it just rescan the growing
        // set of just-imported objects for nothing. Disable for the whole
        // import phase, re-enabled before any test runs (after collect, and
        // before the worker loop). Every path in between either runs no user
        // tests or exits the process, so leaving it off until then is safe.
        python::set_gc_enabled(py, false);
        if let Err(err) = python::activate_virtualenv(py) {
            eprintln!("INTERNAL ERROR: failed to activate virtualenv: {err}");
            return exit_code::INTERNAL_ERROR;
        }
        if let Err(err) = python::install_shim(py) {
            eprintln!("INTERNAL ERROR: failed to install pytest shim: {err}");
            return exit_code::INTERNAL_ERROR;
        }
        // pytest sets PYTEST_VERSION to its own __version__ at startup so
        // tests can check os.environ["PYTEST_VERSION"] == pytest.__version__.
        // Query after shim install so the shim's pytest is the one we read.
        if let Ok(version) = python::pytest_version(py) {
            python::setenv(py, "PYTEST_VERSION", &version);
        }
        // pythonpath ini: add paths relative to rootdir to sys.path early,
        // before conftest/plugin imports (mirrors _pytest/python_path.py).
        for rel in self.config.get_ini_lines("pythonpath") {
            let abs = self.config.rootdir.join(rel);
            if let Err(err) = python::sys_path_prepend(py, &abs) {
                eprintln!("INTERNAL ERROR: failed to add pythonpath entry {rel}: {err}");
            }
        }
        let _debug_guard = super::install_debug_guard(py, &self.config);
        let ini_filters: Vec<String> = self
            .config
            .get_ini_lines("filterwarnings")
            .into_iter()
            .map(str::to_string)
            .collect();
        if !self.config.plugin_disabled("warnings")
            && let Err(err) =
                python::install_warning_capture(py, &ini_filters, &self.config.w_options)
        {
            eprintln!("ERROR: {}", err.value(py));
            return exit_code::USAGE_ERROR;
        }
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
        // Session-wide logging handlers: log_file writes, log_cli interleaves
        // live records with the progress output.
        self.session.live_logging = python::configure_logging(py, &self.config);

        // Color decision: --color beats PY_COLORS / NO_COLOR / FORCE_COLOR,
        // which beat isatty. Validated like pytest's choices.
        let color_option = self.config.get_value("color");
        if let Some(choice) = color_option
            && !matches!(choice, "yes" | "no" | "auto")
        {
            eprintln!(
                "error: argument --color: invalid choice: '{choice}' (choose from 'yes', 'no', 'auto')"
            );
            return exit_code::USAGE_ERROR;
        }
        crate::tw::set_enabled(crate::tw::should_colorize(color_option));
        python::set_tb_color(py, crate::tw::enabled());
        // -l / --showlocals, unless a later --no-showlocals overrides it.
        python::set_showlocals(
            py,
            self.config.get_flag("showlocals") && !self.config.get_flag("no-showlocals"),
        );
        python::set_fulltrace(py, self.config.get_flag("full-trace"));
        if let Some(message) = python::invalid_theme_message(py) {
            eprintln!("ERROR: {message}");
            return exit_code::USAGE_ERROR;
        }

        // Global output capture: -s / --capture=no disable, default "fd"
        // (dup2-based, so os.write and C-level output are captured too).
        let capture_mode = if self.config.get_flag("capture-disable") {
            "no"
        } else {
            self.config.get_value("capture").unwrap_or("fd")
        };
        if !matches!(capture_mode, "fd" | "sys" | "no" | "tee-sys") {
            eprintln!(
                "error: argument --capture: invalid choice: '{capture_mode}' (choose from 'fd', 'sys', 'no', 'tee-sys')"
            );
            return exit_code::USAGE_ERROR;
        }
        python::configure_capture(py, capture_mode);
        // --basetemp must not be the cwd or an ancestor of the cwd (pytest parity).
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
        python::configure_tmp_path(
            py,
            self.config.get_value("basetemp"),
            self.config.get_ini("tmp_path_retention_count"),
            self.config.get_ini("tmp_path_retention_policy"),
        );
        if !self.config.plugin_disabled("unraisableexception") {
            python::unraisable_configure(py);
        }
        if !self.config.plugin_disabled("threadexception") {
            python::threadexception_configure(py);
        }
        python::set_assertion_verbosity(
            py,
            self.config.verbose,
            self.config.verbosity_for("verbosity_assertions"),
        );
        python::set_assertion_rewrite(py, self.config.get_value("assert"));
        python::set_assertion_truncation(
            py,
            self.config.get_ini("truncation_limit_lines"),
            self.config.get_ini("truncation_limit_chars"),
        );
        python::set_python_files_globs(py, &self.config.python_files_patterns());

        // --junitxml: arm the XML writer (workers never write; the parent
        // streams every report through it at session end).
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

        // Arm unknown-mark validation (PytestUnknownMarkWarning on access).
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
            // --runxfail also neutralizes imperative pytest.xfail (pytest's
            // skipping plugin monkeypatches it the same way).
            let _ = py.run(
                c"import pytest\npytest.xfail = lambda reason='': None\n",
                None,
                None,
            );
        }

        python::configure_debugging(py);

        self.run_session(py, started)
    }
}
