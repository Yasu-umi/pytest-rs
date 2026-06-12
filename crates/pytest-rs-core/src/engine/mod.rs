use std::time::Instant;

use pyo3::prelude::*;

use crate::config::Config;
use crate::hooks::Plugin;
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

mod collect;
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
                    "versions pytest-rs-{}, python-{}\n\
                     pytest_configure\n\
                     pytest_sessionstart\n",
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
                // run unconfigure and exit 4. May have an error message appended.
                if let Some(inner) = message.strip_prefix("\x00USAGE_ERROR\x00") {
                    if !inner.is_empty() {
                        eprintln!("ERROR during collection:\n{inner}");
                    }
                    let _ = self.fire_py_hooks_simple(py, "pytest_unconfigure");
                    return exit_code::USAGE_ERROR;
                }
                // Sentinel "\x00EXIT\x00{code}": pytest.exit() during configure or
                // sessionstart — banner already set on session if needed.
                if let Some(rest) = message.strip_prefix("\x00EXIT\x00") {
                    let code = rest.parse().unwrap_or(exit_code::INTERRUPTED);
                    if let Some(banner) = &self.session.abort_banner.clone() {
                        println!("{}", center_with(banner, '!'));
                    }
                    let _ = self.fire_py_hooks_simple(py, "pytest_unconfigure");
                    return code;
                }
                eprintln!("ERROR: {message}");
                return exit_code::USAGE_ERROR;
            }
        };
        // Collection done: tests (and gc-dependent plugins) run from here on.
        python::set_gc_enabled(py, true);
        let n_collect_errors = collect_errors.len();
        if let Some(code) = self.handle_collection_errors(py, collect_errors, started) {
            return code;
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
        // Count module-level skips (pytest.skip/importorskip at module level)
        // that were recorded during collection; these show as "/ N skipped"
        // in the "collected N items" line, matching pytest's format.
        let n_collect_skips = self
            .session
            .reports
            .iter()
            .filter(|r| {
                r.phase == crate::report::Phase::Setup
                    && r.outcome == crate::report::Outcome::Skipped
            })
            .count();
        self.print_collection_count(py, collected, n_collect_errors, n_collect_skips, n_items);

        if self.config.collect_only {
            return self.run_collect_only(py, started, n_collect_errors, n_items);
        }
        if n_items == 0 {
            return self.handle_no_tests(py, started);
        }

        #[cfg(feature = "xdist")]
        match self.resolve_numprocesses(py) {
            Some(workers) => self.run_dist(py, workers),
            None => self.run_items(py),
        }
        #[cfg(not(feature = "xdist"))]
        self.run_items(py);

        self.finish_session(py, started)
    }

    /// Records collection-error reports and, when collection must abort
    /// (no --continue-on-collection-errors, or --maxfail hit), prints the
    /// summary and returns the exit code. Returns `None` to keep running.
    fn handle_collection_errors(
        &mut self,
        py: Python<'_>,
        collect_errors: Vec<(std::path::PathBuf, String)>,
        started: Instant,
    ) -> Option<i32> {
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
            // With explicit --maxfail=N, allow N errors before aborting (so
            // --maxfail=2 overrides -x, which would otherwise abort at 1).
            let should_abort = if self.config.get_flag("continue-on-collection-errors")
                || self.config.collect_only
            {
                false
            } else {
                let budget = self.config.maxfail().unwrap_or(1);
                n_collect_errors >= budget
            };
            if should_abort {
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
                if !self.config.is_worker() {
                    // pytest fires sessionfinish even on aborted collection
                    // (pretty's wall-clock end time comes from it).
                    if let Err(err) = self.fire_py_sessionfinish(py, code) {
                        eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
                    }
                    if self.session.custom_reporter.is_some() {
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
                }
                return Some(code);
            }
        }
        None
    }

    /// Prints the "collected N items" line plus cache status / stepwise lines
    /// and the pytest_report_collectionfinish hook output.
    fn print_collection_count(
        &mut self,
        py: Python<'_>,
        collected: usize,
        n_collect_errors: usize,
        n_collect_skips: usize,
        n_items: usize,
    ) {
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
            // deselected, skipped and selected counts can all appear together.
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
            if n_collect_skips > 0 {
                line += &format!(
                    " / {n_collect_skips} skipped",
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
    }

    /// --collect-only: print the collected tree/nodeids/counts and return.
    fn run_collect_only(
        &mut self,
        py: Python<'_>,
        started: Instant,
        n_collect_errors: usize,
        n_items: usize,
    ) -> i32 {
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

    /// Zero collected items: fire sessionfinish, print summaries, and return
    /// NO_TESTS_COLLECTED.
    fn handle_no_tests(&mut self, py: Python<'_>, started: Instant) -> i32 {
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

    /// Post-run finalization: sessionfinish, caches, the terminal-summary
    /// block, and unraisable/threadexception cleanup. Returns the exit code.
    fn finish_session(&mut self, py: Python<'_>, started: Instant) -> i32 {
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

        if !self.config.no_terminal() && !self.session.reports.is_empty() {
            // Real pytest's sessionfinish prints _tw.line("") before the summary
            // sections, creating a blank line between test output and the summary.
            println!();
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
            println!("{summary}");
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
        // Shadow the process-global config proxy with one built from the nested
        // config, so getini/getoption read the nested run's tox.ini/options
        // instead of the cached outer singleton. Dropped when the run ends.
        let _config_proxy = python::push_nested_config_proxy(py, &self.config).ok();
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
        // Re-set PYTEST_VERSION so monkeypatch overrides in the outer test don't
        // bleed into the inner run (the outer run sets it once in Engine::run).
        if let Ok(version) = python::pytest_version(py) {
            python::setenv(py, "PYTEST_VERSION", &version);
        }
        self.run_session(py, started)
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
