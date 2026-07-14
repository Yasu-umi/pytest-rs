use std::path::{Path, PathBuf};

use pyo3::prelude::*;

use super::super::Engine;
use super::super::scan_nontoplevel_pytest_plugins;
use crate::python;

/// (cli_paths, test_files, deferred_not_found_args)
type ResolvedPaths = (Vec<String>, Vec<PathBuf>, Vec<String>);

impl Engine {
    pub(crate) fn resolve_collection_paths(
        &self,
        py: Python<'_>,
        rootdir: &Path,
    ) -> Result<ResolvedPaths, String> {
        // No CLI paths: the `testpaths` ini (globbed against rootdir) decides
        // where collection starts; an empty glob warns and falls back to a
        // recursive search from the invocation dir, like pytest.
        let mut paths = self.config.paths.clone();
        let testpaths_lines = self.config.get_ini_lines("testpaths");
        // testpaths only applies when invocation_dir == rootdir (like pytest):
        // if you cd into a subdirectory, pytest ignores testpaths and collects
        // from the current directory instead.
        let invocation_is_root = self.config.invocation_dir == *rootdir;
        if paths.is_empty() && !testpaths_lines.is_empty() && invocation_is_root {
            let entries: Vec<String> = testpaths_lines
                .into_iter()
                .flat_map(|v| v.split_whitespace().map(str::to_string))
                .collect();
            if !entries.is_empty() {
                if self.config.get_flag("pyargs") {
                    // --pyargs: testpaths are module names, not filesystem globs.
                    paths = entries;
                } else {
                    let globbed = python::glob_testpaths(py, rootdir, &entries)
                        .map_err(|err| python::format_exception(py, &err))?;
                    if globbed.is_empty() {
                        let _ = python::warn_explicit_at(
                            py,
                            "PytestConfigWarning",
                            "No files were found in testpaths; consider removing or adjusting \
                             your testpaths configuration. Searching recursively from the \
                             current directory instead.",
                            &rootdir.to_string_lossy(),
                            0,
                        );
                    } else {
                        paths = globbed;
                    }
                }
            }
        }
        // --pyargs: resolve dotted module names to filesystem paths.
        if self.config.get_flag("pyargs") {
            let mut resolved = Vec::with_capacity(paths.len());
            for arg in &paths {
                let path_part = arg.split("::").next().unwrap_or(arg);
                let rest = if arg.contains("::") {
                    &arg[path_part.len()..]
                } else {
                    ""
                };
                if let Some(fs_path) = python::resolve_pyarg(py, path_part) {
                    resolved.push(format!("{}{rest}", fs_path.display()));
                } else if self.config.invocation_dir.join(path_part).exists() {
                    // Not a resolvable dotted module (e.g. a filename that
                    // happens to contain dots, like "t.py"), but it IS a
                    // literal path relative to the invocation dir. Upstream's
                    // resolve_collection_argument() falls through to the
                    // literal path in this case instead of erroring —
                    // search_pypath() failing only means module_name stays
                    // unset, not that the arg itself is invalid.
                    resolved.push(arg.clone());
                } else {
                    return Err(format!(
                        "module or package not found: {arg} (missing __init__.py?)"
                    ));
                }
            }
            paths = resolved;
        }
        // Relative CLI paths (and bare collection) resolve against the
        // invocation dir; rootdir only anchors node ids.
        let python_files = self.config.python_files_patterns();
        let norecursedirs = self.config.norecursedirs_patterns();
        let (files, not_found_args) = crate::collect::collect_test_files(
            &self.config.invocation_dir,
            &paths,
            self.config.get_flag("collect-in-virtualenv"),
            &python_files,
            &norecursedirs,
            self.config.get_flag("keep-duplicates"),
            &crate::collect::CollectIgnores::from_config(&self.config),
        )?;
        Ok((paths, files, not_found_args))
    }

