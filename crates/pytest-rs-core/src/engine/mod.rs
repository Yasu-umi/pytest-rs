use std::path::PathBuf;
use std::time::Instant;

use pyo3::prelude::*;

use crate::config::Config;
use crate::hooks::{HookContext, Plugin};
use crate::python;
use crate::report::{Outcome, Phase, exit_code};
use crate::session::Session;

/// Marks owned by the core or bundled plugins.
pub(crate) const BUILTIN_MARKS: [&str; 13] = [
    "skip",
    "skipif",
    "xfail",
    "parametrize",
    "usefixtures",
    "filterwarnings",
    "tryfirst",
    "trylast",
    "asyncio",
    "anyio",
    "benchmark",
    "no_cover",
    "xdist_group",
];

pub struct Engine {
    pub plugins: Vec<Box<dyn Plugin>>,
    pub session: Session,
    pub config: Config,
    /// cacheprovider state (--lf/--ff/--nf, lastfailed persistence).
    cache: Option<crate::cache::CacheState>,
}

mod hooks;
pub mod inprocess;
mod selection;
mod terminal;

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
        // pythonpath ini: add paths relative to rootdir to sys.path early,
        // before conftest/plugin imports (mirrors _pytest/python_path.py).
        for rel in self.config.get_ini_lines("pythonpath") {
            let abs = self.config.rootdir.join(rel);
            if let Err(err) = python::sys_path_prepend(py, &abs) {
                eprintln!("INTERNAL ERROR: failed to add pythonpath entry {rel}: {err}");
            }
        }
        // --debug: pytest's debug trace file (minimal: create the file and
        // announce it on stderr like upstream). The "wrote" message fires on
        // drop so every exit path (early returns, NO_TESTS_COLLECTED, etc.)
        // emits it.
        struct DebugGuard(Option<std::path::PathBuf>);
        impl Drop for DebugGuard {
            fn drop(&mut self) {
                if let Some(path) = &self.0 {
                    eprintln!("wrote pytest debug information to {}", path.display());
                }
            }
        }
        let _debug_guard = if let Some(name) = self.config.get_value("debug") {
            let path = self.config.invocation_dir.join(name);
            let _ = std::fs::write(
                &path,
                format!(
                    "versions pytest-rs-{}, python-{}\n",
                    env!("CARGO_PKG_VERSION"),
                    py.version().split_whitespace().next().unwrap_or("")
                ),
            );
            eprintln!("writing pytest debug information to {}", path.display());
            DebugGuard(Some(path))
        } else {
            DebugGuard(None)
        };
        if let Some(report) = self.config.get_value("doctest-report") {
            const CHOICES: &[&str] = &["none", "cdiff", "udiff", "ndiff", "only_first_failure"];
            if !CHOICES.iter().any(|c| c.eq_ignore_ascii_case(report)) {
                eprintln!(
                    "error: argument --doctest-report: invalid choice: '{report}' (choose from {})",
                    CHOICES.join(", ")
                );
                return exit_code::USAGE_ERROR;
            }
        }
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
        python::set_assertion_verbosity(py, self.config.verbose);
        python::set_assertion_rewrite(py, self.config.get_value("assert"));
        python::set_assertion_truncation(
            py,
            self.config.get_ini("truncation_limit_lines"),
            self.config.get_ini("truncation_limit_chars"),
        );

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

        self.run_session(py, started)
    }

    /// Collection + run-loop + reporting core. Shared by the outer process
    /// run (above) and in-process nested sub-sessions. Assumes the global
    /// setup layer (capture, logging, gc, shim, junit, ...) has already run.
    fn run_session(&mut self, py: Python<'_>, started: Instant) -> i32 {
        if let Err(err) = self
            .fire_configure(py)
            .and_then(|()| self.fire_sessionstart(py))
        {
            if python::is_usage_error(py, &err) {
                eprintln!("ERROR: {}", err.value(py));
                return exit_code::USAGE_ERROR;
            }
            eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
            return exit_code::INTERNAL_ERROR;
        }

        // -n worker mode: this process is driven over stdin/stdout; it
        // never collects or reports on its own.
        #[cfg(feature = "xdist")]
        if self.config.is_worker() {
            // Workers collect and run inside run_worker; restore gc so their
            // test execution is unaffected by the bootstrap-phase disable.
            python::set_gc_enabled(py, true);
            return self.run_worker(py);
        }

        // The session header prints from collect() once plugins are loaded
        // (a reporter-replacing plugin owns it in delegated mode).
        self.cache = Some(crate::cache::CacheState::new(py, &self.config));

        // --cache-show: display cache contents instead of running tests.
        if let Some(glob) = self.config.get_value("cache-show").map(str::to_string) {
            self.print_header(py);
            let glob = if glob.is_empty() { "*" } else { &glob };
            return match python::cache_show(py, &self.config, glob) {
                Ok(()) => exit_code::OK,
                Err(err) => {
                    eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
                    exit_code::INTERNAL_ERROR
                }
            };
        }

        if self.session.live_logging {
            python::log_set_live_when(py, "collection");
        }
        let collect_errors = match self.collect(py) {
            Ok(errors) => errors,
            Err(message) => {
                // Sentinel "\x00INTERNAL\x00" means an unexpected hook exception
                // (e.g. conftest pytest_sessionstart raised) — print as INTERNALERROR.
                if let Some(inner) = message.strip_prefix("\x00INTERNAL\x00") {
                    for line in inner.lines() {
                        println!("INTERNALERROR> {line}");
                    }
                    return exit_code::INTERNAL_ERROR;
                }
                // Sentinel "\x00KEYBOARD_INTERRUPT\x00": KeyboardInterrupt during
                // collection — print the special "!!! KeyboardInterrupt !!!" banner.
                if message == "\x00KEYBOARD_INTERRUPT\x00" {
                    println!("!!! KeyboardInterrupt !!!");
                    return exit_code::INTERRUPTED;
                }
                // Sentinel "\x00USAGE_ERROR\x00": UsageError in configure —
                // message already printed; run unconfigure and exit 4.
                if message == "\x00USAGE_ERROR\x00" {
                    let _ = self.fire_py_hooks_simple(py, "pytest_unconfigure");
                    return exit_code::USAGE_ERROR;
                }
                eprintln!("ERROR: {message}");
                return exit_code::USAGE_ERROR;
            }
        };
        // Collection done: tests (and gc-dependent plugins) run from here on.
        python::set_gc_enabled(py, true);
        let n_collect_errors = collect_errors.len();
        if n_collect_errors > 0 {
            // Collection errors still report as errors in the summary.
            for (path, err) in collect_errors {
                let nodeid = crate::collect::file_nodeid(&self.config.rootdir, &path);
                self.session
                    .collect_errors
                    .push((nodeid.clone(), err.clone()));
                // Delegated mode: replacement reporter sees a failed CollectReport
                // (sugar prints them instantly). Native mode: instance plugins
                // (e.g., relay plugin) still need to observe collect errors.
                python::reporter_collect_error(py, &nodeid, &err);
                self.session.reports.push(crate::report::TestReport {
                    nodeid,
                    phase: Phase::Setup,
                    outcome: Outcome::Failed,
                    duration: std::time::Duration::ZERO,
                    longrepr: Some(err),
                    location: None,
                    subtest_desc: None,
                    sections: Vec::new(),
                    rerun: false,
                    xfail_longrepr: None,
                    reprcrash_message: None,
                    head_line: None,
                });
            }
            // --maxfail aborting collection exits TESTS_FAILED with a
            // "stopping after N failures" banner; otherwise INTERRUPTED.
            let maxfail_hit = self.config.maxfail().is_some_and(|m| n_collect_errors >= m);
            // --collect-only still lists the items it did collect plus an
            // error count (pytest's "3 tests collected, 1 error"), so it falls
            // through to the collect-only branch like continue-on-errors.
            if (!self.config.get_flag("continue-on-collection-errors") && !self.config.collect_only)
                || maxfail_hit
            {
                // Under -n, xdist reports collection errors as plain errors
                // (exit 1, no Interrupted banner) below the worker banner.
                #[cfg(feature = "xdist")]
                let dist_workers = if maxfail_hit {
                    None
                } else {
                    self.resolve_numprocesses(py)
                };
                #[cfg(not(feature = "xdist"))]
                let dist_workers: Option<usize> = None;
                // --no-summary suppresses the ERRORS section and short summary,
                // like pytest's terminal-summary block (the count line and the
                // Interrupted banner still show).
                let no_summary = self.config.get_flag("no-summary");
                if !self.config.no_terminal() {
                    #[cfg(feature = "xdist")]
                    if let Some(workers) = dist_workers {
                        self.print_dist_banner(workers);
                    }
                    if dist_workers.is_none() && !self.config.quiet {
                        // pytest still applies -k/-m selection before aborting,
                        // so the count line shows deselected/selected too.
                        let _ = self.apply_selection(py);
                        let deselected = self.session.deselected_items.len();
                        let n_items = self.session.items.len();
                        let collected = n_items + deselected;
                        let mut line = format!(
                            "collected {collected} item{} / {n_collect_errors} error{}",
                            if collected == 1 { "" } else { "s" },
                            if n_collect_errors == 1 { "" } else { "s" }
                        );
                        if deselected > 0 {
                            line += &format!(" / {deselected} deselected / {n_items} selected");
                        }
                        println!("{line}");
                    }
                    if !no_summary {
                        self.print_collect_errors();
                    }
                }
                // A file that fails collection is a "last failed" entry.
                if let Some(cache) = &self.cache {
                    cache.sessionfinish(
                        py,
                        &self.config,
                        &self.session.reports,
                        &self.session.items,
                    );
                }
                self.write_junit_xml(py);
                if !self.config.no_terminal() {
                    if !no_summary {
                        self.print_short_summary();
                    }
                    if dist_workers.is_none() {
                        let banner = if maxfail_hit {
                            format!("stopping after {n_collect_errors} failures")
                        } else {
                            format!(
                                "Interrupted: {n_collect_errors} error{} during collection",
                                if n_collect_errors == 1 { "" } else { "s" }
                            )
                        };
                        println!("{}", center_with(&banner, '!'));
                    }
                    let summary = crate::runner::summary_line(
                        &self.session.reports,
                        self.session.deselected,
                        python::warning_count(py),
                        started.elapsed(),
                        self.config.global_verbosity(),
                    );
                    if !summary.is_empty() {
                        println!("{summary}");
                    }
                }
                let code = if maxfail_hit || dist_workers.is_some() {
                    exit_code::TESTS_FAILED
                } else {
                    exit_code::INTERRUPTED
                };
                if self.session.custom_reporter.is_some() && !self.config.is_worker() {
                    // pytest fires sessionfinish even on aborted collection
                    // (pretty's wall-clock end time comes from it).
                    if let Err(err) = self.fire_py_sessionfinish(py, code) {
                        eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
                    }
                    let banner = if maxfail_hit {
                        Some(format!("stopping after {n_collect_errors} failures"))
                    } else if dist_workers.is_none() {
                        Some(format!(
                            "Interrupted: {n_collect_errors} error{} during collection",
                            if n_collect_errors == 1 { "" } else { "s" }
                        ))
                    } else {
                        None
                    };
                    python::reporter_finish(py, &self.config, code, banner.as_deref());
                }
                return code;
            }
        }

        if let Err(message) = self.check_strict_markers(py) {
            println!("{message}");
            return exit_code::USAGE_ERROR;
        }

        let collected = self.session.items.len();
        if let Err(err) = self.apply_deselect() {
            eprintln!("INTERNAL ERROR: {err}");
            return exit_code::INTERNAL_ERROR;
        }
        if let Err(err) = self.fire_collection_modifyitems(py) {
            if python::is_usage_error(py, &err) {
                eprintln!("ERROR: {}", err.value(py));
                return exit_code::USAGE_ERROR;
            }
            // Upstream pytest_internalerror: the traceback goes to the
            // terminal (stdout), each line prefixed "INTERNALERROR> ".
            for line in python::format_exception(py, &err).lines() {
                println!("INTERNALERROR> {line}");
            }
            return exit_code::INTERNAL_ERROR;
        }
        if let Some(cache) = &mut self.cache {
            cache.modify_items(
                &self.config,
                &mut self.session.items,
                &mut self.session.deselected_items,
            );
        }
        if let Err(err) = self.fire_py_deselected(py) {
            eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
            return exit_code::INTERNAL_ERROR;
        }

        let n_items = self.session.items.len();
        // Plugins may also expand items (e.g. loop-factory
        // parametrization), so saturate against growth.
        self.session.deselected = collected.saturating_sub(n_items);
        // Collection settled: pytest_collection_finish python hooks see the
        // final item set (sugar's progress total comes from here).
        if let Err(err) = self.fire_py_collection_finish(py) {
            eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
        }
        // The replacement reporter prints its own "collected N items" line.
        if self.session.custom_reporter.is_some() {
            python::reporter_collection_finish(py, &self.config, collected);
        }
        if !self.config.quiet && !self.config.no_terminal() {
            let deselected = self.session.deselected;
            // -v shows the live "collecting ..." prefix resolved in place.
            let prefix = if self.config.verbose > 0 {
                "collecting ... "
            } else {
                ""
            };
            // pytest's report_collect builds the line incrementally so error,
            // deselected and selected counts can all appear together.
            let mut line = format!(
                "{prefix}collected {collected} item{}",
                if collected == 1 { "" } else { "s" }
            );
            if n_collect_errors > 0 {
                line += &format!(
                    " / {n_collect_errors} error{}",
                    if n_collect_errors == 1 { "" } else { "s" }
                );
            }
            if deselected > 0 {
                line += &format!(" / {deselected} deselected");
                line += &format!(" / {n_items} selected");
            }
            println!("{line}");
            if let Some(line) = self
                .cache
                .as_ref()
                .and_then(|cache| cache.status_line(&self.config))
            {
                println!("{line}");
            }
            if let Some(cache) = self.cache.as_ref() {
                for line in cache.stepwise_lines(&self.config) {
                    println!("{line}");
                }
            }
            if let Err(err) = self.print_py_report_collectionfinish(py) {
                eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
            }
            // The blank line separating collection from the run is omitted
            // at negative test-case verbosity (the progress chars group
            // directly under "collected N items"); --collect-only keeps it.
            if self.config.collect_only || self.config.test_case_verbosity() >= 0 {
                println!();
            }
        }

        if self.config.collect_only {
            // The --collect-only tree prints natively even in delegated
            // mode: upstream reporter plugins inherit it from the base
            // class rather than reimplementing it.
            if !self.config.no_terminal_explicit() {
                // pytest's _printcollecteditems keys the layout on the
                // test-case verbosity: < -1 → per-file counts, == -1 →
                // bare nodeids, >= 0 → the node tree (docstrings at >= 1).
                let tc = self.config.test_case_verbosity();
                if tc < -1 {
                    // -qq / verbosity_test_cases<-1: per-file counts.
                    let mut counts: Vec<(String, usize)> = Vec::new();
                    for item in &self.session.items {
                        let file = item.nodeid.split("::").next().unwrap_or("").to_string();
                        match counts.iter_mut().find(|(name, _)| name == &file) {
                            Some((_, count)) => *count += 1,
                            None => counts.push((file, 1)),
                        }
                    }
                    for (file, count) in counts {
                        println!("{file}: {count}");
                    }
                } else if tc == -1 {
                    for item in &self.session.items {
                        println!("{}", item.nodeid);
                    }
                } else {
                    self.print_collect_tree(py, tc >= 1);
                }
                if self.session.custom_reporter.is_some() {
                    // The closing stats line is the replacement reporter's
                    // (upstream collect-only still runs its sessionfinish
                    // wrapper, e.g. pretty's "Results:" table).
                    let code = if n_items == 0 {
                        exit_code::NO_TESTS_COLLECTED
                    } else {
                        exit_code::OK
                    };
                    if let Err(err) = self.fire_py_sessionfinish(py, code) {
                        eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
                    }
                    python::reporter_finish(py, &self.config, code, None);
                } else {
                    // Collection errors still surface their traceback (the
                    // ERRORS section) above the collected-count summary.
                    if n_collect_errors > 0 {
                        self.print_collect_errors();
                    }
                    self.print_collect_only_summary(started.elapsed(), n_collect_errors);
                }
            }
            return if n_collect_errors > 0 {
                exit_code::INTERRUPTED
            } else if n_items == 0 {
                exit_code::NO_TESTS_COLLECTED
            } else {
                exit_code::OK
            };
        }
        if n_items == 0 {
            // Upstream: zero collected items is NO_TESTS_COLLECTED even when
            // module-level skips produced skip reports.
            let code = exit_code::NO_TESTS_COLLECTED;
            if let Err(err) = self.fire_py_sessionfinish(py, code) {
                eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
            }
            // Stop the session-wide capture (errors surface on stderr).
            python::capture_session_end(py);
            if let Some(cache) = &self.cache {
                cache.sessionfinish(py, &self.config, &self.session.reports, &self.session.items);
            }
            if self.config.no_terminal() {
                self.write_junit_xml(py);
                if self.session.custom_reporter.is_some() && !self.config.is_worker() {
                    python::reporter_finish(py, &self.config, code, None);
                }
            } else {
                self.print_warnings_summary(py, 0, false);
                self.write_junit_xml(py);
                self.print_short_summary();
                let summary = crate::runner::summary_line(
                    &self.session.reports,
                    self.session.deselected,
                    python::warning_count(py),
                    started.elapsed(),
                    self.config.global_verbosity(),
                );
                if !summary.is_empty() {
                    println!("{summary}");
                }
            }
            return code;
        }

        #[cfg(feature = "xdist")]
        match self.resolve_numprocesses(py) {
            Some(workers) => self.run_dist(py, workers),
            None => self.run_items(py),
        }
        #[cfg(not(feature = "xdist"))]
        self.run_items(py);

        let failed = self
            .session
            .reports
            .iter()
            .any(|r| r.outcome == Outcome::Failed);
        let mut code = if failed {
            exit_code::TESTS_FAILED
        } else {
            exit_code::OK
        };

        if let Err(err) = self.fire_sessionfinish(py, code) {
            eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
        }
        // Stop the session-wide capture (errors surface on stderr).
        python::capture_session_end(py);
        if let Some(cache) = &self.cache {
            cache.sessionfinish(py, &self.config, &self.session.reports, &self.session.items);
        }
        if let Some(forced) = self.session.exit_code_override {
            code = forced;
        }

        if self.config.no_terminal() {
            self.write_junit_xml(py);
            // Delegated mode: the replacement reporter renders the
            // summaries the engine just suppressed.
            if self.session.custom_reporter.is_some() && !self.config.is_worker() {
                if let Err(err) = self.print_plugin_summaries(py, code) {
                    eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
                }
                let banner = self
                    .session
                    .shouldfail
                    .clone()
                    .or(self.session.abort_banner.clone())
                    .or_else(|| {
                        self.session
                            .stopped_after
                            .map(|n| format!("stopping after {n} failures"))
                    });
                python::reporter_finish(py, &self.config, code, banner.as_deref());
            }
            return code;
        }
        if let Some(banner) = &self.session.abort_banner {
            println!("{}", center_with(banner, '!'));
        }
        // --no-summary suppresses pytest's whole terminal-summary block
        // (FAILURES/ERRORS/PASSES/warnings/short summary + the conftest
        // pytest_terminal_summary hooks); the final stats line still shows.
        let no_summary = self.config.get_flag("no-summary");
        // Warnings shown in the first summary; the "(final)" pass after the
        // short summary reports any emitted during pytest_terminal_summary.
        let mut warnings_shown = 0usize;
        if !no_summary {
            // pytest's pytest_terminal_summary order: errors/failures/xfailures,
            // warnings (first), passes/xpasses, then the conftest & plugin
            // pytest_terminal_summary hooks (which may emit more warnings),
            // then the short summary and the final warnings pass.
            self.print_collect_errors();
            self.print_failures();
            self.print_xfailures();
            warnings_shown = self.print_warnings_summary(py, 0, false);
            self.print_passes();
            self.print_xpasses();
            if let Err(err) = self.print_plugin_summaries(py, code) {
                eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
            }
        }
        self.write_junit_xml(py);
        if let Some(banner) = &self.session.dist_banner {
            println!("{}", center_banner(banner));
        }
        if !no_summary {
            self.print_short_summary();
            self.print_warnings_summary(py, warnings_shown, true);
        }
        if let Some(n) = self.session.stopped_after {
            println!(
                "{}",
                center_with(&format!("stopping after {n} failures"), '!')
            );
        }
        if let Some(msg) = &self.session.shouldfail {
            println!("{}", center_with(msg, '!'));
        }
        // A test/plugin set session.shouldstop with a reason: pytest banners
        // it after the failure summary (the INTERRUPTED-exit case shows it via
        // the interrupt report instead). Exit code is INTERRUPTED regardless
        // of whether any tests also failed (e.g. --stepwise stops after the
        // first failure).
        if self.session.abort_banner.is_none()
            && let Some(reason) = python::session_shouldstop(py)
        {
            println!("{}", center_with(&format!("Interrupted: {reason}"), '!'));
            code = exit_code::INTERRUPTED;
        }
        let warning_count = python::warning_count(py) + self.session.worker_warning_count;
        let summary = crate::runner::summary_line(
            &self.session.reports,
            self.session.deselected,
            warning_count,
            started.elapsed(),
            self.config.global_verbosity(),
        );
        if !summary.is_empty() {
            println!("\n{summary}");
        }
        // Unraisable leftovers (e.g. refcycles with broken __del__, only
        // collectable after a forced gc) drain after the terminal reporter
        // has finished, like upstream's config cleanup; an error filter
        // propagates them to the top (exit 1, traceback on stderr). Stop
        // the warnings capture first so they print for real.
        let _ = py
            .import("pytest._wcapture")
            .and_then(|m| m.call_method0("uninstall"));
        if let Err(err) = python::unraisable_session_cleanup(py) {
            eprintln!("{}", python::format_exception(py, &err));
            if code == 0 {
                code = crate::report::exit_code::TESTS_FAILED;
            }
        }
        if let Err(err) = python::threadexception_session_cleanup(py) {
            eprintln!("{}", python::format_exception(py, &err));
            if code == 0 {
                code = crate::report::exit_code::TESTS_FAILED;
            }
        }
        code
    }

    /// In-process nested run (backs pytester's `inline_run`): a fresh session
    /// inside the already-running outer process. The shim, virtualenv, and
    /// assertion rewriting are global and already installed by the outer run,
    /// so we skip them and (re)configure only the per-session global state the
    /// `run_session` core depends on, then run it.
    ///
    /// The caller swaps in a fresh global capture state and snapshots `sys.*`
    /// around this call; minimal setup here is intentional — save/restore of
    /// each global is added empirically as nested fidelity requires it.
    pub(crate) fn run_nested(&mut self, py: Python<'_>) -> i32 {
        let started = Instant::now();
        // While this guard lives, hook dispatch notifies the plugin manager's
        // call monitors (HookRecorder) with live kwargs so getcalls works.
        let _recording = inprocess::RecordingGuard::enter();
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
        self.run_session(py, started)
    }

    /// Returns per-file collection errors (formatted).
    fn collect(&mut self, py: Python<'_>) -> Result<Vec<(PathBuf, String)>, String> {
        let rootdir = self.config.rootdir.clone();
        // No CLI paths: the `testpaths` ini (globbed against rootdir) decides
        // where collection starts; an empty glob warns and falls back to a
        // recursive search from the invocation dir, like pytest.
        let mut paths = self.config.paths.clone();
        let testpaths_lines = self.config.get_ini_lines("testpaths");
        // testpaths only applies when invocation_dir == rootdir (like pytest):
        // if you cd into a subdirectory, pytest ignores testpaths and collects
        // from the current directory instead.
        let invocation_is_root = self.config.invocation_dir == rootdir;
        if paths.is_empty() && !testpaths_lines.is_empty() && invocation_is_root {
            let entries: Vec<String> = testpaths_lines
                .into_iter()
                .flat_map(|v| v.split_whitespace().map(str::to_string))
                .collect();
            if !entries.is_empty() {
                let globbed = python::glob_testpaths(py, &rootdir, &entries)
                    .map_err(|err| python::format_exception(py, &err))?;
                if globbed.is_empty() {
                    let _ = python::warn_explicit_at(
                        py,
                        "PytestConfigWarning",
                        "No files were found in testpaths; consider removing or adjusting \
                         your testpaths configuration. Searching recursively from the \
                         current directory instead.",
                        &rootdir.to_string_lossy(),
                        0,
                    );
                } else {
                    paths = globbed;
                }
            }
        }
        // Relative CLI paths (and bare collection) resolve against the
        // invocation dir; rootdir only anchors node ids.
        let python_files = self.config.python_files_patterns();
        let norecursedirs = self.config.norecursedirs_patterns();
        let mut files = crate::collect::collect_test_files(
            &self.config.invocation_dir,
            &paths,
            self.config.get_flag("collect-in-virtualenv"),
            &python_files,
            &norecursedirs,
            self.config.get_flag("keep-duplicates"),
            &crate::collect::CollectIgnores::from_config(&self.config),
        )?;

        // -p NAME (non-"no:") plugins import before conftests, like
        // pytest's cmdline plugin loading. PYTEST_PLUGINS (comma-separated
        // module names) loads the same way — pytest's env-driven early
        // plugins, used when PYTEST_DISABLE_PLUGIN_AUTOLOAD is set.
        let mut named_plugins: Vec<String> = self
            .config
            .plugin_opts
            .iter()
            .filter(|spec| !spec.starts_with("no:"))
            .cloned()
            .collect();
        if let Ok(env_plugins) = std::env::var("PYTEST_PLUGINS") {
            for name in env_plugins
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                if !named_plugins.iter().any(|n| n == name) {
                    named_plugins.push(name.to_string());
                }
            }
        }
        if !named_plugins.is_empty()
            && let Err(err) = python::load_named_plugins(
                py,
                &named_plugins,
                Some(&self.config.invocation_dir),
                &mut self.session.registry,
                &mut self.session.py_hooks,
            )
        {
            return Err(python::format_exception(py, &err));
        }

        // Installed third-party plugins (pytest11 entry points) autoload
        // next, before conftests — pytest's setuptools plugin loading.
        let blocked: Vec<String> = self
            .config
            .plugin_opts
            .iter()
            .filter_map(|spec| spec.strip_prefix("no:"))
            .map(str::to_string)
            .collect();
        if let Err(err) = python::load_entrypoint_plugins(
            py,
            &blocked,
            &mut self.session.registry,
            &mut self.session.py_hooks,
            &mut self.session.plugin_distinfo,
        ) {
            return Err(python::format_exception(py, &err));
        }

        // Conftests load for every collection start dir (even ones with no
        // test files — pytest imports initial conftests during dir scan),
        // plus each collected file's directory chain.
        let mut start_dirs: Vec<PathBuf> = Vec::new();
        if paths.is_empty() {
            start_dirs.push(self.config.invocation_dir.clone());
        } else {
            for path in &paths {
                let fs_part = path.split("::").next().unwrap_or_default();
                let resolved = self.config.invocation_dir.join(fs_part);
                if resolved.is_dir() {
                    start_dirs.push(resolved);
                } else if let Some(parent) = resolved.parent() {
                    start_dirs.push(parent.to_path_buf());
                }
            }
        }
        start_dirs.extend(
            files
                .iter()
                .filter_map(|f| f.parent().map(std::path::Path::to_path_buf)),
        );

        let mut conftests: Vec<PathBuf> = Vec::new();
        for start in &start_dirs {
            let mut dir = Some(start.as_path());
            let mut chain = Vec::new();
            while let Some(d) = dir {
                let conftest = d.join("conftest.py");
                if conftest.exists() {
                    chain.push(conftest);
                }
                if d == rootdir {
                    break;
                }
                dir = d.parent();
            }
            chain.reverse();
            for conftest in chain {
                if !conftests.contains(&conftest) {
                    conftests.push(conftest);
                }
            }
        }

        let mut errors = Vec::new();
        if let Err(err) = python::register_builtin_fixtures(py, &mut self.session.registry) {
            return Err(python::format_exception(py, &err));
        }
        for conftest in &conftests {
            if let Err(err) = python::collect_conftest(
                py,
                &rootdir,
                conftest,
                &mut self.session.registry,
                &mut self.session.py_hooks,
            ) {
                errors.push((conftest.clone(), python::format_exception(py, &err)));
            }
        }
        // Upstream reports pytest_plugins in non-top-level conftests as an error.
        // When explicit paths are given, conftests in those ascending chains are
        // loaded before configure (exempt). When collecting from invocation_dir,
        // all non-rootdir conftests are loaded after configure and must be checked.
        let scan_skip_loaded = !paths.is_empty();
        scan_nontoplevel_pytest_plugins(
            &rootdir,
            &start_dirs,
            if scan_skip_loaded { &conftests } else { &[] },
            &mut errors,
        );

        // Plugin/conftest pytest_addoption hooks record their option and
        // ini specs (defaults for getoption/getini) before configure.
        if let Err(err) = self.fire_py_addoption_hooks(py) {
            errors.push((rootdir.clone(), python::format_exception(py, &err)));
        }
        // CLI tokens clap didn't know resolve against the specs registered
        // above; anything still unknown is a usage error (pytest parity).
        if let Err(err) = self.apply_plugin_cli_args(py) {
            // Usage errors print their bare message ("ERROR: <message>"),
            // not a class-prefixed traceback line.
            if python::is_usage_error(py, &err) {
                return Err(err.value(py).to_string());
            }
            return Err(python::format_exception(py, &err));
        }
        // Unknown config-option validation (pytest's _validate_config_options):
        // [pytest]-section keys that are neither a registered (plugin/conftest)
        // nor a core ini. Under --strict-config / the strict_config / strict
        // ini, the first is a fatal UsageError; otherwise each warns (and is
        // silenceable via filterwarnings).
        if !self.config.is_worker() {
            let ini_keys = self.config.ini_file_keys();
            let unknown = python::unknown_ini_keys(py, &ini_keys)
                .map_err(|err| python::format_exception(py, &err))?;
            if !unknown.is_empty() {
                let strict_config = self.config.ini_bool("strict_config");
                let strict = self.config.get_flag("strict-config")
                    || strict_config == Some(true)
                    || (strict_config.is_none()
                        && (self.config.get_flag("strict")
                            || self.config.ini_bool("strict") == Some(true)));
                if strict {
                    return Err(format!("Unknown config option: {}", unknown[0]));
                }
                let inipath = self
                    .config
                    .config_file_name
                    .as_ref()
                    .map(|name| rootdir.join(name).to_string_lossy().to_string())
                    .unwrap_or_else(|| rootdir.to_string_lossy().to_string());
                for key in &unknown {
                    let _ = python::warn_explicit_at(
                        py,
                        "PytestConfigWarning",
                        &format!("Unknown config option: {key}"),
                        &inipath,
                        0,
                    );
                }
            }
        }
        // --override-ini keys that aren't registered/core get the same warning
        // as unknown ini file keys (upstream issues this via config.getoption).
        if !self.config.is_worker() {
            let override_keys: Vec<String> = self.config.ini_overrides.keys().cloned().collect();
            if !override_keys.is_empty() {
                let unknown_overrides = python::unknown_ini_keys(py, &override_keys)
                    .map_err(|err| python::format_exception(py, &err))?;
                for key in &unknown_overrides {
                    let _ = python::warn_explicit_at(
                        py,
                        "PytestConfigWarning",
                        &format!("Unknown config option: {key}"),
                        "<cmdline>",
                        0,
                    );
                }
            }
        }
        // pytest_load_initial_conftests (pytest-env sets os.environ here),
        // after option specs are registered so getini resolves, before configure.
        if let Err(err) = self.fire_py_load_initial_conftests(py) {
            errors.push((rootdir.clone(), python::format_exception(py, &err)));
        }
        // The default 'terminalreporter' plugin registers before configure
        // so reporter-replacing plugins (pytest-sugar/pretty) find it.
        if let Err(err) = python::reporter_setup(py, &self.config) {
            errors.push((rootdir.clone(), python::format_exception(py, &err)));
        }
        // conftest pytest_configure hooks run once conftests are loaded.
        if let Err(err) = self.fire_py_hooks_simple(py, "pytest_configure") {
            if python::is_usage_error(py, &err) {
                // UsageError in configure → eprintln ERROR: msg, then exit 4.
                let msg = python::format_exception(py, &err);
                // Extract just the exception message (drop "pytest.UsageError: " prefix).
                let usage_msg = msg
                    .lines()
                    .last()
                    .and_then(|l| l.strip_prefix("pytest.UsageError: "))
                    .unwrap_or(msg.trim());
                eprintln!("ERROR: {usage_msg}");
                return Err("\x00USAGE_ERROR\x00".to_string());
            }
            errors.push((rootdir.clone(), python::format_exception(py, &err)));
        }
        // A plugin swapped in its own terminal reporter: suppress native
        // terminal output and drive the replacement object instead.
        if let Some(reporter) = python::reporter_replacement(py) {
            self.session.custom_reporter = Some(reporter);
            self.config.set_reporter_delegated();
        }
        // pytest_sessionstart python hooks fire before the header, like
        // upstream (the terminal's own sessionstart, which prints the header,
        // runs last under pluggy LIFO). A conftest sessionstart may stash
        // state the pytest_report_header hooks read back (e.g. config._x).
        if let Err(err) = self.fire_py_sessionstart(py) {
            // An unexpected exception in pytest_sessionstart is an INTERNALERROR
            // (exit 3), not a collection error (exit 2). Signal the caller with
            // a sentinel prefix so it can print the INTERNALERROR banner.
            let msg = python::format_exception(py, &err);
            return Err(format!("\x00INTERNAL\x00{msg}"));
        }
        // The session header: the replacement reporter's pytest_sessionstart
        // owns it in delegated mode (upstream prints it from that hook);
        // otherwise the native header plus pytest_report_header lines
        // (e.g. pytest-timeout's "timeout: 1.0s" block).
        if self.session.custom_reporter.is_some() {
            python::reporter_sessionstart(py, &self.config);
        } else {
            self.print_header(py);
            if let Err(err) = self.print_py_report_header(py) {
                errors.push((rootdir.clone(), python::format_exception(py, &err)));
            }
        }
        // --markers: list registered markers (configure hooks above already
        // ran their addinivalue_line("markers", ...)) and skip collection.
        if self.config.get_flag("markers") {
            if let Err(err) = self.print_markers(py) {
                errors.push((rootdir.clone(), python::format_exception(py, &err)));
            }
            return Ok(errors);
        }
        // Apply collect_ignore / collect_ignore_glob / pytest_ignore_collect from
        // loaded conftests. This is a post-filter after collect_test_files so that
        // conftest hooks can prune files from the collection set.
        // Note: for explicit path args, pytest_ignore_collect is NOT called (upstream
        // "not called on argument" behaviour). collect_ignore is always applied.
        let no_explicit_file_args = paths.is_empty();
        {
            // Gather ignore paths/globs from all loaded conftest modules.
            let mut extra_ignore_paths: Vec<std::path::PathBuf> = Vec::new();
            let mut extra_ignore_globs: Vec<String> = Vec::new();
            for conftest_path in &conftests {
                if let Some(conftest_dir) = conftest_path.parent() {
                    let (mut paths_from, mut globs_from) =
                        python::extract_collect_ignores(py, conftest_dir, conftest_path);
                    extra_ignore_paths.append(&mut paths_from);
                    extra_ignore_globs.append(&mut globs_from);
                }
            }
            if !extra_ignore_paths.is_empty() || !extra_ignore_globs.is_empty() {
                files.retain(|f| {
                    let f_canonical = std::fs::canonicalize(f).unwrap_or_else(|_| f.clone());
                    // collect_ignore: check if file or any ancestor is in the ignore list
                    for ip in &extra_ignore_paths {
                        let ip_canonical = std::fs::canonicalize(ip).unwrap_or_else(|_| ip.clone());
                        if f_canonical.starts_with(&ip_canonical) || f_canonical == ip_canonical {
                            return false;
                        }
                    }
                    // collect_ignore_glob: check against full path
                    if !extra_ignore_globs.is_empty() {
                        let f_str = f_canonical.to_string_lossy();
                        let f_name = f_canonical
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("");
                        for glob in &extra_ignore_globs {
                            if crate::collect::wildcard_match(glob, f_name)
                                || crate::collect::wildcard_match(glob, &f_str)
                            {
                                return false;
                            }
                        }
                    }
                    true
                });
            }
            // pytest_ignore_collect: only applied when no explicit file args
            if no_explicit_file_args {
                let mut kept = Vec::with_capacity(files.len());
                for f in files.drain(..) {
                    match python::call_ignore_collect_hooks(
                        py,
                        &self.session.py_hooks,
                        &f,
                        &rootdir,
                    ) {
                        None => kept.push(f),
                        Some(None) => {} // ignored silently
                        Some(Some(reason)) => {
                            // pytest.skip() in the hook: emit a skip report for this file
                            let nodeid = crate::collect::file_nodeid(&rootdir, &f);
                            self.session.reports.push(crate::report::TestReport {
                                nodeid,
                                phase: crate::report::Phase::Setup,
                                outcome: crate::report::Outcome::Skipped,
                                duration: std::time::Duration::ZERO,
                                longrepr: Some(reason),
                                location: None,
                                subtest_desc: None,
                                sections: Vec::new(),
                                rerun: false,
                                xfail_longrepr: None,
                                reprcrash_message: None,
                                head_line: None,
                            });
                        }
                    }
                }
                files = kept;
            }
        }

        // pytest's catching_logs around pytest_collection: a root handler
        // during import keeps module-level logging calls from triggering
        // logging.basicConfig (issue #6240).
        let log_level_cfg: Option<String> = self
            .config
            .get_value("log-level")
            .map(str::to_string)
            .or_else(|| self.config.get_ini("log_level").map(str::to_string));
        python::log_start_phase(py, "collection", log_level_cfg.as_deref());
        // Expose pytest_pycollect_makeitem hooks to Python for collect_class.
        {
            use pyo3::types::PyAnyMethods;
            let makeitem_hooks: Vec<Py<PyAny>> = self
                .session
                .py_hooks
                .iter()
                .filter(|h| h.name == "pytest_pycollect_makeitem")
                .map(|h| h.func.clone_ref(py))
                .collect();
            let _ = py
                .import("pytest._node")
                .and_then(|m| m.call_method1("set_pycollect_hooks", (makeitem_hooks,)));
        }
        // Explicit non-Python, non-text-doctest file args that no collector handles.
        let mut not_found_files: Vec<PathBuf> = Vec::new();
        for file in &files {
            // --maxfail aborts collection once the budget is spent on
            // collection errors, ignoring further files.
            if let Some(m) = self.config.maxfail()
                && errors.len() >= m
            {
                break;
            }
            let is_py = file.extension().and_then(|e| e.to_str()) == Some("py");
            if !is_py {
                // Non-Python files: only text files with doctest content.
                // For explicitly-specified files, collect regardless of --doctest-glob.
                // For scanned files, the glob loop below handles them.
                let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
                let is_text_doctest = matches!(ext, "txt" | "rst" | "md");
                if is_text_doctest {
                    if let Ok(py_config) = python::make_py_config(py, &self.config)
                        && let Err(err) = python::collect_doctests_from_textfile(
                            py,
                            &rootdir,
                            file,
                            &py_config,
                            &mut self.session.items,
                        )
                    {
                        errors.push((file.clone(), python::format_exception(py, &err)));
                    }
                } else {
                    // No collector can handle this file type (e.g. .pyc).
                    not_found_files.push(file.clone());
                }
                continue;
            }
            // Import-time output attaches to a failing collect report as
            // "Captured stdout/stderr" sections (pytest's
            // pytest_make_collect_report capture).
            python::capture_collect_begin(py);
            // Where this file's items start: --doctest-modules inserts the
            // module's doctest items BEFORE its functions (upstream order).
            let file_items_start = self.session.items.len();
            let collect_result = python::collect_module(
                py,
                &rootdir,
                file,
                &mut self.session.items,
                &mut self.session.registry,
                &mut self.session.py_hooks,
            );
            let collect_sections = python::capture_collect_end(py);
            let with_sections = |mut message: String| {
                for (title, text) in &collect_sections {
                    message.push_str(&format!(
                        "\n{:-^80}\n{}",
                        format!(" {title} "),
                        text.trim_end_matches('\n')
                    ));
                }
                message
            };
            let module_ok = match collect_result {
                Ok(()) => true,
                Err(ref err) if err.is_instance_of::<pyo3::exceptions::PyKeyboardInterrupt>(py) => {
                    // KeyboardInterrupt during collection stops immediately with
                    // the "!!! KeyboardInterrupt !!!" banner (exit 2).
                    return Err("\x00KEYBOARD_INTERRUPT\x00".to_string());
                }
                Err(ref err) if err.is_instance_of::<pyo3::exceptions::PySystemExit>(py) => {
                    // SystemExit during collection is an INTERNALERROR.
                    let msg = python::format_exception(py, err);
                    return Err(format!("\x00INTERNAL\x00{msg}"));
                }
                Err(err) => {
                    // pytest.skip(..., allow_module_level=True) or
                    // unittest.SkipTest at module import skip the whole module;
                    // a bare pytest.skip there is an error.
                    match python::module_level_skip(py, &err) {
                        Some(Ok(reason)) => {
                            let nodeid = crate::collect::file_nodeid(&rootdir, file);
                            // The skip call site (file:line), like pytest.
                            let location = python::raise_location(py, &err)
                                .unwrap_or_else(|| format!("{nodeid}:1"));
                            self.session.reports.push(crate::report::TestReport {
                                nodeid: nodeid.clone(),
                                phase: crate::report::Phase::Setup,
                                outcome: crate::report::Outcome::Skipped,
                                duration: std::time::Duration::ZERO,
                                longrepr: Some(reason),
                                location: Some(location),
                                subtest_desc: None,
                                sections: Vec::new(),
                                rerun: false,
                                xfail_longrepr: None,
                                reprcrash_message: None,
                                head_line: None,
                            });
                        }
                        Some(Err(message)) => errors.push((file.clone(), with_sections(message))),
                        // CollectError carries a user-facing message, no traceback.
                        None => {
                            match python::collect_error_message(py, &err) {
                                Some(message) => {
                                    errors.push((file.clone(), with_sections(message)))
                                }
                                None if python::is_import_error(py, &err) => {
                                    // A test module that fails to import gets
                                    // pytest's wrapped CollectError header
                                    // (importtestmodule), with a short-style
                                    // traceback.
                                    let tb = python::format_test_failure(py, &err, "short");
                                    let message = format!(
                                        "ImportError while importing test module '{}'.\n\
                                         Hint: make sure your test modules/packages have valid Python names.\n\
                                         Traceback:\n{tb}",
                                        file.display()
                                    );
                                    errors.push((file.clone(), with_sections(message)));
                                }
                                None => errors.push((
                                    file.clone(),
                                    // pytest-style frames + E lines (upstream
                                    // collect errors honor --tb; default "short"
                                    // matches pytest's auto style for collection).
                                    with_sections(python::format_test_failure(
                                        py,
                                        &err,
                                        self.config.get_value("tb").unwrap_or("short"),
                                    )),
                                )),
                            }
                            // Upstream DoctestModule: with --doctest-ignore-import-errors
                            // the doctest collector skips while the Module still errors.
                            if self.config.get_flag("doctest-modules")
                                && self.config.get_flag("doctest-ignore-import-errors")
                            {
                                let nodeid = crate::collect::file_nodeid(&rootdir, file);
                                self.session.reports.push(crate::report::TestReport {
                                    nodeid: nodeid.clone(),
                                    phase: crate::report::Phase::Setup,
                                    outcome: crate::report::Outcome::Skipped,
                                    duration: std::time::Duration::ZERO,
                                    longrepr: Some(format!(
                                        "unable to import module PosixPath('{}')",
                                        file.display()
                                    )),
                                    location: Some(format!("{nodeid}:1")),
                                    subtest_desc: None,
                                    sections: Vec::new(),
                                    rerun: false,
                                    xfail_longrepr: None,
                                    reprcrash_message: None,
                                    head_line: None,
                                });
                            }
                        }
                    }
                    false
                }
            };
            // --doctest-modules: collect doctests from each successfully-imported module.
            if module_ok
                && self.config.get_flag("doctest-modules")
                && let Ok(py_config) = python::make_py_config(py, &self.config)
            {
                let doctests_start = self.session.items.len();
                match python::collect_doctests_from_module(
                    py,
                    &rootdir,
                    file,
                    &py_config,
                    &mut self.session.items,
                ) {
                    Ok(()) => {
                        // The module's doctests run BEFORE its functions
                        // (upstream collects the DoctestModule first).
                        let n_doctests = self.session.items.len().saturating_sub(doctests_start);
                        self.session.items[file_items_start..].rotate_right(n_doctests);
                    }
                    Err(err) => {
                        // Non-fatal: log as collect error and continue.
                        errors.push((file.clone(), python::format_exception(py, &err)));
                    }
                }
            }
        }

        // Explicit file args with no matching collector → USAGE_ERROR.
        if !not_found_files.is_empty() {
            for file in &not_found_files {
                eprintln!("ERROR: not found: {}", file.display());
                eprintln!("(no match in any of [<Session ''>])");
                eprintln!();
            }
            return Err("\x00USAGE_ERROR\x00".to_string());
        }

        // --doctest-modules: also scan ALL .py files (not just test files) for doctests.
        if self.config.get_flag("doctest-modules") {
            let extra_py = crate::collect::collect_all_python_files(
                &self.config.invocation_dir,
                &paths,
                self.config.get_flag("collect-in-virtualenv"),
                &files,
            );
            if let Ok(py_config) = python::make_py_config(py, &self.config) {
                for extra_file in &extra_py {
                    // Import the module and collect doctests.
                    if let Err(err) = python::collect_doctests_from_module(
                        py,
                        &rootdir,
                        extra_file,
                        &py_config,
                        &mut self.session.items,
                    ) {
                        // Import errors skip the module with --doctest-ignore-import-errors.
                        if self.config.get_flag("doctest-ignore-import-errors") {
                            let nodeid = crate::collect::file_nodeid(&rootdir, extra_file);
                            self.session.reports.push(crate::report::TestReport {
                                nodeid: nodeid.clone(),
                                phase: crate::report::Phase::Setup,
                                outcome: crate::report::Outcome::Skipped,
                                duration: std::time::Duration::ZERO,
                                longrepr: Some(format!(
                                    "unable to import module PosixPath('{}')",
                                    extra_file.display()
                                )),
                                location: Some(format!("{nodeid}:1")),
                                subtest_desc: None,
                                sections: Vec::new(),
                                rerun: false,
                                xfail_longrepr: None,
                                reprcrash_message: None,
                                head_line: None,
                            });
                        } else {
                            errors.push((extra_file.clone(), python::format_exception(py, &err)));
                        }
                    }
                }
            }
        }

        // Text files matching the glob (default: test*.txt) are always collected
        // even without explicit --doctest-modules or --doctest-glob flags, mirroring
        // upstream pytest's _is_doctest() behavior.
        let scan_text_files = true;
        if scan_text_files && let Ok(py_config) = python::make_py_config(py, &self.config) {
            let text_files =
                crate::collect::collect_doctest_textfiles(&self.config.invocation_dir, &paths);
            for tf in text_files {
                // Skip files already collected in the explicit-file loop above.
                if files.contains(&tf) {
                    continue;
                }
                if let Ok(true) = python::is_doctest_textfile(py, &tf, &py_config)
                    && let Err(err) = python::collect_doctests_from_textfile(
                        py,
                        &rootdir,
                        &tf,
                        &py_config,
                        &mut self.session.items,
                    )
                {
                    errors.push((tf.clone(), python::format_exception(py, &err)));
                }
            }
        }

        // Custom collectors: plugins like pytest-ruff / pytest-mypy collect
        // non-test files via pytest_collect_file -> pytest.File.collect().
        // Only walk the (broader) candidate file set when such a hook exists.
        if python::has_collect_file_hook(py, &self.session.py_hooks) {
            let candidate = crate::collect::collect_all_python_files_ext(
                &self.config.invocation_dir,
                &paths,
                self.config.get_flag("collect-in-virtualenv"),
                &[],
                // pytest-mypy's pytest_collect_file also handles .pyi stubs.
                true,
            );
            let hooks = std::mem::take(&mut self.session.py_hooks);
            let result = python::collect_custom_files(
                py,
                &rootdir,
                &candidate,
                &hooks,
                &mut self.session.items,
            );
            self.session.py_hooks = hooks;
            match result {
                Ok(skipped_files) => {
                    for (file, reason) in skipped_files {
                        let nodeid = crate::collect::file_nodeid(&rootdir, &file);
                        self.session.reports.push(crate::report::TestReport {
                            nodeid,
                            phase: crate::report::Phase::Setup,
                            outcome: crate::report::Outcome::Skipped,
                            duration: std::time::Duration::ZERO,
                            longrepr: Some(reason),
                            location: None,
                            subtest_desc: None,
                            sections: Vec::new(),
                            rerun: false,
                            xfail_longrepr: None,
                            reprcrash_message: None,
                            head_line: None,
                        });
                    }
                }
                Err(err) => {
                    errors.push((rootdir.clone(), python::format_exception(py, &err)));
                }
            }
        }

        // Collection over: close its catching_logs phase.
        python::log_end_phase(py);

        // Expand items over parametrized fixtures in their closure; plugins
        // first get to inject closure-affecting marks (anyio's usefixtures).
        let mut items = std::mem::take(&mut self.session.items);
        {
            let mut ctx = HookContext {
                py,
                session: &mut self.session,
                config: &self.config,
            };
            for plugin in &self.plugins {
                if let Err(err) = plugin.pytest_collection_preexpand(&mut ctx, &mut items) {
                    self.session.items = items;
                    return Err(python::format_exception(py, &err));
                }
            }
        }
        match python::expand_fixture_params(py, items, &self.session.registry) {
            Ok(expanded) => self.session.items = expanded,
            Err(err) => return Err(python::format_exception(py, &err)),
        }

        // request.fixturenames must list the item's whole fixture closure
        // (transitive deps + autouse), not just its direct params — plugins
        // probe it (pytest-django: "transactional_db" in request.fixturenames,
        // pulled in transitively by django_db_reset_sequences). Record the
        // closure-only names as extra fixturenames (display only; the fixtures
        // themselves resolve through the dependency chain).
        for item in &mut self.session.items {
            let mut direct: Vec<String> = item.fixture_names.clone();
            direct.extend(item.extra_fixture_names.iter().cloned());
            let closure = self.session.registry.closure_for(&item.nodeid, &direct);
            for def in closure {
                if !item.fixture_names.contains(&def.name)
                    && !item.extra_fixture_names.contains(&def.name)
                {
                    item.extra_fixture_names.push(def.name.clone());
                }
            }
        }

        // Node-id args ("file.py::TestCls::test_a") restrict collection to
        // matching items; unlike -k/-m this is not a deselection.
        enum ArgSel {
            Path(PathBuf),
            NodeId(String),
        }
        if paths.iter().any(|arg| arg.contains("::")) {
            let arg_sels: Vec<ArgSel> = paths
                .iter()
                .map(|arg| match arg.split_once("::") {
                    Some((file_part, rest)) => {
                        let path = self.config.invocation_dir.join(file_part);
                        let path = std::fs::canonicalize(&path).unwrap_or(path);
                        ArgSel::NodeId(format!(
                            "{}::{}",
                            crate::collect::file_nodeid(&rootdir, &path),
                            rest
                        ))
                    }
                    None => {
                        let path = self.config.invocation_dir.join(arg);
                        ArgSel::Path(std::fs::canonicalize(&path).unwrap_or(path))
                    }
                })
                .collect();
            self.session.items.retain(|item| {
                arg_sels.iter().any(|sel| match sel {
                    ArgSel::Path(path) => item.path.starts_with(path),
                    ArgSel::NodeId(sel) => {
                        item.nodeid == *sel
                            || item
                                .nodeid
                                .strip_prefix(sel.as_str())
                                .is_some_and(|rest| rest.starts_with('[') || rest.starts_with("::"))
                    }
                })
            });
            // Emit "not found" error to stderr for NodeId args that matched nothing.
            for sel in &arg_sels {
                if let ArgSel::NodeId(nodeid) = sel {
                    let matched = self.session.items.iter().any(|item| {
                        item.nodeid == *nodeid
                            || item
                                .nodeid
                                .strip_prefix(nodeid.as_str())
                                .is_some_and(|r| r.starts_with('[') || r.starts_with("::"))
                    });
                    if !matched {
                        eprintln!("ERROR: not found: {nodeid}");
                    }
                }
            }
        }

        // --lf drops failure-free files (and non-failed top-level functions
        // of failed files) at collection time.
        if let Some(cache) = &mut self.cache {
            cache.filter_collected_items(
                &rootdir,
                &self.config.invocation_dir,
                &paths,
                &mut self.session.items,
            );
        }
        Ok(errors)
    }
}

