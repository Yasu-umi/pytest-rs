//! Worker mode (-n): this process is driven over stdin/stdout. Like
//! upstream xdist it imports every test module up front (collection
//! phase), so test side effects never leak into module import time.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};
use std::path::PathBuf;

use pyo3::prelude::*;

use crate::collect::TestItem;
use crate::engine::Engine;
use crate::fixture::Scope;
use crate::hooks::HookContext;
use crate::ipc::{ParentMsg, WorkerMsg, encode_frame};
use crate::python;
use crate::report::{Outcome, Phase, TestReport, exit_code};

/// Extract the xdist_group mark value for an item (mirrors Engine::xdist_group_of in dist.rs).
fn xdist_group_of(py: Python<'_>, item: &TestItem) -> Option<String> {
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

fn send(msg: &WorkerMsg) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(encode_frame(msg).as_bytes());
    let _ = stdout.flush();
}

/// A setup-failed + teardown pair, so the parent sees the test as completed
/// (its crash bookkeeping keys on teardown reports).
fn send_collect_error(nodeid: &str, message: String) {
    send(&WorkerMsg::Report {
        report: TestReport {
            nodeid: nodeid.to_string(),
            phase: Phase::Setup,
            outcome: Outcome::Failed,
            duration: std::time::Duration::ZERO,
            longrepr: Some(message),
            location: None,
            subtest_desc: None,
            sections: Vec::new(),
            rerun: false,
            xfail_longrepr: None,
            reprcrash_message: None,
            head_line: None,
        },
    });
    send(&WorkerMsg::Report {
        report: TestReport {
            nodeid: nodeid.to_string(),
            phase: Phase::Teardown,
            outcome: Outcome::Passed,
            duration: std::time::Duration::ZERO,
            longrepr: None,
            location: None,
            subtest_desc: None,
            sections: Vec::new(),
            rerun: false,
            xfail_longrepr: None,
            reprcrash_message: None,
            head_line: None,
        },
    });
}

/// Per-worker lazy collection state.
#[derive(Default)]
struct WorkerCollection {
    /// All items collected so far, in collection order.
    items: Vec<TestItem>,
    by_nodeid: HashMap<String, usize>,
    collected_files: HashSet<PathBuf>,
    loaded_conftests: HashSet<PathBuf>,
    /// How many session py_hooks have had pytest_configure fired.
    configured_hooks: usize,
    prev_class: Option<String>,
    /// The last completed item: deferred scope-teardown failures report
    /// under it (crate::runner::teardown_scope_reported).
    last_nodeid: Option<String>,
}