    /// Phase 2: load `-p NAME` / `PYTEST_PLUGINS` cmdline plugins, then
    /// installed pytest11 entry-point plugins — both before conftests.
    pub(crate) fn load_and_validate_config(
        &mut self,
        py: Python<'_>,
        rootdir: &Path,
        paths: &[String],
        start_dirs: &[PathBuf],
        conftests: &[PathBuf],
        errors: &mut Vec<(PathBuf, String)>,
    ) -> Result<(), String> {
        if let Err(err) =
            python::register_builtin_fixtures(py, &self.config, &mut self.session.registry)
        {
            return Err(python::format_exception(py, &err));
        }
        for conftest in conftests {
            // Skip conftests in directories that are ignored by pytest_ignore_collect.
            // Since conftests are ordered root→inner, ancestor hooks are already loaded
            // by the time we process a subdirectory's conftest.
            let conftest_dir = conftest
                .parent()
                .map(std::path::Path::to_path_buf)
                .unwrap_or_else(|| rootdir.to_path_buf());
            if conftest_dir != rootdir {
                let mut ancestor = conftest_dir.clone();
                let mut dir_ignored = false;
                loop {
                    if ancestor == rootdir {
                        break;
                    }
                    if python::call_ignore_collect_hooks(
                        py,
                        &self.session.py_hooks,
                        &ancestor,
                        rootdir,
                    )
                    .is_some()
                    {
                        dir_ignored = true;
                        break;
                    }
                    match ancestor.parent() {
                        Some(p) => ancestor = p.to_path_buf(),
                        None => break,
                    }
                }
                if dir_ignored {
                    continue;
                }
            }
            // Import-time output (module-level print()s) is captured the same
            // way collection.rs brackets a standard .py file's import: on
            // success it's discarded (matches upstream — a cleanly loading
            // conftest's stdout is silent), on failure it's surfaced below so
            // it still reaches the caller's stdout/stderr instead of leaking
            // into whatever the nested run's own capture happened to be.
            python::capture_collect_begin(py);
            let conftest_result = python::collect_conftest(
                py,
                rootdir,
                conftest,
                &mut self.session.registry,
                &mut self.session.py_hooks,
                crate::collect::ImportMode::from_config(&self.config),
                &self.session.initial_paths,
            );
            let conftest_sections = python::capture_collect_end(py);
            if let Err(err) = conftest_result {
                for (title, text) in &conftest_sections {
                    if title == "Captured stderr" {
                        eprint!("{text}");
                    } else {
                        print!("{text}");
                    }
                }
                // -h/--help: upstream still shows the (full) help text rather
                // than aborting on a broken initial conftest — the failure is
                // downgraded to a PytestConfigWarning (Config.parse's
                // ConftestImportFailure handling) and printed alongside the
                // help output (helpconfig.showhelp's warning lines) instead.
                if self.config.help_text.is_some() {
                    let msg = format!("could not load initial conftests: {}", conftest.display());
                    let _ = python::warn_explicit_at(
                        py,
                        "PytestConfigWarning",
                        &msg,
                        &conftest.to_string_lossy(),
                        0,
                    );
                    continue;
                }
                // Conftest import failures are a configuration error (USAGE_ERROR),
                // not a collection error (INTERRUPTED), and print upstream's
                // dedicated "ImportError while loading conftest" repr verbatim
                // (no "ERROR during collection:" wrapper, no session banner —
                // this happens before the header ever prints). Signal both
                // with the sentinel so the caller in mod.rs returns the right
                // exit code.
                let err_msg =
                    python::format_conftest_import_error(py, &err, &conftest.to_string_lossy());
                return Err(format!("\x00CONFTEST_IMPORT_ERROR\x00{err_msg}"));
            }
        }
        // Upstream reports pytest_plugins in non-top-level conftests as an error.
        // When explicit paths are given, conftests in those ascending chains are
        // loaded before configure (exempt). When collecting from invocation_dir,
        // all non-rootdir conftests are loaded after configure and must be checked.
        let scan_skip_loaded = !paths.is_empty();
        scan_nontoplevel_pytest_plugins(
            rootdir,
            start_dirs,
            if scan_skip_loaded { conftests } else { &[] },
            errors,
        );

        // Plugin/conftest pytest_addoption hooks record their option and
        // ini specs (defaults for getoption/getini) before configure.
        if let Err(err) = self.fire_py_addoption_hooks(py) {
            errors.push((rootdir.to_path_buf(), python::format_exception(py, &err)));
        }
        // CLI tokens clap didn't know resolve against the specs registered
        // above; anything still unknown is a usage error (pytest parity).
        if let Err(err) = self.apply_plugin_cli_args(py, true) {
            // Usage errors print their bare message ("ERROR: <message>"),
            // not a class-prefixed traceback line. Prefixed with the usage
            // synopsis, matching upstream's PytestArgumentParser.error()
            // (`self.format_usage() + msg`) for every argparse-style CLI
            // parsing failure (missing value, invalid choice, unrecognized
            // argument).
            if python::is_usage_error(py, &err) {
                return Err(format!(
                    "{}{}",
                    crate::config::Config::USAGE_SYNOPSIS,
                    err.value(py)
                ));
            }
            return Err(python::format_exception(py, &err));
        }
        // Unknown config-option validation (pytest's _validate_config_options):
        // [pytest]-section keys that are neither a registered (plugin/conftest)
        // nor a core ini. Under --strict-config / the strict_config / strict
        // ini, the first is a fatal UsageError; otherwise each warns (and is
        // silenceable via filterwarnings).
        if !self.config.is_worker() {
            let ini_keys = self.config.ini_file_keys();
            let unknown = python::unknown_ini_keys(py, &ini_keys)
                .map_err(|err| python::format_exception(py, &err))?;
            if !unknown.is_empty() {
                let strict_config = self.config.ini_bool("strict_config");
                let strict = self.config.get_flag("strict-config")
                    || strict_config == Some(true)
                    || (strict_config.is_none()
                        && (self.config.get_flag("strict")
                            || self.config.ini_bool("strict") == Some(true)));
                if strict {
                    return Err(format!("Unknown config option: {}", unknown[0]));
                }
                let inipath = self
                    .config
                    .config_file_name
                    .as_ref()
                    .map(|name| rootdir.join(name).to_string_lossy().to_string())
                    .unwrap_or_else(|| rootdir.to_string_lossy().to_string());
                for key in &unknown {
                    let _ = python::warn_explicit_at(
                        py,
                        "PytestConfigWarning",
                        &format!("Unknown config option: {key}"),
                        &inipath,
                        0,
                    );
                }
            }
        }
        // --override-ini keys that aren't registered/core get the same warning
        // as unknown ini file keys (upstream issues this via config.getoption).
        if !self.config.is_worker() {
            let override_keys: Vec<String> = self.config.ini_overrides.keys().cloned().collect();
            if !override_keys.is_empty() {
                let unknown_overrides = python::unknown_ini_keys(py, &override_keys)
                    .map_err(|err| python::format_exception(py, &err))?;
                for key in &unknown_overrides {
                    let _ = python::warn_explicit_at(
                        py,
                        "PytestConfigWarning",
                        &format!("Unknown config option: {key}"),
                        "<cmdline>",
                        0,
                    );
                }
            }
        }
        Ok(())
    }

    /// Phase 5: load_initial_conftests, set up the (possibly replaced) terminal
    /// reporter, fire pytest_configure / pytest_sessionstart, and print the
    /// session header. Returns `Ok(true)` when `--markers` short-circuits
    /// collection (the caller returns its accumulated errors immediately).
    /// Register @pytest.fixture methods from plugin *instances* registered via
    /// `config.pluginmanager.register()` in pytest_configure (#2270). The
    /// bound method carries `self`, so each becomes a plain global fixture.
    pub(crate) fn register_plugin_instance_fixtures(&mut self, py: Python<'_>) -> PyResult<()> {
        let entries = py
            .import("pytest._pluginmanager")?
            .getattr("plugin_instance_fixtures")?
            .call0()?;
        for entry in entries.try_iter()? {
            let entry = entry?;
            let name: String = entry.get_item(0)?.extract()?;
            let bound = entry.get_item(1)?;
            python::register_fixture_def(py, &name, &bound, "", false, &mut self.session.registry)?;
        }
        Ok(())
    }
}
