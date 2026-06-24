//! Terminal output: header, failures, summaries, durations, fixtures.

mod display;
mod results;
mod summary;

/// pytest's running_on_ci(): the CI / BUILD_NUMBER env vars suppress
/// short-summary message trimming so CI logs keep the full crash text.
/// Python's `%g` format: shortest representation, strip trailing zeros.
fn format_g(v: f64) -> String {
    let s = format!("{v:.6}");
    let s = s.trim_end_matches('0');
    let s = s.trim_end_matches('.');
    s.to_string()
}

fn running_on_ci() -> bool {
    std::env::var_os("CI").is_some() || std::env::var_os("BUILD_NUMBER").is_some()
}

/// pytest's _format_trimmed for the " - {}" short-summary form: ellipsize
/// `msg` to fit `available` columns after the prefix, or None when even the
/// ellipsis would not fit (so the caller drops the message entirely).
fn format_trimmed(prefix: &str, msg: &str, available: usize) -> Option<String> {
    const ELLIPSIS: &str = "...";
    let msg = msg.split('\n').next().unwrap_or(msg);
    let format_width = prefix.chars().count();
    if format_width + ELLIPSIS.len() > available {
        return None;
    }
    if format_width + msg.chars().count() <= available {
        return Some(format!("{prefix}{msg}"));
    }
    let budget = available - format_width - ELLIPSIS.len();
    let trimmed: String = msg.chars().take(budget).collect();
    Some(format!("{prefix}{trimmed}{ELLIPSIS}"))
}