impl Engine {
    /// The spawned-worker entry (--worker): a fresh process, so register
    /// fixtures and import every test module before running anything.
    pub(crate) fn run_worker(&mut self, py: Python<'_>) -> i32 {
        // Test prints (-s) flow over fd 1 as passthrough lines — the parent
        // forwards non-frame lines to its stdout, like upstream's relay.
        // Line buffering keeps them ordered against protocol frames.
        if py
            .run(
                c"import sys\ntry:\n    sys.stdout.reconfigure(line_buffering=True)\nexcept Exception:\n    pass\n",
                None,
                None,
            )
            .is_err()
        {
            return exit_code::INTERNAL_ERROR;
        }
        if let Err(err) =
            python::register_builtin_fixtures(py, &self.config, &mut self.session.registry)
        {
            eprintln!(
                "INTERNAL ERROR: worker fixture registration failed: {}",
                python::format_exception(py, &err)
            );
            return exit_code::INTERNAL_ERROR;
        }
        // Mirror the controller's plugin loading (-p NAME, then pytest11
        // entry points); forked workers inherit these instead.
        let mut loaded_modules: Vec<String> = Vec::new();
        let blocked: Vec<String> = self
            .config
            .plugin_opts
            .iter()
            .filter_map(|spec| spec.strip_prefix("no:"))
            .map(str::to_string)
            .collect();
        let named: Vec<String> = self
            .config
            .plugin_opts
            .iter()
            .filter(|spec| !spec.starts_with("no:"))
            .cloned()
            .collect();
        if let Err(err) = python::load_named_plugins(
            py,
            &named,
            Some(&self.config.invocation_dir),
            &mut self.session.registry,
            &mut self.session.py_hooks,
            &mut loaded_modules,
            &blocked,
            true,
        ) {
            eprintln!(
                "INTERNAL ERROR: worker plugin loading failed: {}",
                python::format_exception(py, &err)
            );
            return exit_code::INTERNAL_ERROR;
        }
        if !self.config.get_flag("disable-plugin-autoload")
            && let Err(err) = python::load_entrypoint_plugins(
                py,
                &blocked,
                &mut self.session.registry,
                &mut self.session.py_hooks,
                &mut self.session.plugin_distinfo,
                &mut loaded_modules,
            )
        {
            eprintln!(
                "INTERNAL ERROR: worker entry-point plugin loading failed: {}",
                python::format_exception(py, &err)
            );
            return exit_code::INTERNAL_ERROR;
        }

        // `-p`/entry-point plugins have now imported, same as collect()'s
        // load_cmdline_and_entrypoint_plugins + fire_plugins_registered — a
        // worker never calls collect() (it has this whole separate startup
        // path instead), so without this a plugin that arms itself here
        // (e.g. pytest-cov's coverage tracing) never starts in worker
        // processes at all.
        if let Err(err) = self.fire_plugins_registered(py) {
            eprintln!(
                "INTERNAL ERROR: worker plugins-registered hook failed: {}",
                python::format_exception(py, &err)
            );
            return exit_code::INTERNAL_ERROR;
        }

        // Mirror the controller's configure pipeline (collect pipeline
        // phases 4-5) now that all plugins (entry points + -p) are loaded.
        // A spawned worker is a fresh process, so unlike a forked worker it
        // does not inherit the controller's already-fired hooks: plugins
        // register options (pytest_addoption), then read them back in
        // pytest_load_initial_conftests (e.g. pytest-django registers --ds/
        // --itv and reads options.itv there, and stores the per-process db
        // blocker stash key the worker's test setup later reads). Skipping
        // these leaves plugin-defined options unregistered and per-process
        // setup undone. (Conftest pytest_configure hooks fire incrementally
        // during precollect below.)
        if let Err(err) = self.fire_py_addoption_hooks(py) {
            eprintln!(
                "INTERNAL ERROR: worker addoption failed: {}",
                python::format_exception(py, &err)
            );
            return exit_code::INTERNAL_ERROR;
        }
        if let Err(err) = self.apply_plugin_cli_args(py, true) {
            if python::is_usage_error(py, &err) {
                eprintln!("ERROR: {}", err.value(py));
                return exit_code::USAGE_ERROR;
            }
            eprintln!("{}", python::format_exception(py, &err));
            return exit_code::USAGE_ERROR;
        }
        if let Err(err) = self.fire_py_load_initial_conftests(py) {
            if python::is_usage_error(py, &err) {
                let msg = python::format_exception(py, &err);
                let usage_msg = msg
                    .lines()
                    .last()
                    .and_then(|l| l.strip_prefix("pytest.UsageError: "))
                    .unwrap_or(msg.trim());
                eprintln!("ERROR: {usage_msg}");
                return exit_code::USAGE_ERROR;
            }
            eprintln!("{}", python::format_exception(py, &err));
            return exit_code::USAGE_ERROR;
        }

        let mut collection = WorkerCollection::default();
        // pytest_configure must fire unconditionally (upstream guarantee),
        // but this worker only otherwise fires it incrementally inside
        // ensure_collected as conftests are discovered per file — if this
        // worker's file set never triggers that (e.g. only a conftest.py,
        // no test_*.py), an entry-point plugin's pytest_configure (which
        // may conditionally register more hooks, e.g. pytest-mypy's
        // pytest_collect_file collector) would otherwise never run at all.
        // Firing it now, before any file is collected, also sets the
        // configured_hooks cursor so this doesn't double-fire later.
        if let Err(err) = self.fire_new_conftest_configure(py, &mut collection.configured_hooks) {
            eprintln!(
                "INTERNAL ERROR: worker configure failed: {}",
                python::format_exception(py, &err)
            );
            return exit_code::INTERNAL_ERROR;
        }
        // Like upstream xdist, import every test module during collection,
        // before any test runs: lazy per-batch imports would let earlier
        // tests' side effects (warning filters, random seeds, monkey
        // patches) leak into later modules' import time.
        let collect_errors = self.precollect_all(py, &mut collection);
        // Mirror the controller's post-collection selection phase
        // (session.rs: apply_deselect -> fire_collection_modifyitems ->
        // fire_py_deselected). Like real pytest-xdist, each worker collects
        // and selects independently — without this, -k/-m/--deselect and
        // conftest/plugin pytest_collection_modifyitems (e.g. anyio/asyncio's
        // backend-parametrization expansion, pytest-split's group slicing,
        // pytest-order's reordering) are silently ignored under -n, since
        // this whole file never calls collect()'s equivalent pipeline.
        let deselected = match self.apply_worker_selection(py, &mut collection) {
            Ok(deselected) => deselected,
            Err(err) => {
                eprintln!(
                    "INTERNAL ERROR: worker collection modifyitems failed: {}",
                    python::format_exception(py, &err)
                );
                return exit_code::INTERNAL_ERROR;
            }
        };
        // Report the collected item set (and any errors) to the controller
        // so it can build work batches without importing test files itself.
        let (nodeids, xdist_groups): (Vec<_>, Vec<_>) = collection
            .items
            .iter()
            .map(|item| (item.nodeid.clone(), xdist_group_of(py, item)))
            .unzip();
        send(&WorkerMsg::Collection {
            nodeids,
            xdist_groups,
            errors: collect_errors,
            deselected,
        });
        self.worker_loop(py, collection)
    }

