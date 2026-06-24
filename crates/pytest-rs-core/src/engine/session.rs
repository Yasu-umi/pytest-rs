use std::time::Instant;

use pyo3::prelude::*;

use super::super::Engine;
use crate::python;
use crate::report::{Outcome, exit_code};

use super::{center_banner, center_with};

impl Engine {
    pub(crate) fn run_session(&mut self, py: Python<'_>, started: Instant) -> i32 {
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

        // --markers (and similar early-exit modes) printed their output during
        // collect and skipped item collection.  Return OK now — falling through
        // would reach handle_no_tests() → exit 5 ("no tests ran").
        if self.config.get_flag("markers") {
            let _ = self.fire_py_hooks_simple(py, "pytest_unconfigure");
            return exit_code::OK;
        }

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

        // --fixtures / --fixtures-per-test: like --collect-only, collect then
        // print (fixtures rather than the item tree) and exit without running.
        if self.config.get_flag("fixtures") {
            if let Err(err) = self.show_fixtures(py) {
                eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
            }
            let _ = self.fire_py_hooks_simple(py, "pytest_unconfigure");
            return exit_code::OK;
        }
        if self.config.get_flag("fixtures-per-test") {
            if let Err(err) = self.show_fixtures_per_test(py) {
                eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
            }
            let _ = self.fire_py_hooks_simple(py, "pytest_unconfigure");
            return exit_code::OK;
        }

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
    pub(crate) fn handle_no_tests(&mut self, py: Python<'_>, started: Instant) -> i32 {
        // Upstream: zero collected items is NO_TESTS_COLLECTED even when
        // module-level skips produced skip reports.
        let mut code = exit_code::NO_TESTS_COLLECTED;
        let mut session_exited = false;
        if let Err(err) = self.fire_py_sessionfinish(py, code) {
            if let Some(returncode) = python::exit_returncode(py, &err) {
                let msg = python::exit_msg(py, &err);
                if !msg.is_empty() {
                    eprintln!("Exit: {msg}");
                }
                if let Some(rc) = returncode {
                    code = rc;
                }
                session_exited = true;
            } else {
                eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
            }
        }
        // Stop the session-wide capture (errors surface on stderr).
        python::capture_session_end(py);
        if let Some(cache) = &self.cache {
            cache.sessionfinish(py, &self.config, &self.session.reports, &self.session.items);
        }
        if session_exited {
            return code;
        }
        if !self.config.no_terminal() {
            println!();
        }
        if self.config.no_terminal() {
            self.write_junit_xml(py);
            if self.session.custom_reporter.is_some() && !self.config.is_worker() {
                python::reporter_finish(py, &self.config, code, None);
            }
        } else {
            let warnings_shown = self.print_warnings_summary(py, 0, false);
            if let Err(err) = self.print_plugin_summaries(py, code) {
                eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
            }
            self.write_junit_xml(py);
            self.print_short_summary();
            self.print_warnings_summary(py, warnings_shown, true);
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
        code
    }

    /// Post-run finalization: sessionfinish, caches, the terminal-summary
    /// block, and unraisable/threadexception cleanup. Returns the exit code.
    pub(crate) fn finish_session(&mut self, py: Python<'_>, started: Instant) -> i32 {
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

        let mut session_exited = false;
        if let Err(err) = self.fire_sessionfinish(py, code) {
            if let Some(returncode) = python::exit_returncode(py, &err) {
                let msg = python::exit_msg(py, &err);
                if !msg.is_empty() {
                    eprintln!("Exit: {msg}");
                }
                if let Some(rc) = returncode {
                    code = rc;
                }
                session_exited = true;
            } else if err.is_instance_of::<pyo3::exceptions::PyKeyboardInterrupt>(py) {
                code = exit_code::INTERRUPTED;
            } else {
                eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
            }
        }
        // Stop the session-wide capture (errors surface on stderr).
        python::capture_session_end(py);
        if let Some(cache) = &self.cache {
            cache.sessionfinish(py, &self.config, &self.session.reports, &self.session.items);
        }
        if session_exited {
            return code;
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
            self.print_durations();
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
        // it after the failure summary. When shouldfail is NOT also set (e.g.
        // stepwise stops on first failure), real pytest raises Interrupted which
        // produces "Interrupted: <reason>" in the repr; when shouldfail IS also
        // set (--maxfail + manual shouldstop) the banner is printed plain.
        // Check both Rust-side (timeout) and Python-side (maxfail plugin) shouldfail.
        if self.session.abort_banner.is_none()
            && let Some(reason) = python::session_shouldstop(py)
        {
            let has_shouldfail =
                self.session.shouldfail.is_some() || python::session_shouldfail(py).is_some();
            let banner = if has_shouldfail {
                reason
            } else {
                format!("Interrupted: {reason}")
            };
            println!("{}", center_with(&banner, '!'));
            code = exit_code::INTERRUPTED;
        }
        let warning_count = python::warning_count(py) + self.session.worker_warning_count;
        let extra_stats = python::reporter_subtest_stats(py);
        let summary = crate::runner::summary_line_with_extras(
            &self.session.reports,
            self.session.deselected,
            warning_count,
            started.elapsed(),
            self.config.global_verbosity(),
            &extra_stats,
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
        // pytest_unconfigure: mirrors upstream's config teardown (fired after
        // the terminal summary, just before the session returns). conftest and
        // plugin hooks observe it; in pytester inline runs the HookRecorder's
        // getcalls("pytest_unconfigure") sees the live config via record_hook.
        let _ = self.fire_py_hooks_simple(py, "pytest_unconfigure");
        code
    }
}
