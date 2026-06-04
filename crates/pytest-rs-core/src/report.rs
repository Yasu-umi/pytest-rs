use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Setup,
    Call,
    Teardown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Passed,
    Failed,
    Skipped,
    /// Failed under @pytest.mark.xfail (expected).
    XFailed,
    /// Passed despite @pytest.mark.xfail.
    XPassed,
}

#[derive(Debug)]
pub struct TestReport {
    pub nodeid: String,
    pub phase: Phase,
    pub outcome: Outcome,
    pub duration: Duration,
    /// Formatted failure representation (traceback) or skip reason.
    pub longrepr: Option<String>,
}

impl TestReport {
    /// The single-char progress marker for this report, if it should print one.
    pub fn progress_char(&self) -> Option<char> {
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
