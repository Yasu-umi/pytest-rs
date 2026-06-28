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

use std::collections::{HashMap, VecDeque};
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
    /// A worker's config.workeroutput JSON (xdist data exchange).
    Workeroutput {
        worker: usize,
        payload: String,
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
    /// True once -x/--maxfail fires: workers must not start new batches.
    soft_stopped: bool,
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
                soft_stopped: false,
                restarts,
            }),
        }
    }

    /// The next batch, or None once all work is dispatched or stopped/aborted.
    fn next(&self) -> Option<Vec<String>> {
        let mut state = self.state.lock().expect("work queue lock poisoned");
        if state.aborted || state.soft_stopped {
            return None;
        }
        state.queue.pop_front()
    }

    /// True when -x/--maxfail or a fatal crash has halted dispatch.
    fn is_stopped(&self) -> bool {
        let state = self.state.lock().expect("work queue lock poisoned");
        state.soft_stopped || state.aborted
    }

    /// -x/--maxfail: stop dispatching new batches; workers finish what
    /// they hold (upstream DSession waits for workers before interrupting).
    fn stop(&self) {
        let mut state = self.state.lock().expect("work queue lock poisoned");
        state.queue.clear();
        state.soft_stopped = true;
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
    /// The controller banner: "created: N/N workers" + "N workers [M items]"
    /// (-q collapses to "bringing up nodes...", -v adds the scheduler line).
    pub(crate) fn print_dist_banner(&self, workers: usize) {
        if self.config.no_terminal() {
            return;
        }
        if self.config.quiet {
            // Upstream -q: a single terse line instead of worker details.
            println!("bringing up nodes...");
            return;
        }
        let dist_mode = self.config.get_value("dist").unwrap_or("load");
        let noun = if workers == 1 { "worker" } else { "workers" };
        println!("created: {workers}/{workers} {noun}");
        if self.config.verbose > 0 {
            // Upstream -v narration, e.g. "scheduling tests via
            // LoadScheduling".
            let scheduler = match dist_mode {
                "each" => "EachScheduling",
                "loadscope" => "LoadScopeScheduling",
                "loadfile" => "LoadFileScheduling",
                "loadgroup" => "LoadGroupScheduling",
                "worksteal" => "WorkStealingScheduling",
                _ => "LoadScheduling",
            };
            println!("scheduling tests via {scheduler}");
        }
        let item_noun = if self.session.items.len() == 1 {
            "item"
        } else {
            "items"
        };
        println!(
            "{} {} [{} {}]",
            workers,
            noun,
            self.session.items.len(),
            item_noun
        );
    }

    /// --dist=loadgroup: the item's scheduling group — its xdist_group mark
    /// names, sorted and joined with "_" (upstream LoadGroupScheduling).
    fn xdist_group_of(py: Python<'_>, item: &crate::collect::TestItem) -> Option<String> {
        let mut names: Vec<String> = item
            .marks
            .iter()
            .filter(|mark| mark.name == "xdist_group")
            .filter_map(|mark| {
                let obj = mark.obj.bind(py);
                obj.getattr("kwargs")
                    .ok()
                    .and_then(|kwargs| kwargs.get_item("name").ok())
                    .and_then(|value| value.extract().ok())
                    .or_else(|| {
                        obj.getattr("args")
                            .ok()
                            .and_then(|args| args.get_item(0).ok())
                            .and_then(|value| value.extract().ok())
                    })
            })
            .collect();
        if names.is_empty() {
            return None;
        }
        names.sort();
        Some(names.join("_"))
    }

    pub(crate) fn run_dist(&mut self, py: Python<'_>, workers: usize) {
        let (batches, nodeid_groups) = self.build_dist_batches(py, workers);

        self.print_dist_banner(workers);

        // The "[gw0] darwin -- Python 3.13.2 /usr/bin/python" failure-repr
        // prefix (upstream getworkerinfoline); workers share our interpreter.
        self.session.worker_platinfo = py
            .import("sys")
            .and_then(|sys| {
                let platform: String = sys.getattr("platform")?.extract()?;
                let executable: String = sys.getattr("executable")?.extract()?;
                let version: (u32, u32, u32) = sys
                    .getattr("version_info")?
                    .get_item(pyo3::types::PySlice::new(py, 0, 3, 1))?
                    .extract()?;
                Ok(format!(
                    "{platform} -- Python {}.{}.{} {executable}",
                    version.0, version.1, version.2
                ))
            })
            .ok();

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

        // xdist data exchange: one controller-side node per worker;
        // conftest pytest_configure_node hooks fill node.workerinput
        // before the worker starts.
        let configure_node_hooks: Vec<Py<pyo3::PyAny>> = self
            .session
            .py_hooks
            .iter()
            .filter(|hook| hook.name == "pytest_configure_node")
            .map(|hook| hook.func.clone_ref(py))
            .collect();
        let nodes: Vec<Option<Py<pyo3::PyAny>>> = (0..workers)
            .map(|index| {
                let node =
                    crate::python::make_worker_node(py, index, workers, &testrun_uid, &self.config)
                        .ok()?;
                for func in &configure_node_hooks {
                    if let Err(err) =
                        crate::python::call_py_hook(py, func, &[("node", node.clone_ref(py))])
                    {
                        eprintln!(
                            "INTERNAL ERROR: {}",
                            crate::python::format_exception(py, &err)
                        );
                    }
                }
                Some(node)
            })
            .collect();
        // The (possibly hook-extended) workerinput each worker receives.
        let worker_inputs: Vec<Option<String>> = nodes
            .iter()
            .map(|node| {
                node.as_ref()
                    .and_then(|node| crate::python::worker_node_input_json(py, node))
            })
            .collect();

        // Fork workers before any thread exists (fork + threads don't
        // mix); a worker that fails to fork falls back to spawning.
        // Explicit --tx specs mean "fresh subprocess" (upstream popen):
        // spawn so worker-side pytest_configure runs with workerinput set.
        #[cfg(unix)]
        let mut initial: Vec<Option<WorkerProc>> = if std::env::var_os("PYTEST_RS_DIST_SPAWN")
            .is_none()
            && self.config.get_value("tx").is_none()
        {
            self.fork_workers(py, workers, &testrun_uid, &worker_inputs)
        } else {
            (0..workers).map(|_| None).collect()
        };
        #[cfg(not(unix))]
        let mut initial: Vec<Option<WorkerProc>> = (0..workers).map(|_| None).collect();

        let worker_chdirs = self.config.tx_worker_chdirs();
        // Pre-assign all batches round-robin so scheduling order is
        // deterministic: batch 0→gw0, batch 1→gw1, batch 2→gw0, …
        // This matches CPython xdist's loadscope/loadfile schedulers
        // which push work to specific workers, not a shared queue.
        let mut per_worker: Vec<VecDeque<Vec<String>>> =
            (0..workers).map(|_| VecDeque::new()).collect();
        {
            let mut state = queue.state.lock().expect("work queue lock poisoned");
            let mut idx = 0;
            while let Some(batch) = state.queue.pop_front() {
                per_worker[idx % workers].push_back(batch);
                idx += 1;
            }
        }
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
                workerinput_json: worker_inputs.get(index).cloned().flatten(),
                chdir: worker_chdirs
                    .as_ref()
                    .and_then(|chdirs| chdirs.get(index).cloned())
                    .flatten(),
                initial: slot.take(),
                assigned: std::mem::take(&mut per_worker[index]),
            };
            handles.push(std::thread::spawn(move || owner.run()));
        }
        drop(sender);

        // Merge loop: progress streams in arrival order (xdist-style).
        let (reports, extras, failed, maxfail_hit) =
            self.run_dist_merge_loop(py, receiver, &queue, &nodes, &nodeid_groups);
        for handle in handles {
            let _ = handle.join();
        }

        if maxfail_hit {
            // Upstream DSession: -x/--maxfail interrupts the session (exit
            // 2) with the "stopping after N failures" banner.
            self.session.stopped_after = Some(failed);
            self.session.exit_code_override = Some(crate::report::exit_code::INTERRUPTED);
        }

        // All workers are down: conftest pytest_testnodedown hooks see the
        // final node.workeroutput (upstream fires one per departing node).
        let testnodedown_hooks: Vec<Py<pyo3::PyAny>> = self
            .session
            .py_hooks
            .iter()
            .filter(|hook| hook.name == "pytest_testnodedown")
            .map(|hook| hook.func.clone_ref(py))
            .collect();
        if !testnodedown_hooks.is_empty() {
            for node in nodes.iter().flatten() {
                for func in &testnodedown_hooks {
                    if let Err(err) = crate::python::call_py_hook(
                        py,
                        func,
                        &[("node", node.clone_ref(py)), ("error", py.None())],
                    ) {
                        eprintln!(
                            "INTERNAL ERROR: {}",
                            crate::python::format_exception(py, &err)
                        );
                    }
                }
            }
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

    /// Partition collected items into worker batches for the active dist
    /// mode (loadscope/loadfile/loadgroup fold per file/group; `each`
    /// duplicates the queue per worker) and reorder largest-first. Returns
    /// the batch queue and the nodeid -> xdist_group map (for -v display).
    fn build_dist_batches(
        &self,
        py: Python<'_>,
        workers: usize,
    ) -> (VecDeque<Vec<String>>, HashMap<String, String>) {
        let dist_mode = self.config.get_value("dist").unwrap_or("load");
        let per_module = matches!(dist_mode, "loadscope" | "loadfile" | "loadgroup");

        // loadgroup: same-group items always batch together (one worker),
        // and their -v nodeids display as "nodeid@group".
        let mut nodeid_groups: HashMap<String, String> = HashMap::new();
        let mut group_batches: HashMap<String, usize> = HashMap::new();
        let mut batches: VecDeque<Vec<String>> = VecDeque::new();
        for item in &self.session.items {
            if dist_mode == "loadgroup"
                && let Some(group) = Self::xdist_group_of(py, item)
            {
                nodeid_groups.insert(item.nodeid.clone(), group.clone());
                match group_batches.get(&group) {
                    Some(&index) => batches[index].push(item.nodeid.clone()),
                    None => {
                        group_batches.insert(group, batches.len());
                        batches.push_back(vec![item.nodeid.clone()]);
                    }
                }
                continue;
            }
            let file = item.nodeid.split("::").next().unwrap_or("");
            let same_module = per_module
                && batches.back().is_some_and(|batch: &Vec<String>| {
                    batch.first().is_some_and(|first| {
                        first.split("::").next().unwrap_or("") == file
                            // Never fold ungrouped items into a group batch.
                            && !nodeid_groups.contains_key(first)
                    })
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
        // loadscope/loadfile/loadgroup reorder the work queue by descending
        // unit size by default (xdist LoadScopeScheduling.schedule, gated on
        // --loadscope-reorder / --no-loadscope-reorder; default on). The sort
        // is stable, so equal-size units keep collection order. This is what
        // sends the largest scope to the first available worker.
        let reorder = per_module && !self.config.get_flag("no-loadscope-reorder");
        if reorder {
            let mut ordered: Vec<Vec<String>> = batches.into_iter().collect();
            ordered.sort_by_key(|batch| std::cmp::Reverse(batch.len()));
            batches = ordered.into();
        }

        if dist_mode == "each" {
            // every test runs on every worker
            let base: Vec<Vec<String>> = batches.iter().cloned().collect();
            for _ in 1..workers {
                batches.extend(base.iter().cloned());
            }
        }
        (batches, nodeid_groups)
    }

    /// Drain worker events in arrival order: stream progress, accumulate
    /// reports/extras, drive the delegated reporter, and honor the shared
    /// --maxfail budget. Returns (reports, plugin extras, failed count,
    /// whether --maxfail tripped).
    fn run_dist_merge_loop(
        &mut self,
        py: Python<'_>,
        receiver: mpsc::Receiver<Event>,
        queue: &Arc<WorkQueue>,
        nodes: &[Option<Py<pyo3::PyAny>>],
        nodeid_groups: &HashMap<String, String>,
    ) -> (Vec<TestReport>, Vec<(String, String)>, usize, bool) {
        let mut reports: Vec<TestReport> = Vec::new();
        let mut extras: Vec<(String, String)> = Vec::new();
        let show_progress =
            !self.config.quiet && !self.config.no_terminal() && self.config.verbose == 0;
        // The console_output_style progress field (percent/count/times/none)
        // shown after the chars and on each -v line.
        let pkind = self.config.progress_kind();
        let mut printed = 0usize;
        let mut total_dur = std::time::Duration::ZERO;
        // Outcome lines printed so far (the -v progress percentage).
        let mut verbose_done = 0usize;
        // -x/--maxfail across all workers: stop dispatching once reached
        // (workers drain their running batches; exit is INTERRUPTED, the
        // upstream DSession behavior).
        let maxfail = self.config.maxfail();
        let mut failed = 0usize;
        let mut maxfail_hit = false;
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
                            // Subtest reports use "{desc} SUBFAIL/SUBPASS" (description
                            // first, then the short word) to match real xdist output.
                            let word = if let Some(desc) = &report.subtest_desc {
                                let short = match report.outcome {
                                    Outcome::Failed => "SUBFAIL",
                                    Outcome::Skipped => "SUBSKIP",
                                    Outcome::XFailed => "SUBXFAIL",
                                    _ => "SUBPASS",
                                };
                                format!("{desc} {short}")
                            } else {
                                crate::runner::outcome_word(&report)
                            };
                            // xdist verbose format: the relayed logstart
                            // line, then "[gw0] [ 50%] PASSED test_a.py::test"
                            // (loadgroup nodeids display as "nodeid@group").
                            verbose_done += 1;
                            let total = self.session.items.len().max(1);
                            let msg = crate::runner::progress_message(
                                pkind,
                                verbose_done.min(total),
                                total,
                                report.duration,
                            );
                            let display = match nodeid_groups.get(&report.nodeid) {
                                Some(group) => format!("{}@{group}", report.nodeid),
                                None => report.nodeid.clone(),
                            };
                            println!("{display} ");
                            // xdist verbose: "[gwN] <progress> WORD nodeid "
                            // (pytest writes the progress message + a space,
                            // then the word and the locationline, which itself
                            // ends with a trailing space).
                            if msg.is_empty() {
                                println!("[gw{worker}] {word} {display} ");
                            } else {
                                println!("[gw{worker}] {msg} {word} {display} ");
                            }
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
                    if report.phase == Phase::Call {
                        total_dur += report.duration;
                    }
                    if report.outcome == Outcome::Failed {
                        // The failure repr's "[gw0] darwin -- Python ..." line.
                        self.session
                            .report_workers
                            .insert(report.nodeid.clone(), worker);
                        failed += 1;
                        if let Some(max) = maxfail
                            && failed >= max
                            && !maxfail_hit
                        {
                            maxfail_hit = true;
                            queue.stop();
                        }
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
                Event::Workeroutput { worker, payload } => {
                    if let Some(Some(node)) = nodes.get(worker) {
                        crate::python::worker_node_set_output(py, node, &payload);
                    }
                }
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
            // Close the progress char line with the right-aligned progress
            // field (pytest's end-of-loop "[100%]" / "[20/20]" / duration).
            let total = self.session.items.len().max(1);
            let msg = crate::runner::progress_message(pkind, total, total, total_dur);
            let color = if failed > 0 {
                crate::tw::RED
            } else {
                crate::tw::GREEN
            };
            let body = " ".repeat(printed % 80);
            println!("{}", crate::runner::progress_suffix(&body, &msg, color));
        }
        (reports, extras, failed, maxfail_hit)
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
        worker_inputs: &[Option<String>],
    ) -> Vec<Option<WorkerProc>> {
        use std::os::fd::FromRawFd;

        const ENV_KEYS: [&str; 4] = [
            "PYTEST_XDIST_WORKER",
            "PYTEST_XDIST_WORKER_COUNT",
            "PYTEST_XDIST_TESTRUNUID",
            "PYTEST_RS_WORKERINPUT",
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
                match worker_inputs.get(index).and_then(Option::as_deref) {
                    Some(json) => {
                        let _ = environ.set_item(ENV_KEYS[3], json);
                    }
                    None => {
                        let _ = environ.call_method1("pop", (ENV_KEYS[3], py.None()));
                    }
                }
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
    /// node.workerinput as JSON (pytest_configure_node additions).
    workerinput_json: Option<String>,
    /// --tx popen//chdir=DIR: the worker's working directory.
    chdir: Option<String>,
    /// A pre-forked worker for this slot; replacements (after a crash)
    /// always spawn — re-forking is unsafe once threads exist.
    initial: Option<WorkerProc>,
    /// Batches pre-assigned to this worker by round-robin so scheduling
    /// order is deterministic (batch 0→gw0, batch 1→gw1, batch 2→gw0, …).
    assigned: VecDeque<Vec<String>>,
}

impl WorkerOwner {
    fn spawn(&self) -> std::io::Result<WorkerProc> {
        let exe = std::env::current_exe()?;
        let mut command = Command::new(exe);
        command
            .args(&self.argv)
            .arg("--worker")
            .env("PYTEST_XDIST_WORKER", format!("gw{}", self.index))
            .env("PYTEST_XDIST_WORKER_COUNT", self.worker_count.to_string())
            .env("PYTEST_XDIST_TESTRUNUID", &self.testrun_uid)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(json) = &self.workerinput_json {
            command.env("PYTEST_RS_WORKERINPUT", json);
        }
        if let Some(dir) = &self.chdir {
            command.current_dir(dir);
        }
        let mut child = command.spawn()?;
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
        // Upstream's pytest_testnodedown narration for a crashed node.
        let _ = self.sender.send(Event::Output(format!(
            "[gw{}] node down: Not properly terminated",
            self.index
        )));
        if let (Some(running), CrashAction::Replace | CrashAction::Abort) = (&running, &action) {
            // Crashes always count as call failures (xdist behavior): even if the
            // call phase completed and the crash was in teardown, reporting as
            // Phase::Teardown would show as an "error" instead of "failed".
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
                    rerun: false,
                    xfail_longrepr: None,
                    reprcrash_message: None,
                    head_line: None,
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
            if self.queue.is_stopped() && !self.assigned.is_empty() {
                // -x/--maxfail fired: drain pre-assigned but unstarted batches
                // without running them, then shut down the worker.
                self.assigned.clear();
            }
            let Some(batch) = self.assigned.pop_front().or_else(|| self.queue.next()) else {
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
                    Some(WorkerMsg::Workeroutput { payload }) => {
                        let _ = self.sender.send(Event::Workeroutput {
                            worker: self.index,
                            payload,
                        });
                    }
                    Some(WorkerMsg::Bye) => break 'work,
                    None => {
                        let _ = self.sender.send(Event::Output(line));
                    }
                }
            }
        }

        // Drain post-shutdown frames: final scope-teardown failure reports,
        // warnings, plugin dumps, bye.
        for line in proc.lines {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                // encode_frame's leading newline (and stray blank output).
                continue;
            }
            match decode_frame(&line) {
                Some(WorkerMsg::Report { report }) => {
                    let _ = self.sender.send(Event::Report {
                        report,
                        worker: self.index,
                    });
                }
                Some(WorkerMsg::Extra { plugin, payload }) => {
                    let _ = self.sender.send(Event::Extra { plugin, payload });
                }
                Some(WorkerMsg::Warnings { lines, count }) => {
                    let _ = self.sender.send(Event::Warnings { lines, count });
                }
                Some(WorkerMsg::Workeroutput { payload }) => {
                    let _ = self.sender.send(Event::Workeroutput {
                        worker: self.index,
                        payload,
                    });
                }
                Some(WorkerMsg::Bye) => {}
                Some(_) => {}
                None => {
                    let _ = self.sender.send(Event::Output(line));
                }
            }
        }
        proc.handle.wait();
    }
}