    /// The forked-worker entry (-n on unix): the parent's interpreter
    /// state — imported test modules, conftests, fixture registry, fired
    /// configure hooks — arrived via fork, so collection reduces to
    /// re-indexing the parent's collected items.
    #[cfg(unix)]
    #[allow(dead_code)]
    pub(crate) fn run_worker_forked(&mut self, py: Python<'_>) -> i32 {
        // The inherited config was parsed by the controller (no --worker
        // flag); plugins must still see this process as a worker (e.g. cov
        // ships its hits via pytest_worker_dump instead of reporting).
        self.config.mark_worker();
        // A reporter replacement detected pre-fork belongs to the
        // controller; workers must never drive it (stdout is the IPC pipe).
        self.session.custom_reporter = None;
        if py
            .run(
                c"import sys\ntry:\n    sys.stdout.reconfigure(line_buffering=True)\nexcept Exception:\n    pass\n",
                None,
                None,
            )
            .is_err()
        {
            return exit_code::INTERNAL_ERROR;
        }
        // The inherited global capture saved the controller's terminal fds;
        // rebuild it against this worker's own fds (fd 1 is the IPC pipe).
        python::capture_reinit_post_fork(py);
        // Fork duplicates the parent's PRNG state into every worker;
        // reseed so workers diverge like freshly spawned processes do.
        let _ = py.run(
            c"import sys, random\nrandom.seed()\n_np = sys.modules.get('numpy')\nif _np is not None:\n    _np.random.seed()\n",
            None,
            None,
        );
        // Collection-time warnings are the parent's to report; drop the
        // inherited copies so they are not double-counted.
        let _ = py.run(
            c"import pytest._wcapture as _w\n_w.captured.clear()\n",
            None,
            None,
        );

        let mut collection = WorkerCollection {
            configured_hooks: self.session.py_hooks.len(),
            ..WorkerCollection::default()
        };
        for item in std::mem::take(&mut self.session.items) {
            let file = item.nodeid.split("::").next().unwrap_or("");
            collection
                .collected_files
                .insert(self.config.rootdir.join(file));
            collection
                .by_nodeid
                .insert(item.nodeid.clone(), collection.items.len());
            collection.items.push(item);
        }
        self.worker_loop(py, collection)
    }

