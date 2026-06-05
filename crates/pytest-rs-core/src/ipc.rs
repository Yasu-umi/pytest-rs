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
    /// Serialized plugin state to merge in the parent (cov hits, ...).
    Extra { plugin: String, payload: String },
    /// Warnings captured in this worker, for the parent's summary.
    Warnings { lines: Vec<String>, count: usize },
    /// Clean shutdown.
    Bye,
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
