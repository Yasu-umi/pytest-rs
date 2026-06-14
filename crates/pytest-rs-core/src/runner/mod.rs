use std::io::Write as _;

use pyo3::prelude::*;

use crate::collect::TestItem;
use crate::engine::Engine;
use crate::fixture::Scope;
use crate::python;
use crate::report::{Outcome, Phase, TestReport};
use crate::session::Session;

mod fixtures;
mod hooks;
mod item;
mod marks;
mod progress;
mod protocol;
mod teardown;

pub(crate) use fixtures::*;
pub(crate) use hooks::*;
pub(crate) use item::*;
pub(crate) use marks::*;
pub use progress::*;
pub(crate) use protocol::{capture_logreport, report_from_proxy, run_item_phases};
pub(crate) use teardown::setup_show_active;
pub(crate) use teardown::*;

impl Engine {
    pub(crate) fn run_items(&mut self, py: Python<'_>) {
        let Self {
            plugins,
            session,
            config,
            ..
        } = self;

        let items = std::mem::take(&mut session.items);
        let total = items.len().max(1);
        // Per-test progress display level (pytest's VERBOSITY_TEST_CASES):
        // >=1 → a line per test, ==0 → chars grouped under a file header,
        // <0 → bare chars with no file header.
        let tc = config.test_case_verbosity();
        // console_output_style progress field, and the per-file call-duration
        // accumulator that feeds it under the "times" style.
        let pkind = config.progress_kind();
        let mut file_dur = std::time::Duration::ZERO;
        let mut done = 0usize;
        let mut prev_module: Option<String> = None;
        let mut prev_class: Option<String> = None;
        let mut current_file = String::new();
        let mut line = String::new();
        // --setup-only prints no progress chars at all; pytest then also
        // omits the closing "[100%]" fill on the narration line.
        let mut any_char = false;
        let maxfail = config.maxfail();
        // --stepwise stops after the first failing item (--stepwise-skip
        // ignores the first one); the resume point persists via the cache.
        let stepwise =
            (config.get_flag("sw") || config.get_flag("sw-skip") || config.get_flag("sw-reset"))
                && maxfail.is_none();
        let sw_skip = config.get_flag("sw-skip");
        let mut sw_failed_items = 0usize;
        // Quiet subtest verbosity: non-failed subtests don't show progress
        // chars or verbose lines (but still count in the summary).
        let quiet_subtests = config
            .get_ini("verbosity_subtests")
            .map(|v| v.trim() == "0")
            .unwrap_or(config.verbose == 0);
        // With -s (no capture) and non-verbose mode, subtest progress chars
        // must be printed inline from Python's __exit__ so they interleave
        // with the test's own stdout (upstream fires logreport inline).
        let inline_sub_chars =
            !config.no_terminal() && tc <= 0 && !session.live_logging && config.capture_disabled();
        python::set_subtest_inline_chars(py, inline_sub_chars);
        // Collection errors (--continue-on-collection-errors) already count
        // toward the --maxfail budget, like pytest's session.testsfailed.
        let mut failed = session
            .reports
            .iter()
            .filter(|r| r.outcome == Outcome::Failed)
            .count();

        // The last completed item: deferred scope-teardown failures report
        // under it, like pytest where those finalizers run inside it.
        let mut last_nodeid: Option<String> = None;
        // A failing deferred teardown becomes an ERROR report: count it
        // toward --maxfail and join its E to the previous progress chars.
        macro_rules! report_scope_teardown {
            ($scope:expr, $prev:expr, $item:expr) => {
                if let Some(report) = teardown_scope_reported(
                    py,
                    plugins,
                    session,
                    config,
                    $scope,
                    $prev,
                    $item,
                    last_nodeid.as_deref(),
                ) {
                    if report.outcome == Outcome::Failed {
                        fire_logreport_hooks(py, session, &report, None, None, false);
                        failed += 1;
                        if !config.no_terminal()
                            && tc <= 0
                            && !session.live_logging
                            && !line.is_empty()
                        {
                            print!("E");
                            let _ = std::io::stdout().flush();
                            line.push('E');
                        }
                        session.reports.push(report);
                    } else if let Some(existing) = session
                        .reports
                        .iter_mut()
                        .rev()
                        .find(|r| r.phase == Phase::Teardown && r.nodeid == report.nodeid)
                    {
                        // A passing scope teardown's captured output belongs on
                        // the item's existing teardown report (pytest runs these
                        // finalizers inside that teardown); merge to avoid a
                        // duplicate report skewing junit/counts.
                        existing.sections.extend(report.sections);
                    } else {
                        session.reports.push(report);
                    }
                }
            };
        }

        for idx in 0..items.len() {
            let item = &items[idx];
            if let Some(m) = maxfail
                && failed >= m
            {
                break;
            }
            // pytest.exit / Ctrl-C inside a test aborts the session.
            if session.exit_code_override.is_some() {
                break;
            }

            let class_instance = item.class_instance();
            if let Some(prev) = &prev_class
                && prev != &class_instance
            {
                report_scope_teardown!(Scope::Class, prev, item);
            }
            prev_class = Some(class_instance);

            let module_instance = item.module_instance();
            if let Some(prev) = &prev_module
                && prev != &module_instance
            {
                report_scope_teardown!(Scope::Module, prev, item);
                // Package-scoped fixtures are keyed per module instance.
                report_scope_teardown!(Scope::Package, prev, item);
            }
            prev_module = Some(module_instance);

            let file = item
                .nodeid
                .split_once("::")
                .map(|(f, _)| f.to_string())
                .unwrap_or_else(|| item.nodeid.clone());
            if tc == 0 && !config.no_terminal() && !session.live_logging && file != current_file {
                if !current_file.is_empty() {
                    let msg = progress_message(pkind, done, total, file_dur);
                    println!(
                        "{}",
                        progress_suffix(&line, &msg, fill_color(py, session, false))
                    );
                }
                file_dur = std::time::Duration::ZERO;
                line = format!("{file} ");
                print!("{line}");
                let _ = std::io::stdout().flush();
                current_file = file;
            }

            // log_cli: the item header prints on its own line up front so
            // live log records appear under it; pytest_runtest_logstart
            // hooks log under a "live log start" section.
            if session.live_logging {
                if !config.no_terminal() && !config.quiet && config.verbose == 0 {
                    println!("{} ", item.nodeid);
                    let _ = std::io::stdout().flush();
                }
                session.live_progress = Some((done + 1, total));
                python::log_set_live_when(py, "start");
            }
            let _ = fire_runtest_py_hooks(py, session, item, "pytest_runtest_logstart");
            if !config.is_worker() {
                python::reporter_logstart(py, item);
            }

            // Failed subtests share the --maxfail budget: tell the fixture
            // how many failures remain before it must stop swallowing.
            python::set_subtest_fail_budget(py, maxfail.map(|m| m.saturating_sub(failed)));
            session.live_printed = 0;
            session.streamed_chars = 0;
            let reports = run_one(py, plugins, session, config, item, items.get(idx + 1));
            if inline_sub_chars {
                let inline = python::pop_subtest_inline_count(py);
                if inline > 0 {
                    session.streamed_chars = reports
                        .iter()
                        .enumerate()
                        .filter(|(_, r)| r.subtest_desc.is_some())
                        .take(inline)
                        .last()
                        .map(|(i, _)| i + 1)
                        .unwrap_or(0);
                }
            }
            // A delegated pytest_runtest_protocol drove the shim TerminalReporter
            // (via ihook), which already rendered; here we only count.
            let delegated = session.delegated_render;
            if !delegated {
                live_flush(session, config, &reports);
            }
            done += 1;
            last_nodeid = Some(item.nodeid.clone());
            let mut item_failed = false;
            for (i, report) in reports.into_iter().enumerate() {
                fire_logreport_hooks(
                    py,
                    session,
                    &report,
                    Some(item.lineno),
                    Some(item),
                    delegated,
                );
                // A "rerun" report is a retried attempt: shown as 'R', never
                // counted as a failure or charged against --maxfail.
                if report.outcome == Outcome::Failed && !report.rerun {
                    failed += 1;
                    item_failed = true;
                }
                // Accumulate call-phase time for the current file's "times"
                // progress field.
                if report.phase == Phase::Call {
                    file_dur += report.duration;
                }
                if report.progress_char().is_some() {
                    any_char = true;
                }
                let is_quiet_sub = quiet_subtests
                    && report.subtest_desc.is_some()
                    && report.outcome != Outcome::Failed;
                if config.no_terminal() || delegated {
                    // -p no:terminal, or a delegated protocol whose shim
                    // TerminalReporter already rendered: no native output.
                } else if is_quiet_sub {
                    // Quiet subtest: counted in the summary but not displayed.
                } else if tc >= 1 {
                    python::reporter_ensure_newline(py);
                    print_verbose_report_line(
                        py, config, session, item, &report, done, total, tc, pkind,
                    );
                } else if session.live_logging && !config.quiet {
                    // log_cli: outcome words print via live_flush (between
                    // the call phase and teardown logs).
                } else if i < session.streamed_chars {
                    // --setup-show already streamed this report's char
                    // (between the item line and the TEARDOWN narration).
                } else if tc <= 0
                    && let Some(c) = report.progress_char()
                {
                    print!(
                        "{}",
                        crate::tw::markup(&c.to_string(), outcome_codes(&report))
                    );
                    let _ = std::io::stdout().flush();
                    line.push(c);
                }
                session.reports.push(report);
            }
            if session.live_logging {
                python::log_set_live_when(py, "finish");
            }
            let _ = fire_runtest_py_hooks(py, session, item, "pytest_runtest_logfinish");
            if !config.is_worker() {
                python::reporter_logfinish(py, item);
            }
            if stepwise && item_failed {
                sw_failed_items += 1;
                if !(sw_skip && sw_failed_items == 1) {
                    // Publish a truthy session.shouldstop so a conftest
                    // pytest_sessionfinish sees it (and cannot unset it).
                    python::set_session_shouldstop(
                        py,
                        "Test failed, continuing from this test next run.",
                    );
                    break;
                }
            }
            // Parametrized session-scope fixture boundary: when the next item
            // does not use the same (fixture, param_index) as the current one,
            // the parametrized session-scope variant is exhausted and should be
            // torn down before the next test sets up the new variant.
            {
                let nextitem = items.get(idx + 1);
                for (fixture_name, param_idx, _) in &item.fixture_params {
                    if let Some(def) = session.registry.lookup(fixture_name, &item.nodeid) {
                        if def.scope == Scope::Session {
                            let next_uses_same = nextitem
                                .map(|next| {
                                    next.fixture_params.iter().any(|(n, i, _)| {
                                        n == fixture_name && i == param_idx
                                    })
                                })
                                .unwrap_or(false);
                            if !next_uses_same {
                                let instance = format!(
                                    "\x00session_param:{}:{}",
                                    fixture_name, param_idx
                                );
                                report_scope_teardown!(Scope::Session, &instance, item);
                            }
                        }
                    }
                }
            }
            // Plugin-set session.shouldfail (pytest-timeout's session
            // deadline) aborts the run with its message banner.
            if let Some(msg) = python::session_shouldfail(py) {
                session.shouldfail = Some(msg);
                break;
            }
        }
        // Final scope teardowns, before the progress line closes so a
        // failing teardown's E joins the last test's progress chars.
        if let Some(prev) = &prev_class.clone()
            && let Some(last) = items.last()
        {
            report_scope_teardown!(Scope::Class, prev, last);
        }
        if let Some(prev) = &prev_module.clone()
            && let Some(last) = items.last()
        {
            report_scope_teardown!(Scope::Module, prev, last);
            report_scope_teardown!(Scope::Package, prev, last);
        }
        if let Some(last) = items.last() {
            report_scope_teardown!(Scope::Session, "", last);
        }
        if tc <= 0
            && !config.no_terminal()
            && !session.live_logging
            && !line.is_empty()
            && (!setup_show_active(config) || any_char)
        {
            let msg = progress_message(pkind, done, total, file_dur);
            println!(
                "{}",
                progress_suffix(&line, &msg, fill_color(py, session, true))
            );
        }
        // pytest prints the banner even when the budget was spent on the
        // very last test, so check the final count rather than the break.
        if let Some(m) = maxfail
            && failed >= m
        {
            session.stopped_after = Some(failed);
            // Publish a truthy session.shouldfail for conftest
            // pytest_sessionfinish (upstream sets the stop message here).
            python::set_session_shouldfail(py, &format!("stopping after {failed} failures"));
        }

        session.items = items;
    }
}

