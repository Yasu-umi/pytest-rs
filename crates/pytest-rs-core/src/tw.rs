//! TerminalWriter-style ANSI markup (pytest's _io.terminalwriter).
//!
//! One process-wide switch, decided once at startup from --color, the
//! standard env knobs and isatty; every colored emission routes through
//! [`markup`] so `--color=no` really is plain.

use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

pub const BOLD: u8 = 1;
pub const RED: u8 = 31;
pub const GREEN: u8 = 32;
pub const YELLOW: u8 = 33;
pub const CYAN: u8 = 36;

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Wrap `text` in the given SGR codes (each its own escape, like pytest)
/// with a single reset; plain passthrough when color is off.
pub fn markup(text: &str, codes: &[u8]) -> String {
    if !enabled() || codes.is_empty() {
        return text.to_string();
    }
    let mut out = String::new();
    for code in codes {
        out.push_str(&format!("\x1b[{code}m"));
    }
    out.push_str(text);
    out.push_str("\x1b[0m");
    out
}

/// The opening SGR sequence(s) without a reset, for pytest's nested
/// summary-line quirk (the left banner segment is left unterminated).
pub fn open(codes: &[u8]) -> String {
    if !enabled() {
        return String::new();
    }
    codes.iter().map(|code| format!("\x1b[{code}m")).collect()
}

/// The color of a session summary / progress fill: red on failures or
/// errors, yellow for warnings or xpasses, green while something passed —
/// or while the run isn't finished yet (pytest's main color, including
/// its not-the-last-item quirk).
pub fn main_color(
    failed: usize,
    errors: usize,
    warnings: usize,
    xpassed: usize,
    passed: usize,
    finished: bool,
) -> u8 {
    if failed > 0 || errors > 0 {
        RED
    } else if warnings > 0 || xpassed > 0 {
        YELLOW
    } else if passed > 0 || !finished {
        GREEN
    } else {
        YELLOW
    }
}

/// Decide whether to emit color: explicit --color wins, then PY_COLORS,
/// NO_COLOR / FORCE_COLOR, finally stdout's tty-ness.
pub fn should_colorize(color_option: Option<&str>) -> bool {
    match color_option {
        Some("yes") => return true,
        Some("no") => return false,
        _ => {}
    }
    match std::env::var("PY_COLORS").as_deref() {
        Ok("1") => return true,
        Ok("0") => return false,
        _ => {}
    }
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if std::env::var_os("FORCE_COLOR").is_some() {
        return true;
    }
    use std::io::IsTerminal;
    std::io::stdout().is_terminal()
}
