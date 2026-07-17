use std::io::Write as _;
use std::time::Duration;

use pyo3::prelude::*;

use crate::collect::TestItem;
use crate::engine::Engine;
use crate::fixture::Scope;
use crate::python;
use crate::report::{Outcome, Phase, TestReport};
use crate::session::Session;

/// Timing mark using Rust's monotonic clock (`std::time::Instant`).
///
/// ## Why not `_pytest.timing.perf_counter`?
///
/// The original implementation called `_pytest.timing.perf_counter` through
/// PyO3 on every mark so that pytest's `mock_timing` fixture could intercept
/// the clock.  That adds ~3 PyO3 boundary-crossings per call; with 4–5 marks
/// per test item the overhead accumulated to ~10 µs/test × N tests.
///
/// Switching to `Instant` eliminates those Python round-trips entirely.
///
/// ## Why `mock_timing` conformance tests are unaffected
///
/// pytest's `TestDurations` tests (in `testing/acceptance_test.py`) patch
/// `_pytest.timing.perf_counter` via `mock_timing` and then run inner pytest
/// sessions via `pytester.runpytest_inprocess()`.  That call invokes vanilla
/// `pytest.main()` in the same process — the *inner* session is real pytest,
/// not pytest-rs.  Vanilla pytest still reads `_pytest.timing.perf_counter`
/// (which is mocked), so the `--durations` output remains correct and those
/// tests continue to pass.  pytest-rs's `TimeMark` only affects the *outer*
/// session's `TestReport.duration` values, which no conformance test asserts
/// exact values for.
#[derive(Clone, Copy)]
pub(crate) struct TimeMark(std::time::Instant);

impl TimeMark {
    pub(crate) fn now() -> Self {
        Self(std::time::Instant::now())
    }

