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
        if let Err(err) = python::activate_virtualenv(py) {
            eprintln!("INTERNAL ERROR: failed to activate virtualenv: {err}");
            return exit_code::INTERNAL_ERROR;
        }
        if let Err(err) = python::install_shim(py) {
            eprintln!("INTERNAL ERROR: failed to install pytest shim: {err}");
            return exit_code::INTERNAL_ERROR;
        }
        // --debug: pytest's debug trace file (minimal: create the file and
        // announce it on stderr like upstream).
        if let Some(name) = self.config.get_value("debug") {
            let path = self.config.invocation_dir.join(name);
            let _ = std::fs::write(
                &path,
                format!(
                    "versions pytest-rs-{}, python-{}\n",
                    env!("CARGO_PKG_VERSION"),
                    py.version().split_whitespace().next().unwrap_or("")
                ),
            );
            eprintln!("writing pytestdebug information to {}", path.display());
        }
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
            .get_ini("filterwarnings")
            .map(|lines| {
                lines
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if let Err(err) = python::install_warning_capture(py, &ini_filters, &self.config.w_options)
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
                eprintln!("ERROR: {message}");
                return exit_code::USAGE_ERROR;
            }
        };
        let n_collect_errors = collect_errors.len();
        if n_collect_errors > 0 {
            // Collection errors still report as errors in the summary.
            for (path, err) in collect_errors {
                let nodeid = crate::collect::file_nodeid(&self.config.rootdir, &path);
                self.session
                    .collect_errors
                    .push((nodeid.clone(), err.clone()));
                // Delegated mode: the replacement reporter sees a failed
                // CollectReport (sugar prints these instantly).
                if self.session.custom_reporter.is_some() {
                    python::reporter_collect_error(py, &nodeid, &err);
                }
                self.session.reports.push(crate::report::TestReport {
                    nodeid,
                    phase: Phase::Setup,
                    outcome: Outcome::Failed,
                    duration: std::time::Duration::ZERO,
                    longrepr: Some(err),
                    location: None,
                    subtest_desc: None,
                    sections: Vec::new(),
                });
            }
            // --maxfail aborting collection exits TESTS_FAILED with a
            // "stopping after N failures" banner; otherwise INTERRUPTED.
            let maxfail_hit = self.config.maxfail().is_some_and(|m| n_collect_errors >= m);
            if !self.config.get_flag("continue-on-collection-errors") || maxfail_hit {
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
                    self.print_collect_errors();
                }
                // A file that fails collection is a "last failed" entry.
                if let Some(cache) = &self.cache {
                    cache.sessionfinish(py, &self.config, &self.session.reports);
                }
                self.write_junit_xml(py);
                if !self.config.no_terminal() {
                    self.print_short_summary();
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
                    self.print_collect_only_summary(started.elapsed());
                }
            }
            return if n_items == 0 {
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
                cache.sessionfinish(py, &self.config, &self.session.reports);
            }
            if self.config.no_terminal() {
                self.write_junit_xml(py);
                if self.session.custom_reporter.is_some() && !self.config.is_worker() {
                    python::reporter_finish(py, &self.config, code, None);
                }
            } else {
                self.print_warnings_summary(py);
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
            cache.sessionfinish(py, &self.config, &self.session.reports);
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
        if !no_summary {
            // --continue-on-collection-errors: the ERRORS section was deferred
            // until after the run, like pytest's terminal reporter.
            self.print_collect_errors();
            self.print_failures();
            if let Err(err) = self.print_plugin_summaries(py, code) {
                eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
            }
            self.print_warnings_summary(py);
            self.print_passes();
        }
        self.write_junit_xml(py);
        if let Some(banner) = &self.session.dist_banner {
            println!("{}", center_banner(banner));
        }
        if !no_summary {
            self.print_short_summary();
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
        // the interrupt report instead; --stepwise drives its own banner).
        if !self.config.get_flag("sw")
            && !self.config.get_flag("sw-skip")
            && self.session.abort_banner.is_none()
            && let Some(reason) = python::session_shouldstop(py)
        {
            println!("{}", center_with(&reason, '!'));
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

    /// Returns per-file collection errors (formatted).
    fn collect(&mut self, py: Python<'_>) -> Result<Vec<(PathBuf, String)>, String> {
        let rootdir = self.config.rootdir.clone();
        // No CLI paths: the `testpaths` ini (globbed against rootdir) decides
        // where collection starts; an empty glob warns and falls back to a
        // recursive search from the invocation dir, like pytest.
        let mut paths = self.config.paths.clone();
        if paths.is_empty()
            && let Some(testpaths) = self.config.get_ini("testpaths")
        {
            let entries: Vec<String> = testpaths.split_whitespace().map(str::to_string).collect();
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
        let files = crate::collect::collect_test_files(
            &self.config.invocation_dir,
            &paths,
            self.config.get_flag("collect-in-virtualenv"),
            &python_files,
            &norecursedirs,
            self.config.get_flag("keep-duplicates"),
            &crate::collect::CollectIgnores::from_config(&self.config),
        )?;

        // -p NAME (non-"no:") plugins import before conftests, like
        // pytest's cmdline plugin loading.
        let named_plugins: Vec<String> = self
            .config
            .plugin_opts
            .iter()
            .filter(|spec| !spec.starts_with("no:"))
            .cloned()
            .collect();
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

        // Plugin/conftest pytest_addoption hooks record their option and
        // ini specs (defaults for getoption/getini) before configure.
        if let Err(err) = self.fire_py_addoption_hooks(py) {
            errors.push((rootdir.clone(), python::format_exception(py, &err)));
        }
        // CLI tokens clap didn't know resolve against the specs registered
        // above; anything still unknown is a usage error (pytest parity).
        if let Err(err) = self.apply_plugin_cli_args(py) {
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
        // The default 'terminalreporter' plugin registers before configure
        // so reporter-replacing plugins (pytest-sugar/pretty) find it.
        if let Err(err) = python::reporter_setup(py, &self.config) {
            errors.push((rootdir.clone(), python::format_exception(py, &err)));
        }
        // conftest pytest_configure hooks run once conftests are loaded.
        if let Err(err) = self.fire_py_hooks_simple(py, "pytest_configure") {
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
            errors.push((rootdir.clone(), python::format_exception(py, &err)));
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
        // pytest's catching_logs around pytest_collection: a root handler
        // during import keeps module-level logging calls from triggering
        // logging.basicConfig (issue #6240).
        let log_level_cfg: Option<String> = self
            .config
            .get_value("log-level")
            .map(str::to_string)
            .or_else(|| self.config.get_ini("log_level").map(str::to_string));
        python::log_start_phase(py, "collection", log_level_cfg.as_deref());
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
                if is_text_doctest
                    && let Ok(py_config) = python::make_py_config(py, &self.config)
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
                                    // collect errors honor --tb).
                                    with_sections(python::format_test_failure(
                                        py,
                                        &err,
                                        self.config.get_value("tb").unwrap_or("long"),
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
