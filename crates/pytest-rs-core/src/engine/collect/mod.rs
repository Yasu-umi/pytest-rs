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
        let (paths, mut files) = self.resolve_collection_paths(py, &rootdir)?;
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
                print!("\nNOTE: displaying only minimal help due to UsageError.\n\n");
            }
            return Err(err);
        }
        if self.fire_configure_and_print_header(py, &rootdir, &mut errors)? {
            // --markers (or another short-circuit) handled output; skip collection.
            return Ok(errors);
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
        Ok(errors)
    }
}
