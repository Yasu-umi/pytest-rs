use std::time::Instant;

use pyo3::prelude::*;

use super::super::Engine;
use crate::python;
use crate::report::{Outcome, exit_code};

use super::{center_banner, center_with, flush_hook_output};

impl Engine {
    pub(crate) fn run_session(&mut self, py: Python<'_>, started: Instant) -> i32 {
        #[cfg(feature = "xdist")]
        {
            let using_xdist = self.config.is_worker()
                || self.config.numprocesses_spec().is_some_and(|s| s != "0")
                || self.config.get_flag("dist-load")
                || self.config.get_value("tx").is_some();
            if using_xdist {
                python::register_xdist_marker_plugin(py);
            }
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
            // configure_capture() starts this run's capture already
            // suspended (only collection/each item's own resume calls turn
            // it on); this early-return path never reaches either, so
            // cache_show's print() would otherwise land wherever sys.stdout
            // pointed *before* this run's capture was installed — a nested
            // run's outer session buffer, not this run's own captured
            // stdout. Resume first so it's actually captured, then flush
            // (every normal exit path does the same before finishing; see
            // finish_session/handle_no_tests).
            let _ = python::capture_force_resume(py);
            let result = python::cache_show(py, &self.config, glob);
            python::capture_session_end(py);
            return match result {
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
                // Sentinel "\x00INTERNAL_DONE\x00{code}" means an unexpected hook
                // exception (e.g. conftest pytest_sessionstart raised) — the
                // banner and pytest_internalerror dispatch already ran at the
                // point of failure (collection.rs), carrying the resolved exit
                // code (possibly overridden by an Exit raised from a hookimpl).
                if let Some(code) = message.strip_prefix("\x00INTERNAL_DONE\x00") {
                    return code.parse().unwrap_or(exit_code::INTERNAL_ERROR);
                }
                // Sentinel "\x00INTERNAL_STDERR\x00": unexpected exception in
                // pytest_configure — print INTERNALERROR to stderr (upstream
                // routes configure failures to stderr, vs sessionstart on
                // stdout). #49
                if let Some(inner) = message.strip_prefix("\x00INTERNAL_STDERR\x00") {
                    for line in inner.lines() {
                        eprintln!("INTERNALERROR> {line}");
                    }
                    return exit_code::INTERNAL_ERROR;
                }
                // Sentinel "\x00KEYBOARD_INTERRUPT\x00": KeyboardInterrupt during
                // collection — print the special "!!! KeyboardInterrupt !!!" banner.
                if message == "\x00KEYBOARD_INTERRUPT\x00" {
                    println!("!!! KeyboardInterrupt !!!");
                    return exit_code::INTERRUPTED;
                }
                // Sentinel "\x00CONFTEST_IMPORT_ERROR\x00": a broken initial
                // conftest — print upstream's dedicated repr verbatim (no
                // "ERROR during collection:" wrapper) and exit 4.
                if let Some(inner) = message.strip_prefix("\x00CONFTEST_IMPORT_ERROR\x00") {
                    eprintln!("{inner}");
                    let _ = flush_hook_output(py, || {
                        self.fire_py_hooks_simple(py, "pytest_unconfigure")
                    });
                    return exit_code::USAGE_ERROR;
                }
                // Sentinel "\x00USAGE_ERROR\x00": UsageError in configure —
                // run unconfigure and exit 4. May have an error message appended.
                if let Some(inner) = message.strip_prefix("\x00USAGE_ERROR\x00") {
                    if !inner.is_empty() {
                        eprintln!("ERROR during collection:\n{inner}");
                    }
                    let _ = flush_hook_output(py, || {
                        self.fire_py_hooks_simple(py, "pytest_unconfigure")
                    });
                    return exit_code::USAGE_ERROR;
                }
                // Sentinel "\x00EXIT\x00{code}": pytest.exit() during configure or
                // sessionstart — banner already set on session if needed.
                if let Some(rest) = message.strip_prefix("\x00EXIT\x00") {
                    let code = rest.parse().unwrap_or(exit_code::INTERRUPTED);
                    if let Some(banner) = &self.session.abort_banner.clone() {
                        println!("{}", center_with(banner, '!'));
                    }
                    let _ = flush_hook_output(py, || {
                        self.fire_py_hooks_simple(py, "pytest_unconfigure")
                    });
                    return code;
                }
                eprintln!("ERROR: {message}");
                return exit_code::USAGE_ERROR;
            }
        };
        // Collection done: tests (and gc-dependent plugins) run from here on.
        python::set_gc_enabled(py, true);
        let n_collect_errors = collect_errors.len();

        // A plugin's pytest_cmdline_main hookimpl claimed the whole run
        // (e.g. pytest-bdd's --generate-missing) — it already did its own
        // collection/reporting via session.perform_collect(), so skip
        // deselection/running/summary entirely and exit with its code.
        if let Some(code) = self.session.cmdline_main_exit {
            // The plugin's own printing (e.g. pytest-bdd's TerminalWriter
            // output) went through the session-wide native capture that was
            // still installed while pytest_cmdline_main fired during
            // collect() — release it now (same call the normal end-of-run
            // path makes) or it stays buffered until some later,
            // unrelated capture_session_end and surfaces on the wrong
            // stream (e.g. an outer nested run's own captured stdout).
            python::capture_session_end(py);
            // Real pytest's wrap_session fires pytest_sessionfinish, whose
            // terminal reporter prints the closing "==== ... in X.XXs ===="
            // line even when nothing ran (a plugin's own Session never ran
            // the test protocol) — pytester's assert_outcomes needs that
            // duration-bearing line to parse a (here, all-zero) outcome
            // count instead of raising "summary report not found".
            let summary = crate::runner::summary_line(
                &[],
                0,
                0,
                started.elapsed(),
                self.config.global_verbosity(),
            );
            if !summary.is_empty() {
                println!("{summary}");
            }
            let _ = flush_hook_output(py, || self.fire_py_hooks_simple(py, "pytest_unconfigure"));
            return code;
        }

        // --markers / -h,--help (and similar early-exit modes) printed their
        // output during collect and skipped item collection.  Return OK now
        // — falling through would reach handle_no_tests() → exit 5 ("no
        // tests ran"). A --help combined with a UsageError never reaches
        // here at all (collect() returns Err, handled above).
        if self.config.get_flag("markers") || self.config.help_text.is_some() {
            let _ = flush_hook_output(py, || self.fire_py_hooks_simple(py, "pytest_unconfigure"));
            return exit_code::OK;
        }

        let collect_code = self.handle_collection_errors(py, collect_errors, started);
        // Explicit node-id args that matched nothing force USAGE_ERROR even
        // when collection errors were reported (and even when there were
        // none) — upstream returns exit 4 here. #134
        if !self.session.not_found_nodeids.is_empty() {
            return exit_code::USAGE_ERROR;
        }
        if let Some(code) = collect_code {
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
        if let Err(err) = self.check_pending_hooks(py) {
            // Upstream pytest_internalerror: the traceback goes to the
            // terminal (stdout), each line prefixed "INTERNALERROR> ".
            for line in python::format_exception(py, &err).lines() {
                println!("INTERNALERROR> {line}");
            }
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
        if let Err(err) = self.fire_deferred_modifyitems_wrappers(py) {
            eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
            return exit_code::INTERNAL_ERROR;
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
        // In xdist mode, workers collect modules themselves (controller skips
        // collect_module). The actual count comes from the merge loop, so
        // suppress the "collected N items" line here to avoid "collected 0 items".
        // Exception: --collect-only bypasses the merge loop and collects locally,
        // so the count is available now and must be printed here.
        #[cfg(feature = "xdist")]
        let in_dist_mode = (self.config.numprocesses_spec().is_some()
            || self.config.get_flag("dist-load")
            || self.config.get_value("tx").is_some())
            && !self.config.collect_only;
        #[cfg(not(feature = "xdist"))]
        let in_dist_mode = false;

        if !in_dist_mode {
            self.print_collection_count(py, collected, n_collect_errors, n_collect_skips, n_items);
        }

        // --fixtures / --fixtures-per-test: like --collect-only, collect then
        // print (fixtures rather than the item tree) and exit without running.
        if self.config.get_flag("fixtures") {
            if let Err(err) = self.show_fixtures(py) {
                eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
            }
            let _ = flush_hook_output(py, || self.fire_py_hooks_simple(py, "pytest_unconfigure"));
            return exit_code::OK;
        }
        if self.config.get_flag("fixtures-per-test") {
            if let Err(err) = self.show_fixtures_per_test(py) {
                eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
            }
            let _ = flush_hook_output(py, || self.fire_py_hooks_simple(py, "pytest_unconfigure"));
            return exit_code::OK;
        }

        if self.config.collect_only {
            return self.run_collect_only(py, started, n_collect_errors, n_items);
        }

        // A conftest's own plain pytest_runtestloop hookimpl (firstresult,
        // like pytest_runtest_protocol) may fully replace item-running —
        // dispatch it before the native loop, matching upstream's
        // `config.hook.pytest_runtestloop(session=session)` call in `_main`.
        // Only the item-running step is skipped when one returns non-None;
        // session-finish/no-tests handling below still runs unconditionally,
        // matching upstream (that happens in `wrap_session`, outside
        // pytest_runtestloop itself).
        let loop_replaced = match flush_hook_output(py, || self.fire_py_runtestloop(py)) {
            Ok(replaced) => replaced,
            Err(err) => {
                eprintln!("INTERNAL ERROR: {}", python::format_exception(py, &err));
                false
            }
        };

        #[cfg(feature = "xdist")]
        match self.resolve_numprocesses(py) {
            Some(workers) => {
                // In dist mode workers collect themselves — n_items may be 0 here.
                if !loop_replaced {
                    self.run_dist(py, workers);
                }
            }
            None => {
                if n_items == 0 {
                    let plugin_reports = python::drain_plugin_reports(py);
                    self.session.reports.extend(plugin_reports);
                    return self.handle_no_tests(py, started);
                } else if !loop_replaced {
                    self.run_items(py);
                }
            }
        }
        #[cfg(not(feature = "xdist"))]
        {
            if n_items == 0 {
                let plugin_reports = python::drain_plugin_reports(py);
                self.session.reports.extend(plugin_reports);
                return self.handle_no_tests(py, started);
            } else if !loop_replaced {
                self.run_items(py);
            }
        }

        if let Some(code) = self.session.internal_error_exit_code {
            // A replacing pytest_runtest_protocol hookimpl raised mid-run
            // (INTERNALERROR): the banner, pytest_internalerror dispatch, and
            // junit "internal" node already ran in run_one. Mirror
            // wrap_session's finally block (end capture, write --junitxml,
            // run unconfigure) but skip the normal pass/fail terminal summary
            // — upstream's terminal reporter prints no stats line here either.
            python::capture_session_end(py);
            self.write_junit_xml(py);
            let _ = flush_hook_output(py, || self.fire_py_hooks_simple(py, "pytest_unconfigure"));
            return code;
        }

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
        if let Err(err) = flush_hook_output(py, || self.fire_py_sessionfinish(py, code)) {
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
            self.print_short_summary(py);
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
        } else if self.session.dist_total_items == Some(0) {
            exit_code::NO_TESTS_COLLECTED
        } else {
            exit_code::OK
        };

        let mut session_exited = false;
        if let Err(err) = flush_hook_output(py, || self.fire_sessionfinish(py, code)) {
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
        // A KeyboardInterrupt's richer repr prints later, after the summary
        // (below) — skip this immediate pytest.exit()-style banner for it.
        if let Some(banner) = &self.session.abort_banner
            && self.session.keyboard_interrupt_repr.is_none()
        {
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
            self.print_pastebin_failed(py);
        }
        self.write_junit_xml(py);
        if let Some(banner) = &self.session.dist_banner {
            println!("{}", center_banner(banner));
        }
        if !no_summary {
            self.print_short_summary(py);
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
        // A KeyboardInterrupt during a test: upstream's
        // _report_keyboardinterrupt prints its banner + crash info here, at
        // the very end (after the whole summary), not with the other abort
        // banners printed earlier.
        if let Some((short, long)) = &self.session.keyboard_interrupt_repr {
            println!("{}", center_with("KeyboardInterrupt", '!'));
            if self.config.get_flag("full-trace") {
                println!("{long}");
            } else {
                println!("{short}");
                println!(
                    "{}",
                    crate::tw::markup(
                        "(to show a full traceback on KeyboardInterrupt use --full-trace)",
                        &[crate::tw::YELLOW]
                    )
                );
            }
        }
        let warning_count = python::warning_count(py) + self.session.worker_warning_count;
        let extra_stats = python::reporter_subtest_stats(py);
        // Hide non-failed subtests only for pytest 9's builtin subtests
        // (verbosity_subtests == 0); the third-party pytest-subtests plugin
        // has no quiet mode and always counts them.
        let hide_subtests = self.config.quiet_subtests() && !python::has_subtests_plugin(py);
        let summary = crate::runner::summary_line_with_extras(
            &self.session.reports,
            self.session.deselected,
            warning_count,
            started.elapsed(),
            self.config.global_verbosity(),
            &extra_stats,
            hide_subtests,
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
        // pytest_unconfigure: mirrors upstream's config teardown (fired after
        // the terminal summary, just before the session returns). conftest and
        // plugin hooks observe it; in pytester inline runs the HookRecorder's
        // getcalls("pytest_unconfigure") sees the live config via record_hook.
        // Must run before threadexception_session_cleanup below: upstream's
        // own threadexception cleanup lives on the SAME `add_cleanup` stack
        // drained here, after any test-registered cleanup (LIFO) — a thread
        // spawned by a `request.config.add_cleanup()` callback (as opposed to
        // one spawned during a test's setup/call/teardown, drained per-phase
        // in runner/item/body.rs) only exists once this drain runs, so
        // checking for it any earlier always finds nothing.
        let _ = self.fire_py_hooks_simple(py, "pytest_unconfigure");
        if let Err(err) = python::threadexception_session_cleanup(py) {
            eprintln!("{}", python::format_exception(py, &err));
            // Unlike a thread exception discovered during a test's own
            // setup/call/teardown (drained per-phase, becoming a normal test
            // failure — TESTS_FAILED), anything still here happened after
            // every test's own reporting window already closed — genuinely
            // unattributable to any test, matching upstream's real behavior
            // of an uncaught exception escaping session teardown entirely
            // (an INTERNALERROR, not a test outcome, and overriding whatever
            // exit status the test run itself produced).
            code = crate::report::exit_code::INTERNAL_ERROR;
        }
        code
    }
}
