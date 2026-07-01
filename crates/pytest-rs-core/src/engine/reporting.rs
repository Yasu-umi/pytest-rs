use std::time::Instant;

use pyo3::prelude::*;

use super::super::Engine;
use crate::python;
use crate::report::{Outcome, Phase, exit_code};

use super::center_with;

impl Engine {
    pub(crate) fn handle_collection_errors(
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
                let reprcrash_message = super::short_message(&err)
                    .filter(|message| !message.starts_with("SyntaxError:"));
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
                    reprcrash_message,
                    head_line: None,
                });
            }
            // --maxfail aborting collection exits TESTS_FAILED with a
            // "stopping after N failures" banner; otherwise INTERRUPTED.
            let maxfail_hit = self.config.maxfail().is_some_and(|m| n_collect_errors >= m);
            // --collect-only still lists the items it did collect plus an
            // error count (pytest's "3 tests collected, 1 error"), so it falls
            // through to the collect-only branch like continue-on-errors.
            // Any collection error (not just maxfail-budget ones) aborts the
            // session with INTERRUPTED — --maxfail only counts test failures.
            // Collection stops early when maxfail is hit during scanning, so
            // n_collect_errors is already capped at that budget by Phase 7.
            let should_abort = if self.config.get_flag("continue-on-collection-errors")
                || self.config.collect_only
            {
                false
            } else {
                n_collect_errors > 0
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
                if self.session.custom_reporter.is_some() {
                    let _ = self.apply_selection(py);
                    let n_items = self.session.items.len();
                    let deselected = self.session.deselected_items.len();
                    let collected = n_items + deselected;
                    python::reporter_collection_finish(py, &self.config, collected);
                }
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
                        self.print_short_summary(py);
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
                let code = if !self.session.not_found_nodeids.is_empty() {
                    // Explicit node-id args that matched nothing force
                    // USAGE_ERROR even when collection errors aborted (#134).
                    exit_code::USAGE_ERROR
                } else if maxfail_hit || dist_workers.is_some() {
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
    pub(crate) fn print_collection_count(
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
                line += &format!(" / {n_collect_skips} skipped",);
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
            // With zero items the run never starts; the blank line belongs
            // to handle_no_tests / finish_session (before the summary).
            if n_items > 0 && (self.config.collect_only || self.config.test_case_verbosity() >= 0) {
                println!();
            }
        }
    }

    /// --collect-only: print the collected tree/nodeids/counts and return.
    pub(crate) fn run_collect_only(
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
                    self.print_short_summary(py);
                    let banner = format!(
                        "Interrupted: {n_collect_errors} error{} during collection",
                        if n_collect_errors == 1 { "" } else { "s" }
                    );
                    println!(
                        "{}",
                        crate::tw::markup(&center_with(&banner, '!'), &[crate::tw::RED],)
                    );
                }
                self.print_collect_only_summary(started.elapsed(), n_collect_errors);
            }
        }
        if n_collect_errors > 0 {
            exit_code::INTERRUPTED
        } else if n_items == 0 {
            exit_code::NO_TESTS_COLLECTED
        } else {
            exit_code::OK
        }
    }
}