/// The one-line summary appended to FAILED/ERROR entries: the first
/// E-prefixed explanation line, else the exception line.
fn short_message(longrepr: &str) -> Option<String> {
    let from_e_line = longrepr.lines().find_map(|line| {
        line.strip_prefix("E ")
            .map(|rest| rest.trim_start().to_string())
    });
    from_e_line
        .or_else(|| {
            // Native exception-group repr: the group's own message line
            // ("ExceptionGroup: ... (2 sub-exceptions)"), not the box art.
            longrepr
                .lines()
                .find(|line| line.trim_end().ends_with("sub-exceptions)"))
                .or_else(|| {
                    longrepr
                        .lines()
                        .find(|line| line.trim_end().ends_with("sub-exception)"))
                })
                .map(|line| line.trim().trim_start_matches('|').trim().to_string())
        })
        .or_else(|| {
            longrepr
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .map(|line| line.trim().to_string())
        })
        .filter(|message| !message.is_empty())
}

pub fn center_banner(label: &str) -> String {
    center_with(label, '=')
}

fn center_named(label: &str) -> String {
    center_with(label, '_')
}

pub fn center_with(label: &str, fill: char) -> String {
    const WIDTH: usize = 80;
    let label = format!(" {label} ");
    let pad = WIDTH.saturating_sub(label.len());
    let left = (pad / 2).max(1);
    let right = (pad - pad / 2).max(1);
    format!(
        "{}{}{}",
        fill.to_string().repeat(left),
        label,
        fill.to_string().repeat(right)
    )
}

