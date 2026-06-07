//! Distributed execution (-n N): start N workers, feed them batches of
//! node IDs from a shared queue (work stealing: fast workers pull more),
//! and merge the streamed reports plus per-plugin state dumps.
//!
//! On unix, workers fork off the parent after collection — the imported
//! test modules, conftests, and fixture registry arrive copy-on-write, so
//! workers skip the per-process import cost that upstream xdist pays.
//! PYTEST_RS_DIST_SPAWN=1 opts back into spawn-per-worker (each worker
//! re-imports everything, xdist's model); non-unix always spawns.
//!
//! Dispatch granularity follows --dist: per-test for load/worksteal (the
//! default, xdist parity), per-module for loadscope/loadfile/loadgroup
//! (each module imported by one worker), duplicated per worker for each.
//! Crashed workers fail their running test, requeue the rest, and are
//! replaced while --max-worker-restart's budget lasts; an exhausted budget
//! aborts undispatched work (xdist's shutdown semantics).

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Lines, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};

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

/// The shared work queue. An empty queue means shutdown for whoever asks —
/// like xdist, workers are released once everything is dispatched (waiting
/// for completion would deadlock suites whose class setups hold
/// cross-process locks). Crash bookkeeping lives under the same lock so
/// concurrent crashes resolve deterministically: a crashed worker requeues
/// its remainder for its own replacement; the crash that exhausts the
/// restart budget aborts whatever was not yet dispatched; crashes that
/// land after the abort are silent (their tests count as undispatched,
/// not failed — xdist's shutdown semantics).
struct WorkQueue {
    state: Mutex<QueueState>,
}

struct QueueState {
    queue: VecDeque<Vec<String>>,
    aborted: bool,
    /// Remaining worker-restart budget (no flag = effectively unlimited).
    restarts: isize,
}

/// What a worker-owner thread must do about its crashed worker.
enum CrashAction {
    /// Budget left: the remainder was requeued, start a replacement.
    Replace,
    /// This crash exhausted the budget: report it and stop dispatching.
    Abort,
    /// The run was already aborted: stop without reporting.
    Silent,
}

impl WorkQueue {
    fn new(batches: VecDeque<Vec<String>>, restarts: isize) -> Self {
        Self {
            state: Mutex::new(QueueState {
                queue: batches,
                aborted: false,
                restarts,
            }),
        }
    }

    /// The next batch, or None once all work is dispatched or aborted.
    fn next(&self) -> Option<Vec<String>> {
        let mut state = self.state.lock().expect("work queue lock poisoned");
        if state.aborted {
            return None;
        }
        state.queue.pop_front()
    }

