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
        if let Err(err) = python::register_builtin_fixtures(py, &mut self.session.registry) {
            eprintln!(
                "INTERNAL ERROR: worker fixture registration failed: {}",
                python::format_exception(py, &err)
            );
            return exit_code::INTERNAL_ERROR;
        }
        // Mirror the controller's plugin loading (-p NAME, then pytest11
        // entry points); forked workers inherit these instead.
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
        ) {
            eprintln!(
                "INTERNAL ERROR: worker plugin loading failed: {}",
                python::format_exception(py, &err)
            );
            return exit_code::INTERNAL_ERROR;
        }
        let blocked: Vec<String> = self
            .config
            .plugin_opts
            .iter()
            .filter_map(|spec| spec.strip_prefix("no:"))
            .map(str::to_string)
            .collect();
        if let Err(err) = python::load_entrypoint_plugins(
            py,
            &blocked,
            &mut self.session.registry,
            &mut self.session.py_hooks,
            &mut self.session.plugin_distinfo,
        ) {
            eprintln!(
                "INTERNAL ERROR: worker entry-point plugin loading failed: {}",
                python::format_exception(py, &err)
            );
            return exit_code::INTERNAL_ERROR;
        }

        let mut collection = WorkerCollection::default();
        // Like upstream xdist, import every test module during collection,
        // before any test runs: lazy per-batch imports would let earlier
        // tests' side effects (warning filters, random seeds, monkey
        // patches) leak into later modules' import time.
        self.precollect_all(py, &mut collection);
        self.worker_loop(py, collection)
    }

    /// The forked-worker entry (-n on unix): the parent's interpreter
    /// state — imported test modules, conftests, fixture registry, fired
    /// configure hooks — arrived via fork, so collection reduces to
    /// re-indexing the parent's collected items.
    #[cfg(unix)]
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
                    self.run_batch(py, &mut collection, &mut prev_module, &nodeids);
                    // xdist's [setproctitle] extra: idle between batches.
                    python::worker_set_title(py, "[pytest-xdist idle]");
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

        let mut ctx = HookContext {
            py,
            session: &mut self.session,
            config: &self.config,
        };
        for plugin in self.plugins.iter_mut() {
            if let Err(err) = plugin.pytest_sessionfinish(&mut ctx, exit_code::OK) {
                eprintln!(
                    "INTERNAL ERROR: worker sessionfinish: {}",
                    python::format_exception(py, &err)
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
    ) {
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
            let _ = crate::runner::fire_runtest_py_hooks(
                py,
                &self.session,
                item,
                "pytest_runtest_logstart",
            );
            let reports = crate::runner::run_one(
                py,
                &self.plugins,
                &mut self.session,
                &self.config,
                item,
                None,
            );
            collection.last_nodeid = Some(item.nodeid.clone());
            for report in reports {
                crate::runner::fire_logreport_hooks(py, &self.session, &report, Some(lineno), Some(item));
                send(&WorkerMsg::Report { report });
            }
            let item = &collection.items[index];
            let _ = crate::runner::fire_runtest_py_hooks(
                py,
                &self.session,
                item,
                "pytest_runtest_logfinish",
            );
        }
    }

    /// Import every test module the session can reach, mirroring the
    /// controller's discovery. Files that fail to import are skipped here;
    /// the error reports properly when a batch references them.
    fn precollect_all(&mut self, py: Python<'_>, collection: &mut WorkerCollection) {
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
        let python_files = self.config.python_files_patterns();
        let norecursedirs = self.config.norecursedirs_patterns();
        let Ok(files) = crate::collect::collect_test_files(
            &self.config.invocation_dir,
            &paths,
            self.config.get_flag("collect-in-virtualenv"),
            &python_files,
            &norecursedirs,
            self.config.get_flag("keep-duplicates"),
            &crate::collect::CollectIgnores::from_config(&self.config),
        ) else {
            return;
        };
        for file in files {
            let rel = crate::collect::file_nodeid(&self.config.rootdir, &file);
            let _ = self.ensure_collected(py, collection, &rel);
        }
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

        // Conftest chain (rootdir-most first), each loaded once per worker.
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
        for conftest in chain {
            python::collect_conftest(
                py,
                &self.config.rootdir,
                &conftest,
                &mut self.session.registry,
                &mut self.session.py_hooks,
            )
            .map_err(|err| python::format_exception(py, &err))?;
            collection.loaded_conftests.insert(conftest);
        }
        self.fire_new_conftest_configure(py, &mut collection.configured_hooks)
            .map_err(|err| python::format_exception(py, &err))?;

        let mut new_items = Vec::new();
        python::collect_module(
            py,
            &self.config.rootdir,
            &path,
            &mut new_items,
            &mut self.session.registry,
            &mut self.session.py_hooks,
        )
        .map_err(|err| python::format_exception(py, &err))?;
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
