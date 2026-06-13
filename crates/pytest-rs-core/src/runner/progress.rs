//! Progress chars, outcome words/colors, summary line, error reports.

#[allow(unused_imports)]
use super::*;
use crate::collect::TestItem;
use crate::config::{Config, ProgressKind};
use crate::fixture::Scope;
use crate::python;
use crate::report::{Outcome, Phase, TestReport};
use crate::session::Session;
use std::time::{Duration, Instant};

/// log_cli live mode: print outcome words for reports not yet printed
/// (the call outcome appears between the call and teardown log sections).
pub(crate) fn live_flush(session: &mut Session, config: &Config, reports: &[TestReport]) {
    if !session.live_logging || config.verbose != 0 || config.quiet || config.no_terminal() {
        session.live_printed = reports.len();
        return;
    }
    let Some((done, total)) = session.live_progress else {
        session.live_printed = reports.len();
        return;
    };
    let kind = config.progress_kind();
    while session.live_printed < reports.len() {
        let report = &reports[session.live_printed];
        session.live_printed += 1;
        if report.phase == Phase::Call || report.outcome != Outcome::Passed {
            let msg = progress_message(kind, done, total, report.duration);
            println!("{}", with_progress(&outcome_word(report), &msg));
            let _ = std::io::stdout().flush();
        }
    }
}

/// SGR codes for a report's outcome (progress letters, verbose words).
pub(crate) fn outcome_codes(report: &TestReport) -> &'static [u8] {
    use crate::tw;
    if report.rerun {
        return &[tw::YELLOW];
    }
    match report.outcome {
        Outcome::Passed => &[tw::GREEN],
        Outcome::Failed => &[tw::RED],
        Outcome::Skipped | Outcome::XFailed | Outcome::XPassed => &[tw::YELLOW],
    }
}

/// The progress-fill / summary main color from the session so far.
pub(crate) fn fill_color(py: Python<'_>, session: &Session, finished: bool) -> u8 {
    let mut failed = 0usize;
    let mut errors = 0usize;
    let mut xpassed = 0usize;
    let mut passed = 0usize;
    for report in &session.reports {
        match (report.phase, report.outcome) {
            (Phase::Call, Outcome::Passed) => passed += 1,
            (Phase::Call, Outcome::Failed) => failed += 1,
            (Phase::Setup | Phase::Teardown, Outcome::Failed) => errors += 1,
            (_, Outcome::XPassed) => xpassed += 1,
            _ => {}
        }
    }
    crate::tw::main_color(
        failed,
        errors,
        python::warning_count(py),
        xpassed,
        passed,
        finished,
    )
}

/// The verbose outcome word for a report: "PASSED", "SKIPPED (why)",
/// "[desc] SUBFAIL", ... (pytest-subtests puts description before the word).
pub(crate) fn outcome_word(report: &TestReport) -> String {
    let reasoned = |word: &str| match report.longrepr.as_deref() {
        Some(reason) if !reason.is_empty() && !reason.contains('\n') => {
            format!("{word} ({reason})")
        }
        _ => word.to_string(),
    };
    if let Some(desc) = &report.subtest_desc {
        match report.outcome {
            Outcome::Failed => format!("{desc} SUBFAIL"),
            Outcome::Skipped => reasoned(&format!("{desc} SUBSKIP")),
            Outcome::XFailed => reasoned(&format!("{desc} SUBXFAIL")),
            _ => format!("{desc} SUBPASS"),
        }
    } else if report.rerun {
        "RERUN".to_string()
    } else {
        match report.outcome {
            Outcome::Passed => "PASSED".to_string(),
            // setup/teardown failures are "ERROR", not "FAILED" (pytest's
            // report_teststatus: errors are non-call-phase failures).
            Outcome::Failed if report.phase != Phase::Call => "ERROR".to_string(),
            Outcome::Failed => "FAILED".to_string(),
            Outcome::Skipped => reasoned("SKIPPED"),
            Outcome::XFailed => reasoned("XFAIL"),
            Outcome::XPassed => "XPASS".to_string(),
        }
    }
}

/// The bare verbose word and (for skip/xfail/xpass) the raw reason, kept
/// separate so the caller can truncate/wrap the reason to the terminal width.
/// Other outcomes (and subtests/reruns, whose words already embed any reason)
/// carry no separate reason.
pub(crate) fn verbose_outcome(report: &TestReport) -> (String, Option<String>) {
    if report.subtest_desc.is_some() || report.rerun {
        return (outcome_word(report), None);
    }
    let reason = report
        .longrepr
        .clone()
        .filter(|r| !r.is_empty() && !r.contains('\n'));
    match report.outcome {
        Outcome::Skipped => ("SKIPPED".to_string(), reason),
        Outcome::XFailed => ("XFAIL".to_string(), reason),
        Outcome::XPassed => ("XPASS".to_string(), reason),
        _ => (outcome_word(report), None),
    }
}