    pub(crate) fn elapsed(self) -> Duration {
        self.0.elapsed()
    }
}

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
        let mut prev_package: Option<String> = None;
        // Package-scope fixture instance (the module's directory), tracked
        // separately from prev_module — a package-scoped fixture must stay
        // cached across every module in the same directory, only tearing
        // down when the run actually crosses into a different directory.
        let mut prev_pkg_instance: Option<String> = None;
        let mut current_file = String::new();
        let mut line = String::new();
        let maxfail = config.maxfail();
        // --stepwise stops after the first failing item (--stepwise-skip
        // ignores the first one); the resume point persists via the cache.
        let stepwise =
            (config.get_flag("sw") || config.get_flag("sw-skip") || config.get_flag("sw-reset"))
                && maxfail.is_none();
        let sw_skip = config.get_flag("sw-skip");
        let mut sw_failed_items = 0usize;
        // Quiet subtest verbosity: non-failed subtests don't show progress
        // chars, verbose lines, or count in the summary (mirrors upstream's
        // subtests plugin returning ("", "", "") under verbosity_subtests == 0).
        let quiet_subtests = config.quiet_subtests();
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
        macro_rules! handle_teardown_report {
            ($report:expr) => {
                if let Some(report) = $report {
                    if report.outcome == Outcome::Failed {
                        // A failing scope teardown (class/module/package) runs
                        // inside the last item's teardown phase, like pytest.
                        // If that item already has a passing teardown report,
                        // upgrade it in-place rather than adding a second
                        // teardown report for the same nodeid (which would break
                        // tests asserting exactly one teardown report per item).
                        let upgrade_idx = session.reports.iter().rposition(|r| {
                            r.phase == Phase::Teardown
                                && r.nodeid == report.nodeid
                                && r.outcome == Outcome::Passed
                        });
                        if let Some(idx) = upgrade_idx {
                            {
                                let existing = &mut session.reports[idx];
                                existing.outcome = Outcome::Failed;
                                existing.longrepr = report.longrepr;
                                existing.sections.extend(report.sections);
                            }
                            // The deferred teardown flush fires the hook after
                            // all scope teardowns; don't fire it a second time here.
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
                        } else {
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
                        }
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
        macro_rules! report_scope_teardown {
            ($scope:expr, $prev:expr, $item:expr) => {
                handle_teardown_report!(teardown_scope_reported(
                    py,
                    plugins,
                    session,
                    config,
                    $scope,
                    $prev,
                    $item,
                    last_nodeid.as_deref(),
                ))
            };
        }
        // A non-function-scope parametrization moved to its next value within
        // the same scope-instance: tear down (LIFO) the fixtures depending on
        // it before the next item sets the new value up.
        macro_rules! report_param_teardown {
            ($ended:expr, $item:expr) => {{
                let ended = $ended;
                if !ended.is_empty() {
                    let report_nodeid = last_nodeid.clone().unwrap_or_else(|| $item.nodeid.clone());
                    handle_teardown_report!(teardown_ended_params_reported(
                        py,
                        session,
                        config,
                        &ended,
                        &report_nodeid,
                    ));
                }
            }};
        }
        // Deferred teardown hook: the teardown report's logreport hook fires
        // AFTER all scope (class/module/package) teardowns for the previous
        // item have run, so the hook sees the final (possibly upgraded) outcome.
        // Tuple: (report index in session.reports, item lineno, item index, delegated).
        let mut pending_teardown: Option<(usize, u32, usize, bool)> = None;
        macro_rules! flush_pending_teardown {
            () => {
                if let Some((r_idx, lineno, item_idx, del)) = pending_teardown.take() {
                    fire_logreport_hooks(
                        py,
                        session,
                        &session.reports[r_idx],
                        Some(lineno),
                        items.get(item_idx),
                        del,
                    );
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
            if let Some(prev) = &prev_class {
                if prev != &class_instance {
                    report_scope_teardown!(Scope::Class, prev, item);
                } else if idx > 0 {
                    // Same class node, but a class-scoped param advanced.
                    report_param_teardown!(
                        items[idx - 1].ended_param_bindings(py, item, &[Scope::Class]),
                        item
                    );
                }
            }
            prev_class = Some(class_instance);

            let module_instance = item.module_instance();
            if let Some(prev) = &prev_module {
                if prev != &module_instance {
                    report_scope_teardown!(Scope::Module, prev, item);
                } else if idx > 0 {
                    // Same module node, but a module-scoped param advanced.
                    report_param_teardown!(
                        items[idx - 1].ended_param_bindings(py, item, &[Scope::Module]),
                        item
                    );
                }
            }
            prev_module = Some(module_instance);

            let pkg_instance = item.package_instance();
            if let Some(prev) = &prev_pkg_instance {
                if prev != &pkg_instance {
                    report_scope_teardown!(Scope::Package, prev, item);
                } else if idx > 0 {
                    // Same package directory, but a package-scoped param advanced.
                    report_param_teardown!(
                        items[idx - 1].ended_param_bindings(py, item, &[Scope::Package]),
                        item
                    );
                }
            }
            prev_pkg_instance = Some(pkg_instance);

            let package = item
                .module_name
                .rsplit_once('.')
                .map(|(p, _)| p.to_string());
            if prev_package != package
                && let Some(prev_pkg) = &prev_package
            {
                report_scope_teardown!(Scope::Module, prev_pkg, item);
            }
            prev_package = package;

            // Fire the previous item's deferred teardown hook now that all
            // scope teardowns have run and the report reflects the final outcome.
            flush_pending_teardown!();

            let file = item
                .nodeid
                .split_once("::")
                .map(|(f, _)| f.to_string())
                .unwrap_or_else(|| item.nodeid.clone());
            if tc == 0 && !config.no_terminal() && !session.live_logging && file != current_file {
                if !current_file.is_empty() {
                    if setup_show_active(config) {
                        // --setup-plan/-show/-only narration lines don't end
                        // with a newline (so a same-line outcome char could
                        // normally follow); close the line here instead of
                        // the percent-progress field real dot-progress uses.
                        println!();
                    } else {
                        let msg = progress_message(pkind, done, total, file_dur);
                        println!(
                            "{}",
                            progress_suffix(&line, &msg, fill_color(py, session, false))
                        );
                    }
                }
                file_dur = std::time::Duration::ZERO;
                // Display the file path like pytest's write_fspath_result:
                // bestrelpath(startpath, rootdir / nodeid_file_part).
                // For files inside rootdir this is simply the invocation-dir-
                // relative path.  For files outside rootdir pytest builds a
                // "virtual" path by prepending rootdir to the invocation-dir-
                // relative file name, yielding e.g. "root/test_foo.py" when
                // rootdir=…/root and the file lives one level above it.
                let display_file = crate::collect::display_file_path(
                    &config.rootdir,
                    &config.invocation_dir,
                    &item.path,
                );
                line = format!("{display_file} ");
                print!("{line}");
                let _ = std::io::stdout().flush();
                current_file = file;
            }

            // With -v and no capture, print "nodeid " before the test runs so
            // the test's own stdout appears after the ID line, then the outcome
            // word prints on its own line — matching upstream pytest's format.
            let pre_printed_id = tc >= 1
                && !config.no_terminal()
                && config.capture_disabled()
                && !session.live_logging
                && !config.is_worker();
            if pre_printed_id {
                python::reporter_ensure_newline(py);
                print!("{} ", item.nodeid);
                let _ = std::io::stdout().flush();
            }

            // Failed subtests share the --maxfail budget: tell the fixture
            // how many failures remain before it must stop swallowing.
            python::set_subtest_fail_budget(py, maxfail.map(|m| m.saturating_sub(failed)));
            session.live_printed = 0;
            session.streamed_chars = 0;
            let reports = run_one(
                py,
                plugins,
                session,
                config,
                item,
                items.get(idx + 1),
                None,
                |py, session, config, item| {
                    // log_cli: the item header prints on its own line up front
                    // so live log records appear under it; pytest_runtest_logstart
                    // hooks log under a "live log start" section. Only reached
                    // on the native protocol path (see run_one's on_native_start
                    // doc) — a replacing plain pytest_runtest_protocol hookimpl
                    // owns its own nodeid-print/logstart, matching upstream.
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
                },
            );
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
                let is_teardown = report.phase == Phase::Teardown;
                if !is_teardown {
                    fire_logreport_hooks(
                        py,
                        session,
                        &report,
                        Some(item.lineno),
                        Some(item),
                        delegated,
                    );
                }
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
                let is_quiet_sub = quiet_subtests
                    && report.subtest_desc.is_some()
                    && report.outcome != Outcome::Failed;
                if config.no_terminal() || delegated {
                    // -p no:terminal, or a delegated protocol whose shim
                    // TerminalReporter already rendered: no native output.
                } else if is_quiet_sub {
                    // Quiet subtest: not displayed (and not counted in the
                    // summary — see summary_line_with_extras).
                } else if tc >= 1 {
                    python::reporter_ensure_newline(py);
                    print_verbose_report_line(
                        py,
                        config,
                        session,
                        item,
                        &report,
                        done,
                        total,
                        tc,
                        pkind,
                        pre_printed_id,
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
                    // Mid-line edge wrap (pytest's _write_progress_information_
                    // if_past_edge): when the line plus the reserved progress
                    // field would overflow the terminal, emit the field (empty
                    // for "times", which shows the duration only at file end)
                    // and continue the dots on a fresh line. Skip the last
                    // collected item — the final 100%/duration flush handles it.
                    if idx + 1 < items.len()
                        && let Some(msg) = edge_wrap_message(
                            pkind,
                            line.chars().count(),
                            term_width(),
                            done,
                            total,
                            file_dur,
                        )
                    {
                        let color = fill_color(py, session, false);
                        if msg.is_empty() {
                            println!();
                        } else {
                            println!("{}", crate::tw::markup(&msg, &[color]));
                        }
                        line.clear();
                    }
                }
                if is_teardown {
                    pending_teardown = Some((session.reports.len(), item.lineno, idx, delegated));
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
            // does not use the same (fixture, param_value) as the current one,
            // the parametrized session-scope variant is exhausted and should be
            // torn down before the next test sets up the new variant.
            {
                let nextitem = items.get(idx + 1);
                for (fixture_name, _, cur_value) in &item.fixture_params {
                    if let Some(def) = session.registry.lookup(fixture_name, &item.nodeid)
                        && def.scope == Scope::Session
                    {
                        let cur_repr = cur_value
                            .bind(py)
                            .repr()
                            .map(|r| r.to_string())
                            .unwrap_or_default();
                        let next_uses_same = nextitem
                            .map(|next| {
                                next.fixture_params.iter().any(|(n, _, v)| {
                                    n == fixture_name
                                        && v.bind(py)
                                            .repr()
                                            .map(|r| r.to_string())
                                            .unwrap_or_default()
                                            == cur_repr
                                })
                            })
                            .unwrap_or(false);
                        if !next_uses_same {
                            let instance =
                                format!("\x00session_param:{}:{}", fixture_name, cur_repr);
                            report_scope_teardown!(Scope::Session, &instance, item);
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
        }
        if let Some(prev) = &prev_pkg_instance.clone()
            && let Some(last) = items.last()
        {
            report_scope_teardown!(Scope::Package, prev, last);
        }
        if let Some(prev) = &prev_package.clone()
            && let Some(last) = items.last()
        {
            report_scope_teardown!(Scope::Module, prev, last);
        }
        if let Some(last) = items.last() {
            report_scope_teardown!(Scope::Session, "", last);
        }
        flush_pending_teardown!();
        if tc <= 0
            && !config.no_terminal()
            && !session.live_logging
            && !line.is_empty()
            && !setup_show_active(config)
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
    pre_printed_id: bool,
) {
    if !(report.phase == Phase::Call || report.outcome != Outcome::Passed) {
        return;
    }
    let status = report_teststatus(py, config, session, report, Some(item.lineno));
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
    // pytest's _locationline appends " <- <src>" in two cases: an exec'd def
    // has no real source file (co_filename "<string>"), or (at -vv) the
    // nodeid's file differs from the file the code actually lives in — e.g.
    // a test method inherited from a base class defined in another module.
    // The second comparison only applies to files under rootdir: internal
    // engine shims (e.g. the unittest wrapper) are real files too but live
    // outside the project tree, so they must never be flagged.
    let loc_suffix = item
        .func
        .bind(py)
        .getattr("__code__")
        .and_then(|c| c.getattr("co_filename"))
        .and_then(|f| f.extract::<String>())
        .ok()
        .map(|co| {
            let co_path = std::path::Path::new(&co);
            if !co_path.is_file() {
                return format!(" <- {co}");
            }
            let canonical =
                std::fs::canonicalize(co_path).unwrap_or_else(|_| co_path.to_path_buf());
            if config.global_verbosity() >= 2 && canonical.starts_with(&config.rootdir) {
                // Upstream's bestrelpath(startpath, Path(fspath)) call looks
                // directory-relative, but fspath is itself rootdir-relative
                // and startpath is absolute — bestrelpath's "both paths must
                // be the same kind" fallback then just returns fspath as-is.
                // The practical result is the plain rootdir-relative path.
                let co_file = crate::collect::file_nodeid(&config.rootdir, &canonical, &[]);
                let item_file = item.nodeid.split("::").next().unwrap_or(&item.nodeid);
                if co_file != item_file {
                    return format!(" <- {co_file}");
                }
            }
            String::new()
        })
        .unwrap_or_default();
    // When the test ID was already printed before the test ran (capture
    // disabled + verbose), emit only the outcome word so test stdout/stderr
    // appears between the ID line and the outcome line — matching upstream
    // pytest's "-v -s" format where PASSED starts on its own line.
    let (plain, rendered) = if pre_printed_id {
        let reason_suffix = match &reason {
            Some(r) => python::format_verbose_reason(py, word.chars().count(), r, tc, term_width()),
            None => String::new(),
        };
        let p = format!("{word}{reason_suffix}");
        let r = format!("{}{reason_suffix}", crate::tw::markup(&word, &codes));
        (p, r)
    } else {
        // Upstream's cwd_relative_nodeid: the displayed nodeid is
        // invocation-dir-relative, not rootdir-relative, when the two
        // differ (e.g. an explicit --rootdir=subdir run from its parent).
        let display_nodeid = crate::collect::cwd_relative_nodeid(
            &config.rootdir,
            &config.invocation_dir,
            &item.nodeid,
        );
        let prefix = format!("{display_nodeid}{loc_suffix} {word}");
        let reason_suffix = match &reason {
            Some(r) => {
                python::format_verbose_reason(py, prefix.chars().count(), r, tc, term_width())
            }
            None => String::new(),
        };
        let p = format!("{prefix}{reason_suffix}");
        let r = format!(
            "{display_nodeid}{loc_suffix} {}{reason_suffix}",
            crate::tw::markup(&word, &codes)
        );
        (p, r)
    };
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
