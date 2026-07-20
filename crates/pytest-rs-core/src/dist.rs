//! Distributed execution (-n N): start N workers, feed them batches of
//! node IDs from a shared queue (work stealing: fast workers pull more),
//! and merge the streamed reports plus per-plugin state dumps.
//!
//! On unix (the default), workers fork off the parent at a checkpoint
//! right after `collect_pre_configure` — imports (interpreter, `-p`/
//! entry-point plugins, every reachable conftest) arrive copy-on-write, so
//! workers skip that per-process cost upstream xdist pays. The checkpoint
//! sits strictly *before* `pytest_configure` fires anywhere, so each forked
//! child still independently fires its own `pytest_plugins_registered`-
//! onward pipeline afterward (blocking first on a `ParentMsg::Workerinput`
//! delivery — see `worker.rs::run_worker_forked`) — configure semantics
//! identical to a spawned worker, just without redoing the imports.
//! `PYTEST_RS_DIST_SPAWN=1`, or non-unix, always spawns instead: each
//! worker is the same binary in a hidden `--worker` mode that re-imports
//! and re-configures independently from scratch (upstream xdist's model).
//!
//! Dispatch granularity follows --dist: per-test for load/worksteal (the
//! default, xdist parity), per-module for loadscope/loadfile/loadgroup
//! (each module imported by one worker), duplicated per worker for each.
//! Crashed workers fail their running test, requeue the rest, and are
//! replaced while --max-worker-restart's budget lasts; an exhausted budget
//! aborts undispatched work (xdist's shutdown semantics).

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Lines, Read, Write};
use std::process::{Child, Command, Stdio};
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
    /// A worker's config.workeroutput JSON (xdist data exchange).
    Workeroutput {
        worker: usize,
        payload: String,
    },
    /// Passthrough/diagnostic output, printed as-is.
    Output(String),
    /// A fatal distribution condition, shown as a banner before the summary.
    Banner(String),
    /// A worker session was interrupted (KeyboardInterrupt / pytest.exit).
    Interrupted {
        code: i32,
        banner: Option<String>,
    },
    /// Worker finished its precollect phase. Carries the full item set
    /// and any import errors so the merge loop can build batches.
    Collection {
        worker: usize,
        nodeids: Vec<String>,
        xdist_groups: Vec<Option<String>>,
        errors: Vec<(String, String)>,
        deselected: usize,
    },
}

/// The shared work queue. Workers block on `next_blocking` until the merge
/// loop pushes batches (worker-side collection) or until aborted/stopped.
/// Batches are pre-assigned round-robin to per-worker queues so that
/// loadscope/loadfile scheduling is stable: each worker works through its
/// own pre-assigned modules in order (matching upstream xdist behaviour).
/// Crash bookkeeping lives under the same lock so concurrent crashes resolve
/// deterministically: a crashed worker requeues its remainder onto its own
/// slot so the replacement picks it up; the crash that exhausts the restart
/// budget aborts whatever was not yet dispatched; crashes that land after the
/// abort are silent (their tests count as undispatched, not failed — xdist's
/// shutdown semantics).
struct WorkQueue {
    num_workers: usize,
    state: Mutex<QueueState>,
    /// Notified when batches become available (push_batches), or when the
    /// queue is stopped/aborted — so next_blocking() can unblock.
    ready: Condvar,
}

struct QueueState {
    /// Per-worker FIFO queues; index by worker index.
    queues: Vec<VecDeque<Vec<String>>>,
    aborted: bool,
    /// True once -x/--maxfail fires: workers must not start new batches.
    soft_stopped: bool,
    /// Remaining worker-restart budget (no flag = effectively unlimited).
    restarts: isize,
    /// True once push_batches(), stop(), or crash()-abort has been called.
    batches_ready: bool,
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
    fn new(restarts: isize, num_workers: usize) -> Self {
        Self {
            num_workers,
            state: Mutex::new(QueueState {
                queues: vec![VecDeque::new(); num_workers],
                aborted: false,
                soft_stopped: false,
                restarts,
                batches_ready: false,
            }),
            ready: Condvar::new(),
        }
    }

    /// Called by the merge loop once all workers have reported Collection.
    /// Distributes batches round-robin across per-worker queues (matching
    /// upstream xdist's pre-assignment order) and wakes all waiters.
    fn push_batches(&self, batches: VecDeque<Vec<String>>) {
        let mut state = self.state.lock().expect("work queue lock poisoned");
        if !state.aborted && !state.soft_stopped {
            for (i, batch) in batches.into_iter().enumerate() {
                state.queues[i % self.num_workers].push_back(batch);
            }
        }
        state.batches_ready = true;
        self.ready.notify_all();
    }

