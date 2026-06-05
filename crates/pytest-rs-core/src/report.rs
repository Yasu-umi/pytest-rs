use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Setup,
    Call,
    Teardown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    Passed,
    Failed,
    Skipped,
    /// Failed under @pytest.mark.xfail (expected).
    XFailed,
    /// Passed despite @pytest.mark.xfail.
    XPassed,
}

/// Plain data: reports stream across the worker IPC boundary as JSON.
#[derive(Debug, Serialize, Deserialize)]
pub struct TestReport {
    pub nodeid: String,
    pub phase: Phase,
    pub outcome: Outcome,
    pub duration: Duration,
    /// Formatted failure representation (traceback) or skip reason.
    pub longrepr: Option<String>,
    /// "file.py:line" of the skip/xfail origin, for -r summary grouping.
    #[serde(default)]
    pub location: Option<String>,
    /// Subtest description like "[msg] (i=3)"; Some marks a subtest report.
    #[serde(default)]
    pub subtest_desc: Option<String>,
}

impl TestReport {
    /// The single-char progress marker for this report, if it should print one.
    pub fn progress_char(&self) -> Option<char> {
        if self.subtest_desc.is_some() {
            // Upstream subtests short letters: u (failed/passed), - (skipped),
            // y (xfail). Quiet subtest verbosity drops non-failed subtest
            // reports before they reach here, so default runs print only 'u'.
            return match self.outcome {
                Outcome::Skipped => Some('-'),
                Outcome::XFailed => Some('y'),
                _ => Some('u'),
            };
        }
        match (self.phase, self.outcome) {
            (Phase::Call, Outcome::Passed) => Some('.'),
            (_, Outcome::Failed) => Some(if self.phase == Phase::Call { 'F' } else { 'E' }),
            (_, Outcome::Skipped) => Some('s'),
            (_, Outcome::XFailed) => Some('x'),
            (_, Outcome::XPassed) => Some('X'),
            _ => None,
        }
    }
}

/// pytest-compatible exit codes.
pub mod exit_code {
    pub const OK: i32 = 0;
    pub const TESTS_FAILED: i32 = 1;
    pub const INTERRUPTED: i32 = 2;
    pub const INTERNAL_ERROR: i32 = 3;
    pub const USAGE_ERROR: i32 = 4;
    pub const NO_TESTS_COLLECTED: i32 = 5;
}