/// Terminal width for right-aligning the progress percentage, like
/// pytest's TerminalWriter (COLUMNS env, else 80).
pub(crate) fn term_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.trim().parse().ok())
        .unwrap_or(80)
}

/// The console_output_style progress field for `done`/`total` (and, for the
/// "times" style, the elapsed `dur`): "[ 50%]", "[10/20]", " 0.123s", or "".
pub(crate) fn progress_message(
    kind: ProgressKind,
    done: usize,
    total: usize,
    dur: Duration,
) -> String {
    match kind {
        ProgressKind::Percent => format!("[{:>3}%]", done * 100 / total),
        ProgressKind::Count => {
            let width = total.to_string().len();
            format!("[{done:>width$}/{total}]")
        }
        ProgressKind::Times => node_duration(dur),
        ProgressKind::Hidden => String::new(),
    }
}

/// pytest's format_node_duration, sans the leading space: a compact, human
/// readable duration (us/ms/s/m/h) for the "times" progress style.
fn node_duration(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 0.00001 {
        format!("{:.3}us", s * 1_000_000.0)
    } else if s < 0.0001 {
        format!("{:.2}us", s * 1_000_000.0)
    } else if s < 0.001 {
        format!("{:.1}us", s * 1_000_000.0)
    } else if s < 0.01 {
        format!("{:.3}ms", s * 1000.0)
    } else if s < 0.1 {
        format!("{:.2}ms", s * 1000.0)
    } else if s < 1.0 {
        format!("{:.1}ms", s * 1000.0)
    } else if s < 60.0 {
        format!("{s:.3}s")
    } else if s < 3600.0 {
        format!("{:.0}m {:.0}s", s / 60.0, s % 60.0)
    } else {
        format!("{:.0}h {:.0}m", s / 3600.0, (s % 3600.0) / 60.0)
    }
}

/// The padding + progress field that completes an already-printed progress
/// line of `body`'s width (the body itself streamed char by char). Empty
/// when the field is hidden (classic / capture-off styles).
pub(crate) fn progress_suffix(body: &str, msg: &str, color: u8) -> String {
    if msg.is_empty() {
        return String::new();
    }
    // pytest right-justifies the field to fullwidth - 1, leaving one trailing
    // column, so the whole progress line is one char short of the terminal.
    let pad = term_width()
        .saturating_sub(1)
        .saturating_sub(body.chars().count() + msg.len());
    let suffix = if pad > 0 {
        format!("{}{msg}", " ".repeat(pad))
    } else {
        // The field keeps its single leading space when it overflows the
        // line (the space is part of " [ 33%]", not the padding).
        format!(" {msg}")
    };
    crate::tw::markup(&suffix, &[color])
}

/// "body        [ 33%]" — the progress field right-aligned at the line edge.
pub(crate) fn with_progress(body: &str, msg: &str) -> String {
    if msg.is_empty() {
        return body.to_string();
    }
    let pad = term_width()
        .saturating_sub(1)
        .saturating_sub(body.chars().count() + msg.len());
    if pad > 0 {
        format!("{body}{}{msg}", " ".repeat(pad))
    } else {
        format!("{body} {msg}")
    }
}