/// Scan the collection start directories (and their subdirs) for conftest.py
/// files not already loaded. If any non-top-level conftest contains
/// `pytest_plugins`, add an error — upstream reports this since pytest 7.
fn scan_nontoplevel_pytest_plugins(
    rootdir: &std::path::Path,
    start_dirs: &[std::path::PathBuf],
    skip_loaded: &[std::path::PathBuf],
    errors: &mut Vec<(std::path::PathBuf, String)>,
) {
    fn walk(
        dir: &std::path::Path,
        rootdir: &std::path::Path,
        skip_loaded: &[std::path::PathBuf],
        errors: &mut Vec<(std::path::PathBuf, String)>,
    ) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut children: Vec<std::path::PathBuf> =
            entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        children.sort();
        for child in children {
            if child.is_dir() {
                // Don't descend into hidden dirs or known skip dirs.
                if child
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with('.') || n == "__pycache__" || n == "node_modules")
                    .unwrap_or(false)
                {
                    continue;
                }
                walk(&child, rootdir, skip_loaded, errors);
            } else if child.file_name().and_then(|n| n.to_str()) == Some("conftest.py") {
                // Top-level (rootdir/conftest.py) is exempt.
                if child.parent() == Some(rootdir) {
                    continue;
                }
                // Conftests in the ascending chain from explicit test paths are
                // loaded before configure in real pytest and are exempt.
                if skip_loaded.contains(&child) {
                    continue;
                }
                // Quick text scan for `pytest_plugins` assignment.
                if let Ok(content) = std::fs::read_to_string(&child)
                    && content.contains("pytest_plugins")
                {
                    let rel = child
                        .strip_prefix(rootdir)
                        .unwrap_or(&child)
                        .components()
                        .map(|c| c.as_os_str().to_string_lossy())
                        .collect::<Vec<_>>()
                        .join(std::path::MAIN_SEPARATOR_STR);
                    errors.push((
                        child,
                        format!(
                            "Defining 'pytest_plugins' in a non-top-level conftest is \
                                 no longer supported: please remove it from {rel}"
                        ),
                    ));
                }
            }
        }
    }
    for start in start_dirs {
        walk(start, rootdir, skip_loaded, errors);
    }
}