    /// The worker main loop; the exit code is informational (the parent
    /// judges success from the streamed reports).
    fn worker_loop(&mut self, py: Python<'_>, mut collection: WorkerCollection) -> i32 {
        let mut prev_module: Option<String> = None;
        let maxfail = self.config.maxfail();
        let mut total_failed = 0usize;
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            let Ok(msg) = serde_json::from_str::<ParentMsg>(&line) else {
                continue;
            };
            match msg {
                ParentMsg::Run { nodeids } => {
                    total_failed += self.run_batch(
                        py,
                        &mut collection,
                        &mut prev_module,
                        &nodeids,
                        maxfail,
                        total_failed,
                    );
                    // xdist's [setproctitle] extra: idle between batches.
                    python::worker_set_title(py, "[pytest-xdist idle]");
                    // Propagate KeyboardInterrupt / pytest.exit so the controller
                    // can set the right exit code and stop dispatching new work.
                    if let Some(code) = self.session.exit_code_override {
                        send(&WorkerMsg::Interrupted {
                            code,
                            banner: self.session.abort_banner.clone(),
                        });
                        send(&WorkerMsg::Done);
                        break;
                    }
                    // Propagate --maxfail so the controller stops dispatching
                    // the next batch before this worker pulls it from the queue.
                    if let Some(m) = maxfail
                        && total_failed >= m
                    {
                        send(&WorkerMsg::Interrupted {
                            code: exit_code::TESTS_FAILED,
                            banner: None,
                        });
                        send(&WorkerMsg::Done);
                        break;
                    }
                    send(&WorkerMsg::Done);
                }
                ParentMsg::Shutdown => break,
            }
        }

        // Final scope teardowns mirror run_items; failures stream to the
        // parent as teardown ERROR reports.
        if let Some(last) = collection.items.last() {
            if let Some(prev) = &collection.prev_class
                && let Some(report) = crate::runner::teardown_scope_reported(
                    py,
                    &self.plugins,
                    &mut self.session,
                    &self.config,
                    Scope::Class,
                    prev,
                    last,
                    collection.last_nodeid.as_deref(),
                )
            {
                send(&WorkerMsg::Report { report });
            }
            if let Some(prev) = &prev_module
                && let Some(report) = crate::runner::teardown_scope_reported(
                    py,
                    &self.plugins,
                    &mut self.session,
                    &self.config,
                    Scope::Module,
                    prev,
                    last,
                    collection.last_nodeid.as_deref(),
                )
            {
                send(&WorkerMsg::Report { report });
            }
            if let Some(report) = crate::runner::teardown_scope_reported(
                py,
                &self.plugins,
                &mut self.session,
                &self.config,
                Scope::Session,
                "",
                last,
                collection.last_nodeid.as_deref(),
            ) {
                send(&WorkerMsg::Report { report });
            }
        }