    /// Crash bookkeeping, atomically: spend a restart and requeue the
    /// unfinished remainder, or exhaust the budget and abort.
    fn crash(&self, remaining: Vec<String>) -> CrashAction {
        let mut state = self.state.lock().expect("work queue lock poisoned");
        if state.aborted {
            return CrashAction::Silent;
        }
        if state.restarts > 0 {
            state.restarts -= 1;
            if !remaining.is_empty() {
                state.queue.push_front(remaining);
            }
            CrashAction::Replace
        } else {
            state.aborted = true;
            state.queue.clear();
            CrashAction::Abort
        }
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

        let queue = Arc::new(WorkQueue::new(batches, max_restart.unwrap_or(isize::MAX)));
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

        // Fork workers before any thread exists (fork + threads don't
        // mix); a worker that fails to fork falls back to spawning.
        #[cfg(unix)]
        let mut initial: Vec<Option<WorkerProc>> =
            if std::env::var_os("PYTEST_RS_DIST_SPAWN").is_none() {
                self.fork_workers(py, workers, &testrun_uid)
            } else {
                (0..workers).map(|_| None).collect()
            };
        #[cfg(not(unix))]
        let mut initial: Vec<Option<WorkerProc>> = (0..workers).map(|_| None).collect();

        let mut handles = Vec::new();
        for (index, slot) in initial.iter_mut().enumerate().take(workers) {
            let owner = WorkerOwner {
                queue: Arc::clone(&queue),
                sender: sender.clone(),
                argv: argv.clone(),
                index,
                worker_count: workers,
                max_restart,
                testrun_uid: testrun_uid.clone(),
                initial: slot.take(),
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
                    // Delegated mode: the replacement reporter renders the
                    // arrival-order progress (xdist drives it the same way).
                    if self.session.custom_reporter.is_some() {
                        match crate::python::make_report_proxy(py, &report, None) {
                            Ok(proxy) => crate::python::reporter_logreport(py, proxy.bind(py)),
                            Err(err) => {
                                eprintln!(
                                    "INTERNAL ERROR: {}",
                                    crate::python::format_exception(py, &err)
                                );
                            }
                        }
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

    /// Fork one child per worker slot off the already-imported parent
    /// interpreter. The parent sets the xdist worker env vars through
    /// os.environ right before each fork (and restores them after), so
    /// the child holds its identity from its first instruction — visible
    /// to os.register_at_fork callbacks, not just later reads. Children
    /// dup their pipe pair onto stdin/stdout and enter the worker loop;
    /// they never return. A failed fork yields None and that slot spawns
    /// instead.
    #[cfg(unix)]
    #[allow(unsafe_code)]
    fn fork_workers(
        &mut self,
        py: Python<'_>,
        count: usize,
        testrun_uid: &str,
    ) -> Vec<Option<WorkerProc>> {
        use std::os::fd::FromRawFd;

        const ENV_KEYS: [&str; 3] = [
            "PYTEST_XDIST_WORKER",
            "PYTEST_XDIST_WORKER_COUNT",
            "PYTEST_XDIST_TESTRUNUID",
        ];
        // os.environ (not Rust setenv: the Python dict snapshots the C
        // environ at import, and __setitem__ writes through via putenv).
        let environ = py.import("os").and_then(|os| os.getattr("environ")).ok();
        // Restore values for the parent (we may ourselves be a worker of
        // an outer -n run, where these are already set).
        let saved: Vec<Option<String>> = ENV_KEYS
            .iter()
            .map(|key| {
                environ.as_ref().and_then(|environ| {
                    environ
                        .call_method1("get", (*key,))
                        .ok()
                        .and_then(|value| value.extract().ok())
                })
            })
            .collect();

        let mut procs: Vec<Option<WorkerProc>> = Vec::with_capacity(count);
        // Parent-side pipe ends accumulated so far: each child closes its
        // siblings' ends, otherwise a crashed sibling never reads as EOF.
        let mut parent_fds: Vec<libc::c_int> = Vec::new();
        for index in 0..count {
            if let Some(environ) = &environ {
                let _ = environ.set_item(ENV_KEYS[0], format!("gw{index}"));
                let _ = environ.set_item(ENV_KEYS[1], count.to_string());
                let _ = environ.set_item(ENV_KEYS[2], testrun_uid);
            }
            // Flush both runtimes' stdio: buffered bytes would be
            // duplicated into the child, whose fd 1 becomes the protocol
            // pipe.
            let _ = py.run(
                c"import sys\nsys.stdout.flush()\nsys.stderr.flush()\n",
                None,
                None,
            );
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().flush();

            let mut to_child: [libc::c_int; 2] = [0; 2];
            let mut from_child: [libc::c_int; 2] = [0; 2];
            // SAFETY: plain pipe(2) calls; results are checked.
            if unsafe { libc::pipe(to_child.as_mut_ptr()) } != 0 {
                procs.push(None);
                continue;
            }
            if unsafe { libc::pipe(from_child.as_mut_ptr()) } != 0 {
                unsafe {
                    libc::close(to_child[0]);
                    libc::close(to_child[1]);
                }
                procs.push(None);
                continue;
            }

            // SAFETY: the GIL is held (`py`), and no Rust threads exist
            // yet; PyOS_BeforeFork/AfterFork_* run CPython's os.fork
            // bookkeeping (at-fork callbacks, lock reinit).
            unsafe { pyo3::ffi::PyOS_BeforeFork() };
            let pid = unsafe { libc::fork() };
            if pid < 0 {
                unsafe {
                    pyo3::ffi::PyOS_AfterFork_Parent();
                    libc::close(to_child[0]);
                    libc::close(to_child[1]);
                    libc::close(from_child[0]);
                    libc::close(from_child[1]);
                }
                procs.push(None);
                continue;
            }
            if pid == 0 {
                // Child: become worker `index` (its env vars were set
                // before the fork), never return.
                unsafe {
                    pyo3::ffi::PyOS_AfterFork_Child();
                    libc::dup2(to_child[0], 0);
                    libc::dup2(from_child[1], 1);
                    libc::close(to_child[0]);
                    libc::close(to_child[1]);
                    libc::close(from_child[0]);
                    libc::close(from_child[1]);
                    for fd in &parent_fds {
                        libc::close(*fd);
                    }
                }
                // --tx popen//chdir=DIR: this worker runs in DIR.
                if let Some(Some(dir)) = self
                    .config
                    .tx_worker_chdirs()
                    .as_ref()
                    .and_then(|chdirs| chdirs.get(index))
                    .map(|chdir| chdir.as_ref())
                {
                    let _ = std::env::set_current_dir(dir);
                }
                let code = self.run_worker_forked(py);
                std::process::exit(code);
            }
            // Parent: keep our ends, close the child's.
            unsafe {
                pyo3::ffi::PyOS_AfterFork_Parent();
                libc::close(to_child[0]);
                libc::close(from_child[1]);
            }
            parent_fds.push(to_child[1]);
            parent_fds.push(from_child[0]);
            // SAFETY: the parent owns these freshly created pipe ends.
            let stdin: Box<dyn Write + Send> =
                Box::new(unsafe { std::fs::File::from_raw_fd(to_child[1]) });
            let stdout: Box<dyn Read + Send> =
                Box::new(unsafe { std::fs::File::from_raw_fd(from_child[0]) });
            procs.push(Some(WorkerProc {
                handle: WorkerHandle::Forked(pid),
                stdin,
                lines: BufReader::new(stdout).lines(),
            }));
        }
        // Restore the parent's own env (it stays the controller).
        if let Some(environ) = &environ {
            for (key, value) in ENV_KEYS.iter().zip(&saved) {
                let _ = match value {
                    Some(value) => environ.set_item(*key, value),
                    None => environ.del_item(*key),
                };
            }
        }
        procs
    }
}

/// A live worker: spawned (its own exec, re-imports everything) or forked
/// (inherits the parent's imported interpreter, unix only).
enum WorkerHandle {
    Spawned(Child),
    #[cfg(unix)]
    Forked(libc::pid_t),
}

impl WorkerHandle {
    #[allow(unsafe_code)]
    fn wait(&mut self) {
        match self {
            WorkerHandle::Spawned(child) => {
                let _ = child.wait();
            }
            #[cfg(unix)]
            WorkerHandle::Forked(pid) => {
                let mut status: libc::c_int = 0;
                // SAFETY: reaping our own forked child.
                unsafe { libc::waitpid(*pid, &mut status, 0) };
            }
        }
    }
}

struct WorkerProc {
    handle: WorkerHandle,
    stdin: Box<dyn Write + Send>,
    lines: Lines<BufReader<Box<dyn Read + Send>>>,
}

/// One thread per worker slot: feed batches from the shared queue, forward
/// frames, replace the process if it dies mid-batch.
struct WorkerOwner {
    queue: Arc<WorkQueue>,
    sender: mpsc::Sender<Event>,
    argv: Vec<String>,
    index: usize,
    worker_count: usize,
    max_restart: Option<isize>,
    testrun_uid: String,
    /// A pre-forked worker for this slot; replacements (after a crash)
    /// always spawn — re-forking is unsafe once threads exist.
    initial: Option<WorkerProc>,
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
        let stdin: Box<dyn Write + Send> =
            Box::new(child.stdin.take().expect("worker stdin is piped"));
        let stdout: Box<dyn Read + Send> =
            Box::new(child.stdout.take().expect("worker stdout is piped"));
        Ok(WorkerProc {
            handle: WorkerHandle::Spawned(child),
            stdin,
            lines: BufReader::new(stdout).lines(),
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
        proc.handle.wait();
        let running = if remaining.is_empty() {
            None
        } else {
            Some(remaining.remove(0))
        };
        let action = self.queue.crash(remaining);
        if let (Some(running), CrashAction::Replace | CrashAction::Abort) = (&running, &action) {
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
                    subtest_desc: None,
                    sections: Vec::new(),
                },
                worker: self.index,
            });
        }

        match action {
            CrashAction::Replace => {
                let _ = self.sender.send(Event::Output(format!(
                    "replacing crashed worker gw{}",
                    self.index
                )));
                self.spawn().ok()
            }
            CrashAction::Abort => {
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
                None
            }
            // The run was already aborted by another slot's crash: this
            // worker's tests count as undispatched, not failed.
            CrashAction::Silent => None,
        }
    }

    fn run(mut self) {
        let mut proc = match self.initial.take() {
            Some(proc) => proc,
            None => match self.spawn() {
                Ok(proc) => proc,
                Err(err) => {
                    eprintln!("ERROR: failed to spawn worker: {err}");
                    return;
                }
            },
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
                    Some(WorkerMsg::Done) => continue 'work,
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
        proc.handle.wait();
    }
}