/// Verbose (`-v`+) per-item terminal line: the outcome word (honoring a
/// pytest_report_teststatus override), a trimmed/wrapped reason, the
/// location suffix, and the right-aligned progress field.
#[allow(clippy::too_many_arguments)]
fn print_verbose_report_line(
    py: Python<'_>,
    config: &crate::config::Config,
    session: &Session,
    item: &TestItem,
    report: &TestReport,
    done: usize,
    total: usize,
    tc: i32,
    pkind: crate::config::ProgressKind,
) {
    if !(report.phase == Phase::Call || report.outcome != Outcome::Passed) {
        return;
    }
    // Subtest reports use the built-in word (the report proxy isn't a
    // SubTestReport so Python hooks return the generic PASSED/FAILED).
    let status = if report.subtest_desc.is_some() {
        None
    } else {
        report_teststatus(py, config, session, report, Some(item.lineno))
    };
    let (word, reason) = match &status {
        Some(s) => {
            let reason = match report.outcome {
                Outcome::Skipped | Outcome::XFailed | Outcome::XPassed => report
                    .longrepr
                    .clone()
                    .filter(|r| !r.is_empty() && !r.contains('\n')),
                _ => None,
            };
            (s.word.clone(), reason)
        }
        None => verbose_outcome(report),
    };
    let codes = status
        .as_ref()
        .and_then(|s| s.markup.clone())
        .unwrap_or_else(|| outcome_codes(report).to_vec());
    // pytest's _locationline appends " <- <src>" when the
    // test's code has no real source file — an exec'd def
    // reports co_filename "<string>". Real files (including
    // wrapper shims like _unittest.py, whose co_filename is
    // not the test module) are not flagged.
    let loc_suffix = item
        .func
        .bind(py)
        .getattr("__code__")
        .and_then(|c| c.getattr("co_filename"))
        .and_then(|f| f.extract::<String>())
        .ok()
        .filter(|co| !std::path::Path::new(co).is_file())
        .map(|co| format!(" <- {co}"))
        .unwrap_or_default();
    let prefix = format!("{}{} {}", item.nodeid, loc_suffix, word);
    let reason_suffix = match &reason {
        Some(r) => python::format_verbose_reason(py, prefix.chars().count(), r, tc, term_width()),
        None => String::new(),
    };
    let plain = format!("{prefix}{reason_suffix}");
    let rendered = format!(
        "{}{} {}{reason_suffix}",
        item.nodeid,
        loc_suffix,
        crate::tw::markup(&word, &codes)
    );
    // The progress field right-aligns against the last line
    // (a -vv reason may wrap across several).
    let last_line = plain.rsplit('\n').next().unwrap_or(&plain);
    // "times" in verbose mode reports each test's own
    // duration (pytest's per-item showlongtestinfo).
    let msg = progress_message(pkind, done, total, report.duration);
    println!(
        "{rendered}{}",
        progress_suffix(last_line, &msg, fill_color(py, session, done == total))
    );
    let _ = std::io::stdout().flush();
}