pub(crate) fn report_from_err(
    py: Python<'_>,
    config: &Config,
    item: &TestItem,
    phase: Phase,
    started: Instant,
    err: &PyErr,
) -> TestReport {
    // Raw unittest.SkipTest (e.g. from a plain test function) skips like
    // pytest.skip — upstream's makereport conversion (#13985).
    let mapped = python::map_skiptest(py, err.clone_ref(py));
    let err = &mapped;
    if python::is_xfailed(py, err) {
        TestReport {
            nodeid: item.nodeid.clone(),
            phase,
            outcome: Outcome::XFailed,
            duration: started.elapsed(),
            longrepr: python::outcome_msg(py, err),
            location: None,
            subtest_desc: None,
            sections: Vec::new(),
            rerun: false,
            xfail_longrepr: None,
            reprcrash_message: None,
            head_line: None,
        }
    } else if python::is_skipped(py, err) {
        // Imperative skips report where pytest.skip was raised; skips out
        // of fixtures/xunit setup report the item's definition site instead
        // (pytest's _use_item_location), so the user knows which test. An
        // explicit `_location` on the exception (unittest decorators) wins.
        let location = python::skip_location_override(py, err).or_else(|| {
            if phase == Phase::Setup {
                // Use invocation-dir-relative path so the SKIPPED summary shows
                // "tests/test_1.py:N" when rootdir is a subdirectory (not just
                // "test_1.py" which is rootdir-relative).
                let file = crate::collect::file_nodeid(&config.invocation_dir, &item.path);
                Some(format!("{file}:{}", item.lineno))
            } else {
                python::raise_location(py, err)
            }
        });
        TestReport {
            nodeid: item.nodeid.clone(),
            phase,
            outcome: Outcome::Skipped,
            duration: started.elapsed(),
            longrepr: python::outcome_msg(py, err),
            location,
            subtest_desc: None,
            sections: Vec::new(),
            rerun: false,
            xfail_longrepr: None,
            reprcrash_message: None,
            head_line: None,
        }
    } else {
        TestReport {
            nodeid: item.nodeid.clone(),
            phase,
            outcome: Outcome::Failed,
            duration: started.elapsed(),
            longrepr: Some(python::format_test_failure(
                py,
                err,
                config.get_value("tb").unwrap_or("long"),
            )),
            location: None,
            subtest_desc: None,
            // "Captured stdout/log {when}" report sections; the terminal
            // appends them to the longrepr at render time.
            sections: python::log_failure_sections(py),
            rerun: false,
            xfail_longrepr: None,
            reprcrash_message: python::crash_message(py, err),
            head_line: None,
        }
    }
}

pub fn summary_line(
    reports: &[TestReport],
    deselected: usize,
    warning_count: usize,
    elapsed: Duration,
    verbosity: i32,
) -> String {
    summary_line_with_extras(reports, deselected, warning_count, elapsed, verbosity, &Default::default())
}