        let actual_exit = self.session.exit_code_override.unwrap_or(exit_code::OK);
        // Collect py_hooks before the mutable borrow for native plugins.
        let py_sessionfinish: Vec<Py<PyAny>> = self
            .session
            .py_hooks
            .iter()
            .filter(|h| h.name == "pytest_sessionfinish")
            .map(|h| h.func.clone_ref(py))
            .collect();
        let mut ctx = HookContext {
            py,
            session: &mut self.session,
            config: &self.config,
        };
        for plugin in self.plugins.iter_mut() {
            if let Err(err) = plugin.pytest_sessionfinish(&mut ctx, actual_exit) {
                eprintln!(
                    "INTERNAL ERROR: worker sessionfinish: {}",
                    python::format_exception(py, &err)
                );
            }
        }
        // Fire conftest/plugin pytest_sessionfinish py_hooks so worker-side
        // pytest_sessionfinish can populate config.workeroutput for the controller.
        if !py_sessionfinish.is_empty()
            && let (Ok(config_proxy), Ok(session_proxy), Ok(exitstatus)) = (
                python::make_py_config(py, &self.config),
                python::make_session_proxy(py, &self.config),
                actual_exit.into_pyobject(py).map(|o| o.unbind().into_any()),
            )
        {
            for func in &py_sessionfinish {
                let _ = python::call_py_hook(
                    py,
                    func,
                    &[
                        ("config", config_proxy.clone_ref(py)),
                        ("session", session_proxy.clone_ref(py)),
                        ("exitstatus", exitstatus.clone_ref(py)),
                    ],
                );
            }
        }
        for plugin in self.plugins.iter_mut() {
            match plugin.pytest_worker_dump(&mut ctx) {
                Ok(Some(payload)) => send(&WorkerMsg::Extra {
                    plugin: plugin.name().to_string(),
                    payload,
                }),
                Ok(None) => {}
                Err(err) => eprintln!(
                    "INTERNAL ERROR: worker dump: {}",
                    python::format_exception(py, &err)
                ),
            }
        }
        let warning_count = python::warning_count(py);
        if warning_count > 0 {
            send(&WorkerMsg::Warnings {
                lines: python::warning_summary_lines(py, 0),
                count: warning_count,
            });
        }
        // config.workeroutput travels back for pytest_testnodedown.
        if let Some(payload) = python::worker_output_json(py) {
            send(&WorkerMsg::Workeroutput { payload });
        }
        send(&WorkerMsg::Bye);
        exit_code::OK
    }

    fn run_batch(
        &mut self,
        py: Python<'_>,
        collection: &mut WorkerCollection,
        prev_module: &mut Option<String>,
        nodeids: &[String],
        maxfail: Option<usize>,
        failed_before_batch: usize,
    ) -> usize {
        let mut batch_failed = 0usize;
        for nodeid in nodeids {
            if let Err(message) = self.ensure_collected(py, collection, nodeid) {
                send_collect_error(nodeid, message);
                continue;
            }
            let Some(&index) = collection.by_nodeid.get(nodeid) else {
                send_collect_error(nodeid, format!("worker could not collect {nodeid}"));
                continue;
            };

            let item = &collection.items[index];
            let last_nodeid = collection.last_nodeid.clone();
            let class_instance = item.class_instance();
            if let Some(prev) = &collection.prev_class
                && prev != &class_instance
                && let Some(report) = crate::runner::teardown_scope_reported(
                    py,
                    &self.plugins,
                    &mut self.session,
                    &self.config,
                    Scope::Class,
                    prev,
                    item,
                    last_nodeid.as_deref(),
                )
            {
                send(&WorkerMsg::Report { report });
            }
            collection.prev_class = Some(class_instance);

            let item = &collection.items[index];
            let module_instance = item.module_instance();
            if let Some(prev) = prev_module
                && prev != &module_instance
                && let Some(report) = crate::runner::teardown_scope_reported(
                    py,
                    &self.plugins,
                    &mut self.session,
                    &self.config,
                    Scope::Module,
                    prev,
                    item,
                    last_nodeid.as_deref(),
                )
            {
                send(&WorkerMsg::Report { report });
            }
            *prev_module = Some(module_instance);

            let lineno = item.lineno;
            // xdist's [setproctitle] extra: the running item shows in ps.
            python::worker_set_title(py, &format!("[pytest-xdist running] {}", item.nodeid));
            // Track how many reports were streamed before teardown so we don't
            // double-send them (pre_teardown streams setup+call immediately so
            // a teardown crash doesn't swallow the call outcome).
            let pre_teardown_count = std::cell::Cell::new(0usize);
            let reports = crate::runner::run_one(
                py,
                &self.plugins,
                &mut self.session,
                &self.config,
                item,
                None,
                Some(&|pre: &[crate::report::TestReport]| {
                    pre_teardown_count.set(pre.len());
                    for r in pre {
                        send(&WorkerMsg::Report { report: r.clone() });
                    }
                }),
                |py, session, _config, item| {
                    // Workers don't print a nodeid line or set the live-log
                    // "start" label (no terminal of their own) — only fire
                    // pytest_runtest_logstart, on the native protocol path.
                    let _ = crate::runner::fire_runtest_py_hooks(
                        py,
                        session,
                        item,
                        "pytest_runtest_logstart",
                    );
                },
            );
            collection.last_nodeid = Some(item.nodeid.clone());
            let pre_count = pre_teardown_count.get();
            batch_failed += reports
                .iter()
                .filter(|r| r.outcome == crate::report::Outcome::Failed)
                .count();
            for (i, report) in reports.into_iter().enumerate() {
                crate::runner::fire_logreport_hooks(
                    py,
                    &self.session,
                    &report,
                    Some(lineno),
                    Some(item),
                    false,
                );
                if i >= pre_count {
                    send(&WorkerMsg::Report { report });
                }
            }
            let item = &collection.items[index];
            let _ = crate::runner::fire_runtest_py_hooks(
                py,
                &self.session,
                item,
                "pytest_runtest_logfinish",
            );
            // --maxfail/-x: a chunked batch can hold several items (xdist's
            // own load scheduler does the same once there are enough tests
            // per worker); upstream re-derives its `session.shouldfail`
            // per item inside the worker's own runtestloop, so later items
            // already dispatched in this batch still don't run once the
            // threshold is crossed mid-batch.
            if let Some(m) = maxfail
                && failed_before_batch + batch_failed >= m
            {
                break;
            }
        }
        batch_failed
    }

    /// Import every test module the session can reach, mirroring the
    /// controller's discovery. Returns collection errors as (nodeid, message) pairs.
    fn precollect_all(
        &mut self,
        py: Python<'_>,
        collection: &mut WorkerCollection,
    ) -> Vec<(String, String)> {
        let mut errors = Vec::new();
        // Mirror the controller's start paths (CLI args, else testpaths ini).
        let mut paths = self.config.paths.clone();
        if paths.is_empty()
            && let Some(testpaths) = self.config.get_ini("testpaths")
        {
            let entries: Vec<String> = testpaths.split_whitespace().map(str::to_string).collect();
            if !entries.is_empty()
                && let Ok(globbed) = python::glob_testpaths(py, &self.config.rootdir, &entries)
                && !globbed.is_empty()
            {
                paths = globbed;
            }
        }
        self.session.initial_paths =
            crate::collect::resolve_initial_paths(&self.config.invocation_dir, &paths);
        let python_files = self.config.python_files_patterns();
        let norecursedirs = self.config.norecursedirs_patterns();
        let Ok((files, _not_found)) = crate::collect::collect_test_files(
            &self.config.invocation_dir,
            &paths,
            self.config.get_flag("collect-in-virtualenv"),
            &python_files,
            &norecursedirs,
            self.config.get_flag("keep-duplicates"),
            &crate::collect::CollectIgnores::from_config(&self.config),
        ) else {
            return errors;
        };
        for file in files {
            let rel = crate::collect::file_nodeid(&self.config.rootdir, &file, &[]);
            if let Err(msg) = self.ensure_collected(py, collection, &rel) {
                errors.push((rel, msg));
            }
        }
        // Custom collectors (pytest-mypy / pytest-ruff): unlike the
        // controller's collect() -> collect_extra_and_custom, this worker
        // startup path only ever globbed python_files-pattern test files
        // above, so pytest_collect_file hooks never ran here at all and
        // every custom-collected item (including a bare conftest.py, which
        // isn't itself a "test file") silently vanished under -n. Mirror
        // collect_extra_and_custom's logic (reorder.rs) against the
        // broader any-extension candidate set.
        if python::has_collect_file_hook(py, &self.session.py_hooks) {
            let candidate = crate::collect::collect_all_files(
                &self.config.invocation_dir,
                &paths,
                self.config.get_flag("collect-in-virtualenv"),
            );
            // Unlike a standard test file (which ensure_collected always
            // loads the conftest chain for first), a file only reached
            // through pytest_collect_file below never gets its own conftest
            // ancestry imported first — so a conftest.py that is itself a
            // custom-collector's check target (e.g. pytest-mypy checking a
            // bare conftest.py) would see its own not-yet-executed
            // pytest_configure mutations. Load each candidate's chain now.
            for file in &candidate {
                if let Err(msg) = self.ensure_conftest_chain(py, collection, file) {
                    errors.push((
                        crate::collect::file_nodeid(&self.config.rootdir, file, &[]),
                        msg,
                    ));
                }
            }
            let hooks = std::mem::take(&mut self.session.py_hooks);
            let result = python::collect_custom_files(
                py,
                &self.config.rootdir,
                &candidate,
                &hooks,
                &mut collection.items,
            );
            self.session.py_hooks = hooks;
            match result {
                Ok(collect_result) => {
                    if !collect_result.skipped.is_empty() {
                        let skipped_set: std::collections::HashSet<&PathBuf> =
                            collect_result.skipped.iter().map(|(p, _)| p).collect();
                        collection
                            .items
                            .retain(|item| !skipped_set.contains(&item.path));
                    }
                    for (path, longrepr) in collect_result.errors {
                        errors.push((
                            crate::collect::file_nodeid(&self.config.rootdir, &path, &[]),
                            longrepr,
                        ));
                    }
                    for file in collect_result.native_fallback {
                        if collection.collected_files.contains(&file)
                            || file.file_name().and_then(|n| n.to_str()) == Some("conftest.py")
                        {
                            continue;
                        }
                        let rel = crate::collect::file_nodeid(&self.config.rootdir, &file, &[]);
                        if let Err(msg) = self.ensure_collected(py, collection, &rel) {
                            errors.push((rel, msg));
                        }
                    }
                }
                Err(err) => {
                    errors.push((
                        self.config.rootdir.to_string_lossy().into_owned(),
                        python::format_exception(py, &err),
                    ));
                }
            }
            // `collect_custom_files` pushed straight into collection.items
            // (bypassing ensure_collected's own by_nodeid bookkeeping), and
            // the skip-retain above may have shifted indices — rebuild once
            // rather than tracking index ranges through both mutations.
            collection.by_nodeid.clear();
            for (index, item) in collection.items.iter().enumerate() {
                collection.by_nodeid.insert(item.nodeid.clone(), index);
            }
            // A custom-collected item's file was never marked "collected"
            // (only the native-scan and native_fallback paths do that) —
            // run_batch's ensure_collected() sees any dispatched nodeid it
            // doesn't recognize as unfinished collection and tries to
            // *import* the file as a real Python module before running it.
            // For e.g. a MypyFile whose whole point is checking a .pyi file
            // without ever importing it, that spuriously collides with the
            // sibling .py module under the same import name ("import file
            // mismatch"). Mark every surviving item's file as collected so
            // that safety net is a no-op for files that only ever went
            // through the custom-collector path.
            for item in &collection.items {
                collection.collected_files.insert(item.path.clone());
            }
        }
        errors
    }

    /// Mirror the controller's post-collection selection phase against this
    /// worker's own collected items: `--deselect`, then conftest/plugin
    /// `pytest_collection_modifyitems` (which may reorder, add marks, skip,
    /// or expand items — e.g. anyio/asyncio's backend parametrization) and
    /// `-k`/`-m`, then `pytest_deselected` hooks. Reuses the existing Engine
    /// methods verbatim by lending `collection.items` to `self.session.items`
    /// for the duration (those methods all operate through the session's
    /// item list) rather than reimplementing any of this. Returns the
    /// deselected count for the controller's summary line.
    ///
    /// Deliberately NOT handled here: `--strict-markers` validation (a
    /// sibling gap: it also only runs against the controller's always-empty
    /// `self.session.items` in dist mode). Surfacing a worker-side usage
    /// error cleanly needs its own IPC error channel; out of scope here.
    fn apply_worker_selection(
        &mut self,
        py: Python<'_>,
        collection: &mut WorkerCollection,
    ) -> PyResult<usize> {
        let collected = collection.items.len();
        self.session.items = std::mem::take(&mut collection.items);
        let result = (|| -> PyResult<()> {
            self.apply_deselect()
                .map_err(pyo3::exceptions::PyRuntimeError::new_err)?;
            self.fire_collection_modifyitems(py)?;
            self.fire_py_deselected(py)?;
            Ok(())
        })();
        collection.items = std::mem::take(&mut self.session.items);
        result?;
        // Items may have been removed, reordered, or expanded (anyio/asyncio
        // parametrize over backends) — by_nodeid must reflect the final set,
        // same as the rebuild after collect_custom_files above.
        collection.by_nodeid.clear();
        for (index, item) in collection.items.iter().enumerate() {
            collection.by_nodeid.insert(item.nodeid.clone(), index);
        }
        Ok(collected.saturating_sub(collection.items.len()))
    }

    /// Import (once) the conftest.py chain from rootdir down to `path`'s
    /// directory (rootdir-most first), firing `pytest_configure` for any
    /// newly-loaded one. Shared by `ensure_collected` (a file reached via
    /// the standard native scan) and `precollect_all`'s custom-collector
    /// pass: a file only ever reached through `pytest_collect_file` (e.g. a
    /// bare conftest.py that a plugin like pytest-mypy also treats as a
    /// check target) otherwise never has its own conftest ancestry loaded
    /// first, so a conftest's `pytest_configure` mutations (e.g. a plugin's
    /// module-level config) aren't visible yet when that plugin's collector
    /// runs against the very same file.
    fn ensure_conftest_chain(
        &mut self,
        py: Python<'_>,
        collection: &mut WorkerCollection,
        path: &std::path::Path,
    ) -> Result<(), String> {
        let mut chain = Vec::new();
        let mut dir = path.parent();
        while let Some(d) = dir {
            let conftest = d.join("conftest.py");
            if conftest.exists() && !collection.loaded_conftests.contains(&conftest) {
                chain.push(conftest);
            }
            if d == self.config.rootdir {
                break;
            }
            dir = d.parent();
        }
        chain.reverse();
        let import_mode = crate::collect::ImportMode::from_config(&self.config);
        for conftest in chain {
            python::collect_conftest(
                py,
                &self.config.rootdir,
                &conftest,
                &mut self.session.registry,
                &mut self.session.py_hooks,
                import_mode,
                &self.session.initial_paths,
            )
            .map_err(|err| python::format_exception(py, &err))?;
            collection.loaded_conftests.insert(conftest);
        }
        self.fire_new_conftest_configure(py, &mut collection.configured_hooks)
            .map_err(|err| python::format_exception(py, &err))
    }

    /// Import (once) everything needed to resolve a node ID: the conftest
    /// chain, then the test module, then fixture-param expansion.
    fn ensure_collected(
        &mut self,
        py: Python<'_>,
        collection: &mut WorkerCollection,
        nodeid: &str,
    ) -> Result<(), String> {
        let file_part = nodeid.split("::").next().unwrap_or(nodeid);
        let path = self.config.rootdir.join(file_part);
        if collection.collected_files.contains(&path) {
            return Ok(());
        }

        self.ensure_conftest_chain(py, collection, &path)?;
        let import_mode = crate::collect::ImportMode::from_config(&self.config);

        let mut new_items = Vec::new();
        python::collect_module(
            py,
            &self.config.rootdir,
            &path,
            &mut new_items,
            &mut self.session.registry,
            &mut self.session.py_hooks,
            &python::NameFilters::from_config(py, &self.config),
            import_mode,
            &self.plugins,
            &self.session.initial_paths,
            self.config.collect_imported_tests(),
        )
        .map_err(|err| python::format_test_failure(py, &err, "short"))?;
        {
            let mut ctx = HookContext {
                py,
                session: &mut self.session,
                config: &self.config,
            };
            for plugin in &self.plugins {
                plugin
                    .pytest_collection_preexpand(&mut ctx, &mut new_items)
                    .map_err(|err| python::format_exception(py, &err))?;
            }
        }
        let new_items = python::expand_fixture_params(py, new_items, &self.session.registry)
            .map_err(|err| python::format_exception(py, &err))?;

        for item in new_items {
            collection
                .by_nodeid
                .insert(item.nodeid.clone(), collection.items.len());
            collection.items.push(item);
        }
        collection.collected_files.insert(path);
        Ok(())
    }

    /// Fire pytest_configure for conftest hooks loaded since the last call.
    fn fire_new_conftest_configure(
        &mut self,
        py: Python<'_>,
        configured: &mut usize,
    ) -> PyResult<()> {
        let new_hooks: Vec<Py<PyAny>> = self.session.py_hooks[*configured..]
            .iter()
            .filter(|hook| hook.name == "pytest_configure")
            .map(|hook| hook.func.clone_ref(py))
            .collect();
        *configured = self.session.py_hooks.len();
        if new_hooks.is_empty() {
            return Ok(());
        }
        let config_proxy = python::make_py_config(py, &self.config)?;
        for func in &new_hooks {
            python::call_py_hook(py, func, &[("config", config_proxy.clone_ref(py))])?;
        }
        Ok(())
    }
}