    /// Block until batches are available (or the run is stopped/aborted),
    /// then pop and return the next batch for this worker, or None when done.
    fn next_blocking(&self, worker_idx: usize) -> Option<Vec<String>> {
        let mut state = self.state.lock().expect("work queue lock poisoned");
        loop {
            if state.batches_ready || state.aborted {
                break;
            }
            state = self.ready.wait(state).expect("work queue condvar poisoned");
        }
        if state.aborted || state.soft_stopped {
            return None;
        }
        state.queues[worker_idx].pop_front()
    }

    /// -x/--maxfail: stop dispatching new batches; workers finish what
    /// they hold (upstream DSession waits for workers before interrupting).
    fn stop(&self) {
        let mut state = self.state.lock().expect("work queue lock poisoned");
        for q in &mut state.queues {
            q.clear();
        }
        state.soft_stopped = true;
        state.batches_ready = true;
        self.ready.notify_all();
    }

    /// Block until the merge loop has called `stop()` (soft_stopped or aborted).
    /// Called by the worker owner after forwarding `Event::Interrupted` so that
    /// the next `next_blocking()` call is guaranteed to see `soft_stopped == true`
    /// and return `None` instead of dispatching the next batch.
    fn wait_soft_stopped(&self) {
        let mut state = self.state.lock().expect("work queue lock poisoned");
        while !state.soft_stopped && !state.aborted {
            state = self.ready.wait(state).expect("work queue condvar poisoned");
        }
    }

    /// Crash bookkeeping, atomically: spend a restart and requeue the
    /// unfinished remainder onto this worker's slot, or exhaust the budget.
    fn crash(&self, worker_idx: usize, remaining: Vec<String>) -> CrashAction {
        let mut state = self.state.lock().expect("work queue lock poisoned");
        if state.aborted {
            return CrashAction::Silent;
        }
        if state.restarts > 0 {
            state.restarts -= 1;
            if !remaining.is_empty() {
                state.queues[worker_idx].push_front(remaining);
            }
            CrashAction::Replace
        } else {
            state.aborted = true;
            for q in &mut state.queues {
                q.clear();
            }
            state.batches_ready = true;
            self.ready.notify_all();
            CrashAction::Abort
        }
    }
}