pub fn summary_line_with_extras(
    reports: &[TestReport],
    deselected: usize,
    warning_count: usize,
    elapsed: Duration,
    verbosity: i32,
    extra_stats: &std::collections::HashMap<String, usize>,
) -> String {
    // -qq (verbosity < -1) suppresses the stats line entirely.
    if verbosity < -1 {
        return String::new();
    }
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut errors = 0usize;
    let mut skipped = 0usize;
    let mut xfailed = 0usize;
    let mut xpassed = 0usize;
    let mut subtests_passed = 0usize;
    let mut rerun = 0usize;
    for report in reports {
        // A retried attempt (pytest-rerunfailures): its own "rerun" category,
        // never counted as failed/error.
        if report.rerun {
            rerun += 1;
            continue;
        }
        // Passed subtests count their own category; other subtest outcomes
        // fold into the regular buckets (upstream report_teststatus).
        if report.subtest_desc.is_some() && report.outcome == Outcome::Passed {
            subtests_passed += 1;
            continue;
        }
        match (report.phase, report.outcome) {
            (Phase::Call, Outcome::Passed) => passed += 1,
            (Phase::Call, Outcome::Failed) => failed += 1,
            (Phase::Call, Outcome::Skipped) | (Phase::Setup, Outcome::Skipped) => skipped += 1,
            (Phase::Setup, Outcome::Failed) | (Phase::Teardown, Outcome::Failed) => errors += 1,
            (_, Outcome::XFailed) => xfailed += 1,
            (_, Outcome::XPassed) => xpassed += 1,
            _ => {}
        }
    }
    use crate::tw;
    // Extra failed/skipped from plugin-driven reports (e.g. pytest-subtests
    // categorizes subtest failures as "failed" in the TerminalReporter stats).
    let extra_failed = *extra_stats.get("failed").unwrap_or(&0);
    let extra_skipped = *extra_stats.get("skipped").unwrap_or(&0);
    let total_failed = failed + extra_failed;
    let total_skipped = skipped + extra_skipped;
    let mut parts: Vec<(String, u8)> = Vec::new();
    if total_failed > 0 {
        parts.push((format!("{total_failed} failed"), tw::RED));
    }
    if passed > 0 {
        parts.push((format!("{passed} passed"), tw::GREEN));
    }
    if total_skipped > 0 {
        parts.push((format!("{total_skipped} skipped"), tw::YELLOW));
    }
    // Plugin-driven subtest stats (from the TerminalReporter's stats dict,
    // populated by the pytest-subtests plugin's pytest_report_teststatus hook).
    let plugin_subtests_passed = *extra_stats.get("subtests passed").unwrap_or(&0);
    let plugin_subtests_failed = *extra_stats.get("subtests failed").unwrap_or(&0);
    let plugin_subtests_skipped = *extra_stats.get("subtests skipped").unwrap_or(&0);
    let plugin_subtests_xfailed = *extra_stats.get("subtests xfailed").unwrap_or(&0);
    let plugin_subtests_xpassed = *extra_stats.get("subtests xpassed").unwrap_or(&0);
    let total_subtests_passed = subtests_passed + plugin_subtests_passed;
    if total_subtests_passed > 0 {
        parts.push((format!("{total_subtests_passed} subtests passed"), tw::GREEN));
    }
    if plugin_subtests_failed > 0 {
        parts.push((format!("{plugin_subtests_failed} subtests failed"), tw::RED));
    }
    if plugin_subtests_skipped > 0 {
        parts.push((format!("{plugin_subtests_skipped} subtests skipped"), tw::YELLOW));
    }
    if deselected > 0 {
        parts.push((format!("{deselected} deselected"), tw::YELLOW));
    }
    let total_xfailed = xfailed + plugin_subtests_xfailed;
    if total_xfailed > 0 {
        let label = if plugin_subtests_xfailed > 0 && xfailed == 0 {
            "subtests xfailed"
        } else {
            "xfailed"
        };
        parts.push((format!("{total_xfailed} {label}"), tw::YELLOW));
    }
    if xpassed + plugin_subtests_xpassed > 0 {
        parts.push((format!("{} xpassed", xpassed + plugin_subtests_xpassed), tw::YELLOW));
    }
    if rerun > 0 {
        parts.push((format!("{rerun} rerun"), tw::YELLOW));
    }
    if warning_count > 0 {
        parts.push((
            format!(
                "{warning_count} warning{}",
                if warning_count == 1 { "" } else { "s" }
            ),
            tw::YELLOW,
        ));
    }
    if errors > 0 {
        parts.push((
            format!("{errors} error{}", if errors == 1 { "" } else { "s" }),
            tw::RED,
        ));
    }
    if parts.is_empty() {
        parts.push(("no tests ran".to_string(), tw::YELLOW));
    }
    let main = tw::main_color(
        total_failed + plugin_subtests_failed,
        errors,
        warning_count,
        xpassed + plugin_subtests_xpassed,
        passed + total_subtests_passed,
        true,
    );
    let plain_parts: Vec<&str> = parts.iter().map(|(text, _)| text.as_str()).collect();
    let plain_body = format!(
        "{} in {:.2}s",
        plain_parts.join(", "),
        elapsed.as_secs_f64()
    );
    // -q (verbosity < 0) drops the "=" separators around the stats line
    // (pytest's display_sep = verbosity >= 0).
    let sep = verbosity >= 0;
    if !tw::enabled() {
        return if sep {
            crate::engine::center_banner(&plain_body)
        } else {
            plain_body
        };
    }
    if !sep {
        let mut out = String::new();
        for (i, (text, color)) in parts.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            let codes: &[u8] = if *color == main {
                &[*color, tw::BOLD]
            } else {
                &[*color]
            };
            out.push_str(&tw::markup(text, codes));
        }
        out.push_str(&tw::markup(
            &format!(" in {:.2}s", elapsed.as_secs_f64()),
            &[main],
        ));
        return out;
    }
    let banner = crate::engine::center_banner(&plain_body);
    // pytest's nesting: the left banner segment opens the main color
    // without a reset, each count carries its own color (bold when it
    // matches the main color), the tail segments re-open the main color.
    let (left, right) = banner.split_once(&plain_body).unwrap_or_default();
    let mut out = String::new();
    out.push_str(&tw::open(&[main]));
    out.push_str(left);
    for (i, (text, color)) in parts.iter().enumerate() {
        if i > 0 {
            out.push_str(&tw::open(&[main]));
            out.push_str(", ");
        }
        let codes: &[u8] = if *color == main {
            &[*color, tw::BOLD]
        } else {
            &[*color]
        };
        out.push_str(&tw::markup(text, codes));
    }
    out.push_str(&tw::markup(
        &format!(" in {:.2}s", elapsed.as_secs_f64()),
        &[main],
    ));
    out.push_str(&tw::markup(right, &[main]));
    out
}

/// --setup-show display attributes: (scope letter, indent width).
pub(crate) fn scope_display(scope: Scope) -> (char, usize) {
    match scope {
        Scope::Session => ('S', 0),
        Scope::Package => ('P', 2),
        Scope::Module => ('M', 4),
        Scope::Class => ('C', 6),
        Scope::Function => ('F', 8),
    }
}
