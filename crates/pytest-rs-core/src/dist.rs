//! Distributed execution (-n N): spawn N workers (the same binary in
//! --worker mode), feed them batches of node IDs from a shared queue
//! (work stealing: fast workers pull more), and merge the streamed reports
//! plus per-plugin state dumps.
//!
//! Dispatch granularity follows --dist: per-test for load/worksteal (the
//! default, xdist parity), per-module for loadscope/loadfile/loadgroup
//! (each module imported by one worker), duplicated per worker for each.
//! Crashed workers fail their running test, requeue the rest, and are
//! replaced while --max-worker-restart's budget lasts; an exhausted budget
//! aborts undispatched work (xdist's shutdown semantics).

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Lines, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};

use pyo3::prelude::*;

use crate::engine::Engine;
use crate::hooks::HookContext;
use crate::ipc::{ParentMsg, WorkerMsg, decode_frame};
use crate::report::{Outcome, Phase, TestReport};

/// Events flowing from worker-owner threads to the main thread.
enum Event {
    Report {
        report: TestReport,
        worker: usize,
    },
    Extra {
        plugin: String,
        payload: String,
    },
    /// Warnings captured in a worker, for the parent's summary.
    Warnings {
        lines: Vec<String>,
        count: usize,
    },
    /// Passthrough/diagnostic output, printed as-is.
    Output(String),
    /// A fatal distribution condition, shown as a banner before the summary.
    Banner(String),
}

/// The shared work queue: idle workers wait while batches are in flight
/// (a crash may requeue work), and an exhausted restart budget aborts
/// whatever was not yet dispatched.
struct WorkQueue {
    state: Mutex<QueueState>,
    cond: Condvar,
}

struct QueueState {
    queue: VecDeque<Vec<String>>,
    in_flight: usize,
    aborted: bool,
}

impl WorkQueue {
    fn new(batches: VecDeque<Vec<String>>) -> Self {
        Self {
            state: Mutex::new(QueueState {
                queue: batches,
                in_flight: 0,
                aborted: false,
            }),
            cond: Condvar::new(),
        }
    }

    /// The next batch, or None once all work is finished or aborted.
    fn next(&self) -> Option<Vec<String>> {
        let mut state = self.state.lock().expect("work queue lock poisoned");
        loop {
            if state.aborted {
                return None;
            }
            if let Some(batch) = state.queue.pop_front() {
                state.in_flight += 1;
                return Some(batch);
            }
            if state.in_flight == 0 {
                return None;
            }
            state = self.cond.wait(state).expect("work queue lock poisoned");
        }
    }

    fn complete(&self) {
        let mut state = self.state.lock().expect("work queue lock poisoned");
        state.in_flight -= 1;
        self.cond.notify_all();
    }

    /// Crash bookkeeping: the unfinished remainder goes back to the front.
    fn requeue(&self, batch: Vec<String>) {
        let mut state = self.state.lock().expect("work queue lock poisoned");
        state.in_flight -= 1;
        if !state.aborted && !batch.is_empty() {
            state.queue.push_front(batch);
        }
        self.cond.notify_all();
    }

    fn abort(&self) {
        let mut state = self.state.lock().expect("work queue lock poisoned");
        state.aborted = true;
        state.queue.clear();
        self.cond.notify_all();
    }
}

impl Engine {
    pub(crate) fn run_dist(&mut self, py: Python<'_>, workers: usize) {
        let dist_mode = self.config.get_value("dist").unwrap_or("load");
        let per_module = matches!(dist_mode, "loadscope" | "loadfile" | "loadgroup");

        let mut batches: VecDeque<Vec<String>> = VecDeque::new();
        for item in &self.session.items {
            let file = item.nodeid.split("::").next().unwrap_or("");
            let same_module = per_module
                && batches.back().is_some_and(|batch: &Vec<String>| {
                    batch
                        .first()
                        .is_some_and(|first| first.split("::").next().unwrap_or("") == file)
                });
            if same_module {
                batches
                    .back_mut()
                    .expect("just checked")
                    .push(item.nodeid.clone());
            } else {
                batches.push_back(vec![item.nodeid.clone()]);
            }
        }
        if dist_mode == "each" {
            // every test runs on every worker
            let base: Vec<Vec<String>> = batches.iter().cloned().collect();
            for _ in 1..workers {
                batches.extend(base.iter().cloned());
            }
        }

        if !self.config.quiet && !self.config.no_terminal() {
            let noun = if workers == 1 { "worker" } else { "workers" };
            println!("created: {workers}/{workers} {noun}");
            println!("{} {} [{} items]", workers, noun, self.session.items.len());
        }

        // Restart budget shared across workers (no flag = unlimited).
        let max_restart: Option<isize> = self
            .config
            .get_value("max-worker-restart")
            .and_then(|value| value.parse().ok());
        let restarts = Arc::new(AtomicIsize::new(max_restart.unwrap_or(isize::MAX)));

        let queue = Arc::new(WorkQueue::new(batches));
        let (sender, receiver) = mpsc::channel::<Event>();
        let argv: Vec<String> = std::env::args().skip(1).collect();
        // One uid for the whole distributed run (the testrun_uid fixture).
        let testrun_uid = format!(
            "{:032x}",
            std::process::id() as u128
                ^ std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_nanos())
                    .unwrap_or(0)
        );