impl Engine {
    /// Print "created: N/N workers" (and optional scheduler line for -v).
    /// Item count ("N workers [M items]") is printed separately by the merge
    /// loop once all workers have reported their collected nodeids.
    fn print_dist_created_line(&self, workers: usize) {
        if self.config.no_terminal() {
            return;
        }
        if self.config.quiet {
            println!("bringing up nodes...");
            return;
        }
        let noun = if workers == 1 { "worker" } else { "workers" };
        println!("created: {workers}/{workers} {noun}");
        if self.config.verbose > 0 {
            let dist_mode = self.config.get_value("dist").unwrap_or("load");
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
    }

    /// The controller banner for the collection-error abort path:
    /// "created: N/N workers" + "N workers [M items]" (uses session.items).
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

    pub(crate) fn run_dist(&mut self, py: Python<'_>, workers: usize) {
        // Print "created: N/N workers" immediately (item count comes later
        // from the merge loop once all workers report their Collections).
        self.print_dist_created_line(workers);

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

        // Empty queue: batches are pushed by the merge loop once all workers
        // have reported their Collection message.
        let queue = Arc::new(WorkQueue::new(max_restart.unwrap_or(isize::MAX), workers));
        let (sender, receiver) = mpsc::channel::<Event>();
        // effective_args holds the clap-known args (ini addopts + PYTEST_ADDOPTS
        // + command line); plugin_args holds deferred plugin flags (--reuse-db,
        // --create-db, --ds, ...) that clap doesn't know. A spawned worker
        // re-parses its argv from scratch, so without plugin_args it loses
        // those flags (e.g. --reuse-db defaults to False and the worker
        // destroys its test database at teardown). Forked workers inherit the
        // parsed config and don't have this problem.
        let mut argv: Vec<String> = self.config.effective_args.iter().skip(1).cloned().collect();
        argv.extend(self.config.plugin_args.iter().cloned());
        // One uid for the whole distributed run (the testrun_uid fixture).
        // Generated (and, if forking, already handed to each child's
        // pre-fork env) by the collect_pre_configure checkpoint — reused
        // here so a forked child's env and this run's computed workerinput
        // agree. Falls back to generating fresh only if the checkpoint
        // somehow never ran (defensive; every path that reaches run_dist
        // goes through collect_pre_configure first).
        let testrun_uid = self.dist_testrun_uid.take().unwrap_or_else(|| {
            format!(
                "{:032x}",
                std::process::id() as u128
                    ^ std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|elapsed| elapsed.as_nanos())
                        .unwrap_or(0)
            )
        });

        // xdist data exchange: one controller-side node per worker;
        // conftest pytest_configure_node hooks fill node.workerinput
        // before the worker starts.
        let mut configure_node_hooks: Vec<Py<pyo3::PyAny>> = self
            .session
            .py_hooks
            .iter()
            .filter(|hook| hook.name == "pytest_configure_node")
            .map(|hook| hook.func.clone_ref(py))
            .collect();
        // session.py_hooks is a snapshot scanned from conftest/entry-point
        // MODULES at load time; it never sees a hook-providing *object*
        // registered dynamically via `config.pluginmanager.register(obj)`
        // from inside another hook (e.g. pytest-randomly's own
        // pytest_configure does `if pm.hasplugin("xdist"):
        // pm.register(XdistHooks())`). The Python pluginmanager's own
        // `_plugins` list does track that registration, so ask it directly
        // and merge in anything session.py_hooks missed.
        if let Ok(impls) = py
            .import("pytest._pluginmanager")
            .and_then(|m| m.getattr("pluginmanager"))
            .and_then(|pm| pm.getattr("hook"))
            .and_then(|h| h.getattr("pytest_configure_node"))
            .and_then(|hc| hc.call_method0("get_hookimpls"))
            && let Ok(iter) = impls.try_iter()
        {
            for impl_obj in iter.flatten() {
                let Ok(func) = impl_obj.getattr("function") else {
                    continue;
                };
                let already_known = configure_node_hooks
                    .iter()
                    .any(|existing| existing.bind(py).eq(&func).unwrap_or(false));
                if !already_known {
                    configure_node_hooks.push(func.unbind());
                }
            }
        }
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

        let worker_chdirs = self.config.tx_worker_chdirs();

        let rsyncdirs: Vec<String> = self
            .config
            .get_values("rsyncdir")
            .unwrap_or_default()
            .into_iter()
            .map(str::to_string)
            .collect();

        // Workers already forked (or not) at the collect_pre_configure
        // checkpoint, strictly before any pytest_configure fired anywhere —
        // stashed there since fork must happen before any WorkerOwner
        // thread exists (CPython's/glibc's fork-safety both assume a
        // single-threaded parent at fork time), which is long before this
        // point. A crashed forked worker is always replaced by spawn(),
        // never a second fork, once those threads are live.
        let mut forked = std::mem::take(&mut self.forked_workers);

        let mut handles = Vec::new();
        for index in 0..workers {
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
                rsyncdirs: rsyncdirs.clone(),
            };
            let initial = forked.get_mut(index).and_then(Option::take);
            handles.push(std::thread::spawn(move || owner.run(initial)));
        }
        drop(sender);

        // Merge loop: progress streams in arrival order (xdist-style).
        // `workers` is the initial collections_pending count; the loop
        // builds and pushes batches once all workers have reported.
        let (reports, extras, failed, maxfail_hit, total_items) =
            self.run_dist_merge_loop(py, receiver, &queue, &nodes, workers);
        // Upstream: zero items collected across the whole distributed run is
        // NO_TESTS_COLLECTED, same as the non-dist path's n_items==0 check —
        // but the controller's own self.session.items is always empty in
        // dist mode (workers collect, not the controller), so finish_session's
        // ordinary failed-report check can't see this on its own.
        self.session.dist_total_items = Some(total_items);
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

