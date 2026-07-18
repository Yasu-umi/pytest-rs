//! Test collection: `Engine::collect` and its per-phase helpers.
//!
//! Collection runs as a sequence of phases — resolve paths, load plugins and
//! conftests, configure, filter, then collect modules/doctests/custom files and
//! finalize the item list. The orchestrator (`collect`) wires the phases; each
//! phase lives in its own helper so the control flow reads top-down.

use std::path::PathBuf;

use pyo3::prelude::*;

mod collection;
mod pipeline;
mod plugins;
mod reorder;

use super::Engine;
use crate::python;

impl Engine {
    pub(crate) fn collect(&mut self, py: Python<'_>) -> Result<Vec<(PathBuf, String)>, String> {
        let rootdir = self.config.rootdir.clone();
        let (paths, mut files, deferred_not_found_args) =
            self.resolve_collection_paths(py, &rootdir)?;
        self.session.initial_paths =
            crate::collect::resolve_initial_paths(&self.config.invocation_dir, &paths);
        self.load_cmdline_and_entrypoint_plugins(py)?;
        // `-p`/entry-point plugins have now imported (upstream loads them
        // during early config parsing, well before pytest_configure); let
        // plugins that need to distinguish "already loaded" from
        // "not yet collected" code act now (e.g. pytest-cov's coverage
        // start point).
        self.fire_plugins_registered(py)
            .map_err(|err| python::format_exception(py, &err))?;

        let mut errors = Vec::new();
        // Register entry-point/-p plugins' CLI options, then fire
        // pytest_load_initial_conftests, BEFORE any conftest.py loads.
        // Upstream's own conftest-loading step IS that hookspec's default
        // implementation, so a tryfirst hookimpl of the same hookspec (e.g.
        // pytest-django's, which calls django.setup() after reading
        // --ds/--dc) is guaranteed to run first and see those options
        // already registered. fire_py_addoption_hooks fires again later
        // (load_and_validate_config, once conftest-defined options exist
        // too) — harmless, since it's an idempotent dict overwrite
        // (OptionGroup.addoption in _parser.py), not a side-effecting hook.
        if let Err(err) = self.fire_py_addoption_hooks(py) {
            errors.push((rootdir.to_path_buf(), python::format_exception(py, &err)));
        }
        // Also resolve CLI tokens against those early specs (e.g. pytest's
        // own test_early_config_cmdline: a `-p`/entry-point/PYTEST_PLUGINS
        // plugin's pytest_load_initial_conftests hookimpl reading back a
        // matching `--flag=value` from early_config.known_args_namespace) —
        // but never raise on "unknown" here: conftest-defined options aren't
        // registered yet, so a token this early pass can't resolve may
        // simply not have reached its plugin yet, not be genuinely invalid.
        // The later, full pass (pipeline.rs, after every conftest loads)
        // still does the real "truly unrecognized" check.
        if let Err(err) = self.apply_plugin_cli_args(py, false) {
            errors.push((rootdir.to_path_buf(), python::format_exception(py, &err)));
        }
        if let Err(err) = self.fire_py_load_initial_conftests(py) {
            // Errors here are fatal plugin-init failures (e.g. ImportError from a
            // bad DJANGO_SETTINGS_MODULE). Upstream lets them propagate to stderr
            // as a fatal error; we replicate that by printing to stderr and exiting.
            // UsageError is handled specially (exit code 4).
            if python::is_usage_error(py, &err) {
                let msg = python::format_exception(py, &err);
                let usage_msg = msg
                    .lines()
                    .last()
                    .and_then(|l| l.strip_prefix("pytest.UsageError: "))
                    .unwrap_or(msg.trim());
                eprintln!("ERROR: {usage_msg}");
                return Err("\x00USAGE_ERROR\x00".to_string());
            }
            eprintln!("{}", python::format_exception(py, &err));
            return Err("\x00USAGE_ERROR\x00".to_string());
        }
        let (start_dirs, conftests) = self.discover_conftests(&rootdir, &paths, &files);

        if let Err(err) = self.load_and_validate_config(
            py,
            &rootdir,
            &paths,
            &start_dirs,
            &conftests,
            &mut errors,
        ) {
            // -h/--help combined with a UsageError from conftest/plugin
            // option registration (e.g. a malformed pytest_addoption): upstream's
            // Config.pytest_cmdline_parse still shows help — the full option
            // listing only, no markers/fixtures footer — annotated as
            // "minimal help", then re-raises so the usual UsageError
            // reporting (stderr message, exit code) still happens.
            if let Some(help_text) = &self.config.help_text {
                print!("{help_text}");
                print!("{}", self.config.plugin_option_help);
                print!("\nNOTE: displaying only minimal help due to UsageError.\n\n");
            }
            return Err(err);
        }
        // Now that every conftest.py has loaded (and had its chance to
        // install its own logging handlers, e.g. via logging.basicConfig()),
        // attach pytest-rs's own session-wide log_file/log_cli handlers —
        // matching upstream, which never touches the root logger before
        // pytest_configure/pytest_sessionstart.
        python::logging_arm_session_handlers(py);
        if self.fire_configure_and_print_header(py, &rootdir, &mut errors)? {
            // --markers (or another short-circuit) handled output; skip collection.
            return Ok(errors);
        }
        // Deferred path-existence check: conftest pytest_configure hooks have
        // now fired (issue #143 — ensure configure/unconfigure run even when
        // a CLI arg references a non-existent file).
        if !deferred_not_found_args.is_empty() {
            for arg in &deferred_not_found_args {
                eprintln!("ERROR: file or directory not found: {arg}");
            }
            return Err("\x00USAGE_ERROR\x00".to_string());
        }
        self.apply_collect_ignores(py, &rootdir, &paths, &conftests, &mut files);
        // Warnings issued during test collection are attributed to the "collect" phase.
        let _ = py
            .import("pytest._wcapture")
            .and_then(|m| m.call_method1("set_current_when", ("collect",)));
        // In xdist spawn mode, workers import test modules themselves.
        // Skip collect_module in the controller so os._exit at module level
        // cannot kill the controller process.
        #[cfg(feature = "xdist")]
        let skip_module_import = {
            // "-n0" means 0 workers (sequential) — do not skip imports.
            let using_xdist = self.config.numprocesses_spec().is_some_and(|s| s != "0")
                || self.config.get_flag("dist-load")
                || self.config.get_value("tx").is_some();
            using_xdist
                && !self.config.collect_only
                && !self.config.get_flag("fixtures")
                && !self.config.get_flag("fixtures-per-test")
        };
        #[cfg(not(feature = "xdist"))]
        let skip_module_import = false;

        let collect_result = (|| -> Result<(), String> {
            let deferred_not_found =
                self.collect_files(py, &rootdir, &files, &mut errors, skip_module_import)?;
            self.collect_extra_and_custom(py, &rootdir, &paths, &files, &mut errors)?;
            // Non-Python explicit file args that no custom collector handled → USAGE_ERROR.
            if !deferred_not_found.is_empty() {
                let truly_not_found: Vec<_> = deferred_not_found
                    .into_iter()
                    .filter(|f| !self.session.items.iter().any(|item| &item.path == f))
                    .collect();
                if !truly_not_found.is_empty() {
                    for file in &truly_not_found {
                        eprintln!("ERROR: not found: {}", file.display());
                        eprintln!("(no match in any of [<Session ''>])");
                        eprintln!();
                    }
                    return Err("\x00USAGE_ERROR\x00".to_string());
                }
            }
            if let Err(err) =
                python::validate_dynamic_fixture_scopes(py, &self.config, &self.session.registry)
            {
                let message = python::collect_error_message(py, &err)
                    .unwrap_or_else(|| python::format_exception(py, &err));
                errors.push((rootdir.clone(), message));
            }
            self.finalize_items(py, &rootdir, &paths)?;
            Ok(())
        })();
        // Reset to "config" phase after collection (covers sessionfinish/terminal_summary).
        let _ = py
            .import("pytest._wcapture")
            .and_then(|m| m.call_method1("set_current_when", ("config",)));
        collect_result?;
        // A plugin's pytest_cmdline_main hookimpl (firstresult) may claim the
        // whole run (e.g. pytest-bdd's --generate-missing, which calls
        // session.perform_collect() expecting the already-collected items
        // back, then reads session._fixturemanager). Only pay the cost of
        // building the item-proxy/fixturemanager stash when some plugin
        // actually implements the hook — true for essentially no suite.
        if python::has_cmdline_main_hook(py) {
            let node_proxies: Vec<Py<PyAny>> = self
                .session
                .items
                .iter()
                .map(|item| python::make_node(py, item))
                .collect::<PyResult<_>>()
                .map_err(|err| python::format_exception(py, &err))?;
            let items_list = pyo3::types::PyList::new(py, node_proxies)
                .map_err(|err| python::format_exception(py, &err))?;
            let fixturemanager =
                crate::runner::build_fixturemanager_from_session(py, &self.session)
                    .map_err(|err| python::format_exception(py, &err))?;
            py.import("pytest._node")
                .and_then(|m| m.call_method1("set_native_collection", (items_list, fixturemanager)))
                .map_err(|err| python::format_exception(py, &err))?;
            // Collection just finished, so per-file collect capture already
            // suspended itself (collect_end, called for the last file) —
            // while suspended, `sys.stdout` points at whatever it was
            // *before this capture started*, which for a nested run is the
            // outer session's own capture buffer, not this run's redirected
            // fd. A plugin's pytest_cmdline_main impl (pytest-bdd's
            // show_missing_code) prints via a plain TerminalWriter wrapping
            // `sys.stdout` at call time, so re-resume capturing first (this
            // run's own tmpfile) — run_session's cmdline_main_exit branch
            // pops it to the correct fd via capture_session_end. This hook
            // fires on every run of a suite that merely has such a plugin
            // loaded (not just when the plugin's own flag is set), so if
            // nothing claims the exit, put capture's installed-flag back
            // exactly as found — leaving it forced-on would desync the
            // normal per-item capture bookkeeping for the rest of this run.
            let globals = pyo3::types::PyDict::new(py);
            let _ = py.run(
                c"import pytest._capture as _c
was_installed = _c.state._installed
if _c.state._capture is not None:
    _c.state._capture.resume_capturing()
    _c.state._installed = True
",
                Some(&globals),
                None,
            );
            let was_installed: bool = globals
                .get_item("was_installed")
                .ok()
                .flatten()
                .and_then(|v| v.extract().ok())
                .unwrap_or(false);
            let cmdline_result = python::fire_cmdline_main(py);
            if !matches!(cmdline_result, Ok(Some(_))) && !was_installed {
                let _ = py.run(
                    c"import pytest._capture as _c
if _c.state._capture is not None:
    _c.state._capture.suspend_capturing(in_=True)
    _c.state._installed = False
",
                    None,
                    None,
                );
            }
            match cmdline_result {
                Ok(Some(code)) => self.session.cmdline_main_exit = Some(code),
                Ok(None) => {}
                Err(err) => return Err(python::format_exception(py, &err)),
            }
        }
        Ok(errors)
    }
}
