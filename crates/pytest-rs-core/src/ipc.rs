//! Worker IPC: newline-delimited JSON. Parent → worker over a clean stdin;
//! worker → parent over stdout with a sentinel prefix, so stray test output
//! (we don't capture by default) can't be mistaken for protocol frames.

use serde::{Deserialize, Serialize};

use crate::report::TestReport;

/// Prefix marking protocol frames in the worker's stdout.
pub const FRAME_PREFIX: &str = "##pytest-rs-ipc##";

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ParentMsg {
    /// Run this batch of node IDs (module-affinity: usually one module).
    Run { nodeids: Vec<String> },
    /// No more work: tear down, dump plugin state, exit.
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum WorkerMsg {
    /// One phase report for one test, streamed as it happens.
    Report { report: TestReport },
    /// The current batch is finished; ready for the next.
    Done,
    /// A test raised KeyboardInterrupt (or pytest.exit): the session should
    /// stop and use this exit code. Sent before Done so the controller can
    /// call queue.stop() without losing the Done that follows.
    Interrupted { code: i32, banner: Option<String> },
    /// Serialized plugin state to merge in the parent (cov hits, ...).
    Extra { plugin: String, payload: String },
    /// Warnings captured in this worker, for the parent's summary.
    Warnings { lines: Vec<String>, count: usize },
    /// The worker's config.workeroutput as JSON (xdist data exchange:
    /// surfaces as node.workeroutput in pytest_testnodedown).
    Workeroutput { payload: String },
    /// Clean shutdown.
    Bye,
    /// Sent after precollect_all: the worker's full collected item set.
    /// The controller uses this to build work batches (worker-side collection).
    /// `errors` carries formatted collection error strings for files that
    /// failed to import so the controller can show them in the ERRORS section.
    /// `xdist_groups`: parallel to nodeids, the resolved xdist_group mark for
    /// each item (None = ungrouped).
    /// `deselected`: how many items -k/-m/--deselect/conftest
    /// pytest_collection_modifyitems dropped from this worker's raw
    /// collected set, for the controller's "N deselected" summary line.
    Collection {
        nodeids: Vec<String>,
        xdist_groups: Vec<Option<String>>,
        errors: Vec<(String, String)>,
        deselected: usize,
    },
}

/// Encode a worker frame ("\n" first breaks any unterminated test output
/// line so the frame starts at column 0).
pub fn encode_frame(msg: &WorkerMsg) -> String {
    let json = serde_json::to_string(msg).expect("worker message serializes");
    format!("\n{FRAME_PREFIX}{json}\n")
}

/// Decode a line from a worker's stdout: a protocol frame, or passthrough
/// test output.
pub fn decode_frame(line: &str) -> Option<WorkerMsg> {
    let json = line.strip_prefix(FRAME_PREFIX)?;
    serde_json::from_str(json).ok()
}