    /// Drain worker events in arrival order: stream progress, accumulate
    /// reports/extras, drive the delegated reporter, and honor the shared
    /// --maxfail budget. Returns (reports, plugin extras, failed count,
    /// whether --maxfail tripped, total items collected across all workers).
    fn run_dist_merge_loop(
        &mut self,
        py: Python<'_>,
        receiver: mpsc::Receiver<Event>,
        queue: &Arc<WorkQueue>,
        nodes: &[Option<Py<pyo3::PyAny>>],
        // Number of workers that must send Collection before batches are built.
        mut collections_pending: usize,
    ) -> (Vec<TestReport>, Vec<(String, String)>, usize, bool, usize) {
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
        // Nodeids from the first worker that reports Collection. Later workers
        // are expected to collect identically; if they don't (e.g. random
        // parametrize), we report a mismatch and abort.
        let mut all_nodeids: Vec<String> = Vec::new();
        let mut all_xdist_groups: Vec<Option<String>> = Vec::new();
        let mut got_nodeids = false;
        let mut first_collection_worker: usize = 0;
        // nodeid → xdist_group for verbose display ("nodeid@group").
        let mut nodeid_groups: HashMap<String, String> = HashMap::new();
        // Total items known once all Collections are received.
        let mut total_items: usize = 0;
        // Dedup collection errors: multiple workers report the same import
        // errors (each tries to import the failing module), so deduplicate
        // by (nodeid, message) to avoid multiplied output lines.
        let mut seen_errors: HashSet<(String, String)> = HashSet::new();
        let workers = collections_pending;
        for event in receiver {
            match event {
                Event::Collection {
                    worker,
                    nodeids,
                    xdist_groups,
                    errors,
                    deselected,
                } => {
                    // Process collection errors: add to session and reports.
                    for (nodeid, err) in errors {
                        if !seen_errors.insert((nodeid.clone(), err.clone())) {
                            continue; // duplicate from another worker
                        }
                        self.session
                            .collect_errors
                            .push((nodeid.clone(), err.clone()));
                        crate::python::reporter_collect_error(py, &nodeid, &err);
                        reports.push(TestReport {
                            nodeid,
                            phase: Phase::Setup,
                            outcome: Outcome::Failed,
                            duration: std::time::Duration::ZERO,
                            longrepr: Some(err),
                            location: None,
                            subtest_desc: None,
                            sections: Vec::new(),
                            rerun: false,
                            xfail_longrepr: None,
                            reprcrash_message: None,
                            head_line: None,
                        });
                    }
                    // Only use the first worker's nodeids. Later workers
                    // must collect the same items; if they don't (e.g. due
                    // to random parametrize) abort with a clear error.
                    if !got_nodeids {
                        got_nodeids = true;
                        first_collection_worker = worker;
                        all_nodeids = nodeids;
                        all_xdist_groups = xdist_groups;
                        // Every worker selects independently but identically
                        // (guaranteed by the mismatch check below) — take the
                        // count from the first worker only, don't sum across
                        // workers.
                        self.session.deselected = deselected;
                    } else if nodeids != all_nodeids {
                        if line_open {
                            println!();
                            line_open = false;
                        }
                        println!(
                            "Different tests were collected between gw{first_collection_worker} \
                             and gw{worker}"
                        );
                        queue.stop();
                        self.session.exit_code_override =
                            Some(crate::report::exit_code::TESTS_FAILED);
                    }
                    collections_pending = collections_pending.saturating_sub(1);
                    if collections_pending == 0 {
                        // All workers have reported: build nodeid_groups map
                        // for verbose display, build batches, push to queue.
                        total_items = all_nodeids.len();
                        // session.testscollected is normally len(session.items),
                        // but in dist mode items are empty — set the override.
                        let _ = crate::python::set_session_testscollected(py, total_items);
                        for (nodeid, group) in all_nodeids.iter().zip(all_xdist_groups.iter()) {
                            if let Some(g) = group {
                                nodeid_groups.insert(nodeid.clone(), g.clone());
                            }
                        }
                        let batches = self.build_dist_batches_from_nodeids(
                            &all_nodeids,
                            &all_xdist_groups,
                            workers,
                        );
                        // Print "N workers [M items]" after building batches.
                        if !self.config.no_terminal() && !self.config.quiet {
                            let noun = if workers == 1 { "worker" } else { "workers" };
                            let item_noun = if total_items == 1 { "item" } else { "items" };
                            if line_open {
                                println!();
                                line_open = false;
                            }
                            println!("{workers} {noun} [{total_items} {item_noun}]");
                        }
                        queue.push_batches(batches);
                    }
                }
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
                            let total = total_items.max(1);
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
                    if !self.config.no_terminal() {
                        if line_open {
                            println!();
                            line_open = false;
                        }
                        println!("{line}");
                    }
                }
                Event::Banner(message) => {
                    self.session.dist_banner.get_or_insert(message);
                }
                Event::Interrupted { code, banner } => {
                    if !maxfail_hit {
                        maxfail_hit = true;
                        queue.stop();
                        self.session.exit_code_override = Some(code);
                        self.session.abort_banner = banner;
                    }
                }
            }
        }
        if line_open {
            // Close the progress char line with the right-aligned progress
            // field (pytest's end-of-loop "[100%]" / "[20/20]" / duration).
            let total = total_items.max(1);
            let msg = crate::runner::progress_message(pkind, total, total, total_dur);
            let color = if failed > 0 {
                crate::tw::RED
            } else {
                crate::tw::GREEN
            };
            let body = " ".repeat(printed % 80);
            println!("{}", crate::runner::progress_suffix(&body, &msg, color));
        }
        (reports, extras, failed, maxfail_hit, total_items)
    }

    /// Partition nodeids (from worker Collections) into work batches for the
    /// active dist mode. Used in the merge loop once all workers have reported.
    fn build_dist_batches_from_nodeids(
        &self,
        nodeids: &[String],
        xdist_groups: &[Option<String>],
        workers: usize,
    ) -> VecDeque<Vec<String>> {
        let dist_mode = self.config.get_value("dist").unwrap_or("load");
        // Plain `load` (no scope/file/group grouping): upstream's initial
        // distribution is round-robin over CHUNKS of consecutive items, not
        // one at a time, once there are enough items to give every worker
        // at least 2 (xdist.scheduler.load.LoadScheduling.schedule). Below
        // that threshold it falls back to one-at-a-time, same as a chunk
        // size of 1. Keeping adjacent items (e.g. two tests in the same
        // unittest.TestCase, sharing setUpClass/tearDownClass) on the same
        // worker matters beyond just ordering — splitting them across
        // workers double-runs class-scoped setup/teardown.
        if dist_mode == "load" && workers > 0 {
            let chunk_size = if nodeids.len() < 2 * workers {
                1
            } else {
                let items_per_node = nodeids.len() / workers;
                (items_per_node / 4).max(2)
            };
            return nodeids.chunks(chunk_size).map(<[String]>::to_vec).collect();
        }
        let per_module = matches!(dist_mode, "loadscope" | "loadfile" | "loadgroup");
        // Insertion-ordered group-by keyed by scope, not by adjacency in
        // `nodeids`: pytest-order and similar reordering plugins legitimately
        // interleave same-scope items across other scopes (e.g. session-scope
        // sorting can produce file_a, file_b, file_a, file_b, ...), and
        // upstream xdist's own LoadFileScheduling/LoadScopeScheduling group by
        // an OrderedDict keyed on scope regardless of position. Keys are
        // namespaced ("group:"/"file:") so a group name can never collide
        // with a file path that happens to be the same string.
        let mut group_batches: HashMap<String, usize> = HashMap::new();
        let mut batches: VecDeque<Vec<String>> = VecDeque::new();

        for (nodeid, xdist_group) in nodeids.iter().zip(xdist_groups.iter()) {
            let key = if dist_mode == "loadgroup"
                && let Some(group) = xdist_group
            {
                format!("group:{group}")
            } else if per_module {
                let file = nodeid.split("::").next().unwrap_or("");
                format!("file:{file}")
            } else {
                batches.push_back(vec![nodeid.clone()]);
                continue;
            };
            match group_batches.get(&key) {
                Some(&index) => batches[index].push(nodeid.clone()),
                None => {
                    group_batches.insert(key, batches.len());
                    batches.push_back(vec![nodeid.clone()]);
                }
            }
        }

        let reorder = per_module && !self.config.get_flag("no-loadscope-reorder");
        if reorder {
            let mut ordered: Vec<Vec<String>> = batches.into_iter().collect();
            ordered.sort_by_key(|batch| std::cmp::Reverse(batch.len()));
            batches = ordered.into();
        }

        if dist_mode == "each" {
            let base: Vec<Vec<String>> = batches.iter().cloned().collect();
            for _ in 1..workers {
                batches.extend(base.iter().cloned());
            }
        }
        batches
    }

    /// Called once from the tail of `collect_pre_configure`, before any
    /// `pytest_configure` has fired anywhere: if `resolve_numprocesses`
    /// resolved a nonzero worker count (cached in
    /// `session.dist_workers_resolved`), rsync `--rsyncdir` into every
    /// worker's chdir (must land before ANY worker — forked or spawned —
    /// might try to collect from it; a forked worker can start reading its
    /// chdir immediately after this returns, long before `run_dist`'s own
    /// sequential setup would otherwise get to it), generate this run's
    /// testrun_uid, and (when fork-eligible) fork all workers now, while
    /// this is still the only running thread — the hard requirement fork
    /// has everywhere else in this file. A no-op when not distributing.
    #[cfg(feature = "xdist")]
    pub(crate) fn fork_workers_at_checkpoint(&mut self, py: Python<'_>) {
        let Some(Some(workers)) = self.session.dist_workers_resolved else {
            return;
        };
        if workers == 0 {
            return;
        }
        self.rsync_worker_chdirs();
        let testrun_uid = format!(
            "{:032x}",
            std::process::id() as u128
                ^ std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_nanos())
                    .unwrap_or(0)
        );
        self.forked_workers = self.maybe_fork_workers(py, workers, &testrun_uid);
        self.dist_testrun_uid = Some(testrun_uid);
    }

    /// `--rsyncdir`: copy each specified directory or file into every
    /// worker's `--tx popen//chdir=DIR`. A no-op when neither is set.
    fn rsync_worker_chdirs(&self) {
        let Some(chdirs) = self.config.tx_worker_chdirs() else {
            return;
        };
        let Some(rsyncdirs) = self.config.get_values("rsyncdir") else {
            return;
        };
        let unique_chdirs: std::collections::HashSet<&str> =
            chdirs.iter().flatten().map(String::as_str).collect();
        for chdir in unique_chdirs {
            let dest_base = std::path::Path::new(chdir);
            for src_str in &rsyncdirs {
                let src = std::path::Path::new(src_str);
                let Some(name) = src.file_name() else {
                    continue;
                };
                if src.is_dir() {
                    let _ = copy_dir_recursive(src, &dest_base.join(name));
                } else if src.is_file() {
                    let _ = std::fs::copy(src, dest_base.join(name));
                }
            }
        }
    }

    /// Called from `collect_pre_configure`'s checkpoint (before any
    /// `pytest_configure` has fired anywhere). Workers fork off the
    /// already-warm parent interpreter by default (unix only), skipping the
    /// per-process interpreter/plugin/conftest import cost upstream xdist
    /// pays; a forked child still fires its own `pytest_configure`
    /// independently, once identity/workerinput arrive over IPC (see
    /// `worker.rs::run_worker_forked`), so configure semantics match a
    /// spawned worker exactly. `PYTEST_RS_DIST_SPAWN=1` opts back into a
    /// fresh subprocess per worker (upstream xdist's own model) instead.
    #[cfg(unix)]
    pub(crate) fn maybe_fork_workers(
        &mut self,
        py: Python<'_>,
        count: usize,
        testrun_uid: &str,
    ) -> Vec<Option<WorkerProc>> {
        if std::env::var_os("PYTEST_RS_DIST_SPAWN").is_some() {
            (0..count).map(|_| None).collect()
        } else {
            self.fork_workers(py, count, testrun_uid)
        }
    }

    #[cfg(not(unix))]
    pub(crate) fn maybe_fork_workers(
        &mut self,
        _py: Python<'_>,
        count: usize,
        _testrun_uid: &str,
    ) -> Vec<Option<WorkerProc>> {
        (0..count).map(|_| None).collect()
    }

    /// Fork one child per worker slot off the already-imported parent
    /// interpreter. The parent sets the xdist worker identity env vars
    /// through os.environ right before each fork (and restores them after),
    /// so the child holds its identity from its first instruction — visible
    /// to os.register_at_fork callbacks, not just later reads.
    /// `node.workerinput` (from `pytest_configure_node`, computed only after
    /// the parent's own `pytest_configure` fires — strictly after this
    /// point) is NOT set here: it arrives later over the child's stdin pipe
    /// (`ParentMsg::Workerinput`, sent by `WorkerOwner::run` once `run_dist`
    /// computes it), and the child blocks reading it before configuring
    /// itself. Children dup their pipe pair onto stdin/stdout and enter the
    /// worker loop; they never return. A failed fork yields None and that
    /// slot spawns instead.
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
                // --tx popen//chdir=DIR: this worker runs in DIR. A spawned
                // worker gets this for free — its invocation_dir/paths are
                // computed fresh from the OS's actual CWD, which
                // Command::current_dir already set to DIR before that
                // process's own Config ever parses anything. This child
                // instead inherited an already-parsed Config (via fork)
                // whose invocation_dir/paths still reflect the *parent's*
                // original CWD — std::env::set_current_dir alone only moves
                // the OS-level CWD, it doesn't retroactively fix values
                // already computed from the old one, so both need an
                // explicit update here or this worker would try to collect
                // the original (pre-rsync) source tree while sitting in DIR.
                if let Some(Some(dir)) = self
                    .config
                    .tx_worker_chdirs()
                    .as_ref()
                    .and_then(|chdirs| chdirs.get(index))
                    .map(|chdir| chdir.as_ref())
                {
                    let _ = std::env::set_current_dir(dir);
                    if let Ok(cwd) = std::env::current_dir() {
                        self.config.invocation_dir = cwd;
                    }
                    let rsyncdirs: Vec<String> = self
                        .config
                        .get_values("rsyncdir")
                        .unwrap_or_default()
                        .into_iter()
                        .map(str::to_string)
                        .collect();
                    if !rsyncdirs.is_empty() {
                        self.config.paths = rewrite_paths_for_rsync(&self.config.paths, &rsyncdirs);
                    }
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

pub(crate) struct WorkerProc {
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
    /// --rsyncdir values: directories that were rsynced into each worker's chdir.
    rsyncdirs: Vec<String>,
}

/// Rewrite each absolute path under one of `rsyncdirs` (a source directory
/// that was rsync'd into the worker's chdir) to a path relative to that
/// chdir pointing at the rsynced copy — e.g. `/src/pkg/test_a.py` with
/// rsyncdir `/src/pkg` becomes `pkg/test_a.py`. Anything else (a flag, a
/// path outside every rsyncdir) passes through unchanged. Shared by a
/// spawned worker's argv (`WorkerOwner::rewrite_argv_for_rsync`, run once
/// per worker in the controller before it builds the subprocess command
/// line) and a forked worker's already-parsed `config.paths` (rewritten
/// in-place in the child itself, right after it chdirs — see
/// `Engine::fork_workers`'s child branch — since it has no argv to
/// reconstruct, only the already-resolved path list it inherited via fork).
fn rewrite_paths_for_rsync(paths: &[String], rsyncdirs: &[String]) -> Vec<String> {
    paths
        .iter()
        .map(|arg| {
            let path = std::path::Path::new(arg);
            if path.is_absolute() {
                for src_str in rsyncdirs {
                    let src = std::path::Path::new(src_str);
                    if let Ok(rel) = path.strip_prefix(src)
                        && let Some(name) = src.file_name()
                    {
                        return std::path::Path::new(name)
                            .join(rel)
                            .to_string_lossy()
                            .into_owned();
                    }
                }
            }
            arg.clone()
        })
        .collect()
}

impl WorkerOwner {
    /// Rewrite absolute test-path args that fall under an rsync'd directory
    /// to relative paths inside the worker's chdir, so the worker collects
    /// and imports from the rsynced copy rather than the original source.
    fn rewrite_argv_for_rsync(&self) -> Vec<String> {
        if self.chdir.is_none() || self.rsyncdirs.is_empty() {
            return self.argv.clone();
        }
        rewrite_paths_for_rsync(&self.argv, &self.rsyncdirs)
    }

    fn spawn(&self) -> std::io::Result<WorkerProc> {
        let exe = std::env::current_exe()?;
        let argv = self.rewrite_argv_for_rsync();
        let mut command = Command::new(exe);
        command
            .args(&argv)
            .arg("--worker")
            .env("PYTEST_XDIST_WORKER", format!("gw{}", self.index))
            .env("PYTEST_XDIST_WORKER_COUNT", self.worker_count.to_string())
            .env("PYTEST_XDIST_TESTRUNUID", &self.testrun_uid)
            // effective_args already includes PYTEST_ADDOPTS; unset it so the
            // worker subprocess does not re-apply it and double-count options.
            .env_remove("PYTEST_ADDOPTS")
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
        let action = self.queue.crash(self.index, remaining);
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

    /// `initial`: a `WorkerProc` this slot already forked at the
    /// `collect_pre_configure` checkpoint (see
    /// `Engine::fork_workers_at_checkpoint`), before `workerinput_json` was
    /// even known — that child is blocked on its stdin waiting for it.
    /// `None` spawns a fresh subprocess instead, same as always (workerinput
    /// travels via env instead, since a spawned worker configures itself
    /// only after re-parsing its own env on startup).
    fn run(self, initial: Option<WorkerProc>) {
        let was_forked = initial.is_some();
        let mut proc = match initial {
            Some(proc) => proc,
            None => match self.spawn() {
                Ok(proc) => proc,
                Err(err) => {
                    eprintln!("ERROR: failed to spawn worker: {err}");
                    // Unblock the merge loop's collections_pending count.
                    let _ = self.sender.send(Event::Collection {
                        worker: self.index,
                        nodeids: vec![],
                        xdist_groups: vec![],
                        errors: vec![],
                        deselected: 0,
                    });
                    return;
                }
            },
        };

        if was_forked {
            let msg = serde_json::to_string(&ParentMsg::Workerinput {
                payload: self.workerinput_json.clone(),
            })
            .expect("workerinput serializes");
            if writeln!(proc.stdin, "{msg}").is_err() || proc.stdin.flush().is_err() {
                // The child died before it could even receive its identity —
                // same "unblock merge loop" treatment as a failed spawn.
                let _ = self.sender.send(Event::Collection {
                    worker: self.index,
                    nodeids: vec![],
                    xdist_groups: vec![],
                    errors: vec![],
                    deselected: 0,
                });
                return;
            }
        }

        // Collection phase: read from the worker's stdout until it sends
        // WorkerMsg::Collection (after precollect_all) or EOF (crash).
        // Returns Some((nodeids, groups, errors, deselected)) on success,
        // None on crash.
        let collection_msg = loop {
            let Some(line) = proc.lines.next() else {
                break None; // EOF: worker died during precollect
            };
            let Ok(line) = line else {
                break None;
            };
            if line.trim().is_empty() {
                continue;
            }
            match decode_frame(&line) {
                Some(WorkerMsg::Collection {
                    nodeids,
                    xdist_groups,
                    errors,
                    deselected,
                }) => break Some((nodeids, xdist_groups, errors, deselected)),
                Some(WorkerMsg::Bye) => {
                    // Bye before Collection: treat as empty collection + graceful shutdown.
                    let _ = self.sender.send(Event::Collection {
                        worker: self.index,
                        nodeids: vec![],
                        xdist_groups: vec![],
                        errors: vec![],
                        deselected: 0,
                    });
                    proc.handle.wait();
                    return;
                }
                None => {
                    let _ = self.sender.send(Event::Output(line));
                }
                Some(_) => {}
            }
        };

        match collection_msg {
            None => {
                // Worker crashed during precollect (os._exit / segfault).
                // Send empty Collection to unblock merge loop, then handle crash.
                let _ = self.sender.send(Event::Collection {
                    worker: self.index,
                    nodeids: vec![],
                    xdist_groups: vec![],
                    errors: vec![],
                    deselected: 0,
                });
                self.handle_crash(&mut proc, vec![]);
                return;
            }
            Some((nodeids, xdist_groups, errors, deselected)) => {
                let _ = self.sender.send(Event::Collection {
                    worker: self.index,
                    nodeids,
                    xdist_groups,
                    errors,
                    deselected,
                });
            }
        }

        // Work loop: block until the merge loop pushes batches (after all
        // workers have reported Collection), then process them.
        let mut graceful_shutdown = false;

        'work: loop {
            let Some(batch) = self.queue.next_blocking(self.index) else {
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
                    Some(WorkerMsg::Interrupted { code, banner }) => {
                        let _ = self.sender.send(Event::Interrupted { code, banner });
                        // Block until the merge loop calls queue.stop() so that
                        // the next next_blocking() call sees soft_stopped == true
                        // and returns None instead of dispatching another batch.
                        self.queue.wait_soft_stopped();
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
                    Some(WorkerMsg::Bye) => {
                        graceful_shutdown = true;
                        break 'work;
                    }
                    Some(WorkerMsg::Collection { .. }) | None => {
                        // Collection shouldn't arrive during work loop; treat as output.
                        let _ = self.sender.send(Event::Output(line));
                    }
                }
            }
        }

        // Drain post-shutdown frames: final scope-teardown failure reports,
        // warnings, plugin dumps, bye.
        for line in proc.lines.by_ref() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
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
                Some(WorkerMsg::Bye) => {
                    graceful_shutdown = true;
                }
                Some(_) | None => {}
            }
        }
        if graceful_shutdown {
            proc.handle.wait();
        } else {
            // Worker died after the collection phase but before Bye.
            self.handle_crash(&mut proc, vec![]);
        }
    }
}

fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let dest = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}