        let mut handles = Vec::new();
        for index in 0..workers {
            let owner = WorkerOwner {
                queue: Arc::clone(&queue),
                sender: sender.clone(),
                argv: argv.clone(),
                index,
                worker_count: workers,
                restarts: Arc::clone(&restarts),
                max_restart,
                testrun_uid: testrun_uid.clone(),
            };
            handles.push(std::thread::spawn(move || owner.run()));
        }
        drop(sender);

        // Merge loop: progress streams in arrival order (xdist-style).
        let mut reports: Vec<TestReport> = Vec::new();
        let mut extras: Vec<(String, String)> = Vec::new();
        let show_progress =
            !self.config.quiet && !self.config.no_terminal() && self.config.verbose == 0;
        let mut printed = 0usize;
        // Progress chars leave the line open; any full-line output must
        // close it first or fnmatch-style consumers see merged lines.
        let mut line_open = false;
        for event in receiver {
            match event {
                Event::Report { report, worker } => {
                    if self.config.no_terminal() {
                        // silent
                    } else if self.config.verbose > 0 {
                        if report.phase == Phase::Call || report.outcome != Outcome::Passed {
                            let word = match report.outcome {
                                Outcome::Passed => "PASSED",
                                Outcome::Failed => "FAILED",
                                Outcome::Skipped => "SKIPPED",
                                Outcome::XFailed => "XFAIL",
                                Outcome::XPassed => "XPASS",
                            };
                            // xdist verbose format: "[gw0] PASSED test_a.py::test"
                            println!("[gw{worker}] {word} {}", report.nodeid);
                        }
                    } else if show_progress && let Some(c) = report.progress_char() {
                        print!("{c}");
                        line_open = true;
                        printed += 1;
                        if printed.is_multiple_of(80) {
                            println!();
                            line_open = false;
                        }
                        let _ = std::io::stdout().flush();
                    }
                    reports.push(report);
                }
                Event::Extra { plugin, payload } => extras.push((plugin, payload)),
                Event::Warnings { lines, count } => {
                    self.session.worker_warnings.extend(lines);
                    self.session.worker_warning_count += count;
                }
                Event::Output(line) => {
                    if line_open {
                        println!();
                        line_open = false;
                    }
                    println!("{line}");
                }
                Event::Banner(message) => {
                    self.session.dist_banner.get_or_insert(message);
                }
            }
        }
        if line_open {
            println!();
        }
        for handle in handles {
            let _ = handle.join();
        }

        self.session.reports = reports;

        // Per-plugin state dumps (cov hits, benchmark results) merge before
        // sessionfinish builds reports.
        let mut ctx = HookContext {
            py,
            session: &mut self.session,
            config: &self.config,
        };
        for (plugin_name, payload) in extras {
            for plugin in self.plugins.iter_mut() {
                if plugin.name() == plugin_name {
                    if let Err(err) = plugin.pytest_worker_load(&mut ctx, &payload) {
                        eprintln!(
                            "INTERNAL ERROR: merging {plugin_name} worker state: {}",
                            crate::python::format_exception(py, &err)
                        );
                    }
                    break;
                }
            }
        }
    }
}

struct WorkerProc {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
}

/// One thread per worker slot: feed batches from the shared queue, forward
/// frames, replace the process if it dies mid-batch.
struct WorkerOwner {
    queue: Arc<WorkQueue>,
    sender: mpsc::Sender<Event>,
    argv: Vec<String>,
    index: usize,
    worker_count: usize,
    restarts: Arc<AtomicIsize>,
    max_restart: Option<isize>,
    testrun_uid: String,
}

impl WorkerOwner {
    fn spawn(&self) -> std::io::Result<WorkerProc> {
        let exe = std::env::current_exe()?;
        let mut child = Command::new(exe)
            .args(&self.argv)
            .arg("--worker")
            .env("PYTEST_XDIST_WORKER", format!("gw{}", self.index))
            .env("PYTEST_XDIST_WORKER_COUNT", self.worker_count.to_string())
            .env("PYTEST_XDIST_TESTRUNUID", &self.testrun_uid)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdin = child.stdin.take().expect("worker stdin is piped");
        let stdout = BufReader::new(child.stdout.take().expect("worker stdout is piped"));
        Ok(WorkerProc {
            child,
            stdin,
            lines: stdout.lines(),
        })
    }

    /// Crash bookkeeping: the running test fails, the unfinished remainder
    /// requeues, and the worker is replaced while the budget lasts.
    /// Returns the replacement, or None when this slot must stop.
    fn handle_crash(
        &self,
        proc: &mut WorkerProc,
        mut remaining: Vec<String>,
    ) -> Option<WorkerProc> {
        let _ = proc.child.wait();
        if !remaining.is_empty() {
            let running = remaining.remove(0);
            let _ = self.sender.send(Event::Report {
                report: TestReport {
                    nodeid: running.clone(),
                    phase: Phase::Call,
                    outcome: Outcome::Failed,
                    duration: std::time::Duration::ZERO,
                    longrepr: Some(format!(
                        "worker gw{} crashed while running {running}",
                        self.index
                    )),
                    location: None,
                },
                worker: self.index,
            });
        }

        if self.restarts.fetch_sub(1, Ordering::SeqCst) > 0 {
            self.queue.requeue(remaining);
            let _ = self.sender.send(Event::Output(format!(
                "replacing crashed worker gw{}",
                self.index
            )));
            self.spawn().ok()
        } else {
            // Budget exhausted: stop dispatching new work (xdist shutdown).
            let message = match self.max_restart {
                Some(0) => format!(
                    "worker gw{} crashed and worker restarting disabled",
                    self.index
                ),
                Some(max) => format!("maximum crashed workers reached: {max}"),
                None => format!("worker gw{} crashed", self.index),
            };
            let _ = self.sender.send(Event::Output(message.clone()));
            let _ = self.sender.send(Event::Banner(format!("xdist: {message}")));
            self.queue.requeue(Vec::new());
            self.queue.abort();
            None
        }
    }

    fn run(self) {
        let mut proc = match self.spawn() {
            Ok(proc) => proc,
            Err(err) => {
                eprintln!("ERROR: failed to spawn worker: {err}");
                return;
            }
        };

        'work: loop {
            let Some(batch) = self.queue.next() else {
                let msg = serde_json::to_string(&ParentMsg::Shutdown).expect("shutdown serializes");
                let _ = writeln!(proc.stdin, "{msg}");
                let _ = proc.stdin.flush();
                break;
            };

            let msg = serde_json::to_string(&ParentMsg::Run {
                nodeids: batch.clone(),
            })
            .expect("run message serializes");
            if writeln!(proc.stdin, "{msg}").is_err() || proc.stdin.flush().is_err() {
                match self.handle_crash(&mut proc, batch) {
                    Some(replacement) => {
                        proc = replacement;
                        continue;
                    }
                    None => return,
                }
            }

            // A test "completed" once its teardown report arrives.
            let mut remaining: Vec<String> = batch;
            loop {
                let Some(Ok(line)) = proc.lines.next() else {
                    // EOF mid-batch: the worker died (segfault, exit, ...).
                    match self.handle_crash(&mut proc, remaining) {
                        Some(replacement) => {
                            proc = replacement;
                            continue 'work;
                        }
                        None => return,
                    }
                };
                if line.trim().is_empty() {
                    continue;
                }
                match decode_frame(&line) {
                    Some(WorkerMsg::Report { report }) => {
                        if report.phase == Phase::Teardown {
                            remaining.retain(|nodeid| nodeid != &report.nodeid);
                        }
                        let _ = self.sender.send(Event::Report {
                            report,
                            worker: self.index,
                        });
                    }
                    Some(WorkerMsg::Done) => {
                        self.queue.complete();
                        continue 'work;
                    }
                    Some(WorkerMsg::Extra { plugin, payload }) => {
                        let _ = self.sender.send(Event::Extra { plugin, payload });
                    }
                    Some(WorkerMsg::Warnings { lines, count }) => {
                        let _ = self.sender.send(Event::Warnings { lines, count });
                    }
                    Some(WorkerMsg::Bye) => break 'work,
                    None => {
                        let _ = self.sender.send(Event::Output(line));
                    }
                }
            }
        }

        // Drain post-shutdown frames (warnings, plugin dumps, bye).
        for line in proc.lines {
            let Ok(line) = line else { break };
            match decode_frame(&line) {
                Some(WorkerMsg::Extra { plugin, payload }) => {
                    let _ = self.sender.send(Event::Extra { plugin, payload });
                }
                Some(WorkerMsg::Warnings { lines, count }) => {
                    let _ = self.sender.send(Event::Warnings { lines, count });
                }
                Some(WorkerMsg::Bye) | None => {}
                Some(_) => {}
            }
        }
        let _ = proc.child.wait();
    }
}
