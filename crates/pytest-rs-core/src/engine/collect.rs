//! Test collection: `Engine::collect` and its per-phase helpers.
//!
//! Collection runs as a sequence of phases — resolve paths, load plugins and
//! conftests, configure, filter, then collect modules/doctests/custom files and
//! finalize the item list. The orchestrator (`collect`) wires the phases; each
//! phase lives in its own helper so the control flow reads top-down.

use std::path::{Path, PathBuf};

use pyo3::prelude::*;

use super::{Engine, scan_nontoplevel_pytest_plugins};
use crate::hooks::HookContext;
use crate::python;

impl Engine {
    /// Returns per-file collection errors (formatted).
    pub(super) fn collect(&mut self, py: Python<'_>) -> Result<Vec<(PathBuf, String)>, String> {
        let rootdir = self.config.rootdir.clone();
        let (paths, mut files) = self.resolve_collection_paths(py, &rootdir)?;
        self.load_cmdline_and_entrypoint_plugins(py)?;
        let (start_dirs, conftests) = self.discover_conftests(&rootdir, &paths, &files);

        let mut errors = Vec::new();
        self.load_and_validate_config(py, &rootdir, &paths, &start_dirs, &conftests, &mut errors)?;
        if self.fire_configure_and_print_header(py, &rootdir, &mut errors)? {
            // --markers (or another short-circuit) handled output; skip collection.
            return Ok(errors);
        }
        self.apply_collect_ignores(py, &rootdir, &paths, &conftests, &mut files);
        self.collect_files(py, &rootdir, &files, &mut errors)?;
        self.collect_extra_and_custom(py, &rootdir, &paths, &files, &mut errors)?;
        self.finalize_items(py, &rootdir, &paths)?;
        Ok(errors)
    }

    /// Phase 1: figure out where collection starts (CLI paths or `testpaths`)
    /// and scan for candidate test files.
    fn resolve_collection_paths(
        &self,
        py: Python<'_>,
        rootdir: &Path,
    ) -> Result<(Vec<String>, Vec<PathBuf>), String> {
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
        let files = crate::collect::collect_test_files(
            &self.config.invocation_dir,
            &paths,
            self.config.get_flag("collect-in-virtualenv"),
            &python_files,
            &norecursedirs,
            self.config.get_flag("keep-duplicates"),
            &crate::collect::CollectIgnores::from_config(&self.config),
        )?;
        Ok((paths, files))
    }

    /// Phase 2: load `-p NAME` / `PYTEST_PLUGINS` cmdline plugins, then
    /// installed pytest11 entry-point plugins — both before conftests.
    fn load_cmdline_and_entrypoint_plugins(&mut self, py: Python<'_>) -> Result<(), String> {
        // -p NAME (non-"no:") plugins import before conftests, like
        // pytest's cmdline plugin loading. PYTEST_PLUGINS (comma-separated
        // module names) loads the same way — pytest's env-driven early
        // plugins, used when PYTEST_DISABLE_PLUGIN_AUTOLOAD is set.
        let mut named_plugins: Vec<String> = self
            .config
            .plugin_opts
            .iter()
            .filter(|spec| !spec.starts_with("no:"))
            .cloned()
            .collect();
        if let Ok(env_plugins) = std::env::var("PYTEST_PLUGINS") {
            for name in env_plugins
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                if !named_plugins.iter().any(|n| n == name) {
                    named_plugins.push(name.to_string());
                }
            }
        }
        if !named_plugins.is_empty()
            && let Err(err) = python::load_named_plugins(
                py,
                &named_plugins,
                Some(&self.config.invocation_dir),
                &mut self.session.registry,
                &mut self.session.py_hooks,
            )
        {
            return Err(python::format_exception(py, &err));
        }

        // Installed third-party plugins (pytest11 entry points) autoload
        // next, before conftests — pytest's setuptools plugin loading.
        // --disable-plugin-autoload (or the env var) suppresses this.
        if !self.config.get_flag("disable-plugin-autoload") {
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
                return Err(python::format_exception(py, &err));
            }
        }
        Ok(())
    }

    /// Phase 3: enumerate the collection start dirs and the conftest chain to
    /// load (ascending from each start dir up to rootdir).
    fn discover_conftests(
        &self,
        rootdir: &Path,
        paths: &[String],
        files: &[PathBuf],
    ) -> (Vec<PathBuf>, Vec<PathBuf>) {
        if self.config.get_flag("noconftest") {
            let start_dirs = if paths.is_empty() {
                vec![self.config.invocation_dir.clone()]
            } else {
                paths
                    .iter()
                    .map(|p| {
                        let resolved = self
                            .config
                            .invocation_dir
                            .join(p.split("::").next().unwrap_or_default());
                        if resolved.is_dir() {
                            resolved
                        } else {
                            resolved
                                .parent()
                                .map(std::path::Path::to_path_buf)
                                .unwrap_or_default()
                        }
                    })
                    .collect()
            };
            return (start_dirs, Vec::new());
        }
        // Conftests load for every collection start dir (even ones with no
        // test files — pytest imports initial conftests during dir scan),
        // plus each collected file's directory chain.
        let mut start_dirs: Vec<PathBuf> = Vec::new();
        if paths.is_empty() {
            start_dirs.push(self.config.invocation_dir.clone());
        } else {
            for path in paths {
                let fs_part = path.split("::").next().unwrap_or_default();
                let resolved = self.config.invocation_dir.join(fs_part);
                if resolved.is_dir() {
                    start_dirs.push(resolved);
                } else if let Some(parent) = resolved.parent() {
                    start_dirs.push(parent.to_path_buf());
                }
            }
        }
        start_dirs.extend(
            files
                .iter()
                .filter_map(|f| f.parent().map(std::path::Path::to_path_buf)),
        );

        // Resolve --confcutdir: if set, skip conftests in ancestors of that dir.
        let confcutdir: Option<PathBuf> = self.config.get_value("confcutdir").map(|v| {
            let p = std::path::Path::new(v);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                self.config.invocation_dir.join(p)
            }
        });

        let is_in_confcutdir = |dir: &std::path::Path| -> bool {
            match &confcutdir {
                None => true,
                // Skip dir if it is a *strict ancestor* of confcutdir
                // (i.e. confcutdir is a descendant of dir → dir is too high up).
                Some(cut) => !cut.starts_with(dir) || dir == cut,
            }
        };

        let mut conftests: Vec<PathBuf> = Vec::new();
        for start in &start_dirs {
            let mut dir = Some(start.as_path());
            let mut chain = Vec::new();
            while let Some(d) = dir {
                if is_in_confcutdir(d) {
                    let conftest = d.join("conftest.py");
                    if conftest.exists() {
                        chain.push(conftest);
                    }
                }
                if d == rootdir {
                    break;
                }
                dir = d.parent();
            }
            chain.reverse();
            for conftest in chain {
                if !conftests.contains(&conftest) {
                    conftests.push(conftest);
                }
            }
        }
        (start_dirs, conftests)
    }

    /// Phase 4: register builtin fixtures, load conftests, fire addoption,
    /// apply plugin CLI args, and validate ini / override-ini option keys.
    fn load_and_validate_config(
        &mut self,
        py: Python<'_>,
        rootdir: &Path,
        paths: &[String],
        start_dirs: &[PathBuf],
        conftests: &[PathBuf],
        errors: &mut Vec<(PathBuf, String)>,
    ) -> Result<(), String> {
        if let Err(err) = python::register_builtin_fixtures(py, &mut self.session.registry) {
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
            if let Err(err) = python::collect_conftest(
                py,
                rootdir,
                conftest,
                &mut self.session.registry,
                &mut self.session.py_hooks,
            ) {
                let err_msg = python::format_exception(py, &err);
                // Conftest import failures are a configuration error (USAGE_ERROR),
                // not a collection error (INTERRUPTED). Signal with the sentinel so
                // the caller in mod.rs returns the right exit code.
                return Err(format!("\x00USAGE_ERROR\x00{err_msg}"));
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
        if let Err(err) = self.apply_plugin_cli_args(py) {
            // Usage errors print their bare message ("ERROR: <message>"),
            // not a class-prefixed traceback line.
            if python::is_usage_error(py, &err) {
                return Err(err.value(py).to_string());
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
    fn register_plugin_instance_fixtures(&mut self, py: Python<'_>) -> PyResult<()> {
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

    fn fire_configure_and_print_header(
        &mut self,
        py: Python<'_>,
        rootdir: &Path,
        errors: &mut Vec<(PathBuf, String)>,
    ) -> Result<bool, String> {
        // pytest_load_initial_conftests (pytest-env sets os.environ here),
        // after option specs are registered so getini resolves, before configure.
        if let Err(err) = self.fire_py_load_initial_conftests(py) {
            errors.push((rootdir.to_path_buf(), python::format_exception(py, &err)));
        }
        // The default 'terminalreporter' plugin registers before configure
        // so reporter-replacing plugins (pytest-sugar/pretty) find it.
        if let Err(err) = python::reporter_setup(py, &self.config) {
            errors.push((rootdir.to_path_buf(), python::format_exception(py, &err)));
        }
        // conftest pytest_configure hooks run once conftests are loaded.
        if let Err(err) = self.fire_py_hooks_simple(py, "pytest_configure") {
            if python::is_usage_error(py, &err) {
                // UsageError in configure → eprintln ERROR: msg, then exit 4.
                let msg = python::format_exception(py, &err);
                // Extract just the exception message (drop "pytest.UsageError: " prefix).
                let usage_msg = msg
                    .lines()
                    .last()
                    .and_then(|l| l.strip_prefix("pytest.UsageError: "))
                    .unwrap_or(msg.trim());
                eprintln!("ERROR: {usage_msg}");
                return Err("\x00USAGE_ERROR\x00".to_string());
            }
            // pytest.exit() in pytest_configure: print "Exit: msg" to stderr
            // (no banner), return the requested exit code.
            if let Some(code) = python::session_abort_code(py, &err) {
                let exit_msg = err
                    .value(py)
                    .getattr("msg")
                    .and_then(|m| m.extract::<String>())
                    .unwrap_or_default();
                eprintln!("Exit: {exit_msg}");
                return Err(format!("\x00EXIT\x00{code}"));
            }
            errors.push((rootdir.to_path_buf(), python::format_exception(py, &err)));
        }
        // A plugin instance registered in pytest_configure (#2270) may define
        // @pytest.fixture methods; register them as global fixtures bound to
        // the instance, so tests can request them.
        if let Err(err) = self.register_plugin_instance_fixtures(py) {
            errors.push((rootdir.to_path_buf(), python::format_exception(py, &err)));
        }
        // A plugin swapped in its own terminal reporter: suppress native
        // terminal output and drive the replacement object instead.
        if let Some(reporter) = python::reporter_replacement(py) {
            self.session.custom_reporter = Some(reporter);
            self.config.set_reporter_delegated();
        }
        // pytest_sessionstart python hooks fire before the header, like
        // upstream (the terminal's own sessionstart, which prints the header,
        // runs last under pluggy LIFO). A conftest sessionstart may stash
        // state the pytest_report_header hooks read back (e.g. config._x).
        if let Err(err) = self.fire_py_sessionstart(py) {
            // pytest.exit() in pytest_sessionstart: print "Exit: msg" to stderr
            // AND show a "!!! Exit: msg !!!" banner in stdout.
            if let Some(code) = python::session_abort_code(py, &err) {
                let exit_msg = err
                    .value(py)
                    .getattr("msg")
                    .and_then(|m| m.extract::<String>())
                    .unwrap_or_default();
                eprintln!("Exit: {exit_msg}");
                self.session.abort_banner = Some(format!("Exit: {exit_msg}"));
                return Err(format!("\x00EXIT\x00{code}"));
            }
            // An unexpected exception in pytest_sessionstart is an INTERNALERROR
            // (exit 3), not a collection error (exit 2). Signal the caller with
            // a sentinel prefix so it can print the INTERNALERROR banner.
            let msg = python::format_exception(py, &err);
            return Err(format!("\x00INTERNAL\x00{msg}"));
        }
        // The session header: the replacement reporter's pytest_sessionstart
        // owns it in delegated mode (upstream prints it from that hook);
        // otherwise the native header plus pytest_report_header lines
        // (e.g. pytest-timeout's "timeout: 1.0s" block).
        if self.session.custom_reporter.is_some() {
            python::reporter_sessionstart(py, &self.config);
        } else {
            self.print_header(py);
            if let Err(err) = self.print_py_report_header(py) {
                errors.push((rootdir.to_path_buf(), python::format_exception(py, &err)));
            }
        }
        // --markers: list registered markers (configure hooks above already
        // ran their addinivalue_line("markers", ...)) and skip collection.
        if self.config.get_flag("markers") {
            if let Err(err) = self.print_markers(py) {
                errors.push((rootdir.to_path_buf(), python::format_exception(py, &err)));
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// Phase 6: apply collect_ignore / collect_ignore_glob / pytest_ignore_collect
    /// from loaded conftests as a post-filter over the candidate file set.
    fn apply_collect_ignores(
        &mut self,
        py: Python<'_>,
        rootdir: &Path,
        paths: &[String],
        conftests: &[PathBuf],
        files: &mut Vec<PathBuf>,
    ) {
        // Note: for explicit path args, pytest_ignore_collect is NOT called (upstream
        // "not called on argument" behaviour). collect_ignore is always applied.
        let no_explicit_file_args = paths.is_empty();
        // Gather ignore paths/globs from all loaded conftest modules.
        let mut extra_ignore_paths: Vec<std::path::PathBuf> = Vec::new();
        let mut extra_ignore_globs: Vec<String> = Vec::new();
        for conftest_path in conftests {
            if let Some(conftest_dir) = conftest_path.parent() {
                let (mut paths_from, mut globs_from) =
                    python::extract_collect_ignores(py, conftest_dir, conftest_path);
                extra_ignore_paths.append(&mut paths_from);
                extra_ignore_globs.append(&mut globs_from);
            }
        }
        if !extra_ignore_paths.is_empty() || !extra_ignore_globs.is_empty() {
            files.retain(|f| {
                let f_canonical = std::fs::canonicalize(f).unwrap_or_else(|_| f.clone());
                // collect_ignore: check if file or any ancestor is in the ignore list
                for ip in &extra_ignore_paths {
                    let ip_canonical = std::fs::canonicalize(ip).unwrap_or_else(|_| ip.clone());
                    if f_canonical.starts_with(&ip_canonical) || f_canonical == ip_canonical {
                        return false;
                    }
                }
                // collect_ignore_glob: check against full path
                if !extra_ignore_globs.is_empty() {
                    let f_str = f_canonical.to_string_lossy();
                    let f_name = f_canonical
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("");
                    for glob in &extra_ignore_globs {
                        if crate::collect::wildcard_match(glob, f_name)
                            || crate::collect::wildcard_match(glob, &f_str)
                        {
                            return false;
                        }
                    }
                }
                true
            });
        }
        // pytest_ignore_collect: only applied when no explicit file args
        if no_explicit_file_args {
            // Cache ignored/ok directories to avoid redundant hook calls.
            let mut known_ignored: std::collections::HashSet<PathBuf> = Default::default();
            let mut known_ok: std::collections::HashSet<PathBuf> = Default::default();

            let mut kept = Vec::with_capacity(files.len());
            for f in files.drain(..) {
                // Check ancestor directories: real pytest calls pytest_ignore_collect on
                // directories too, not just files. If a parent dir is ignored, all files
                // within it are skipped without inspecting them individually.
                let mut dir_ignored = false;
                let mut ancestor = f.parent().map(std::path::Path::to_path_buf);
                while let Some(ref d) = ancestor {
                    if d == rootdir {
                        break;
                    }
                    if known_ignored.contains(d) {
                        dir_ignored = true;
                        break;
                    }
                    if !known_ok.contains(d) {
                        if python::call_ignore_collect_hooks(py, &self.session.py_hooks, d, rootdir)
                            .is_some()
                        {
                            known_ignored.insert(d.clone());
                            dir_ignored = true;
                            break;
                        }
                        known_ok.insert(d.clone());
                    }
                    ancestor = d.parent().map(std::path::Path::to_path_buf);
                }
                if dir_ignored {
                    continue;
                }

                match python::call_ignore_collect_hooks(py, &self.session.py_hooks, &f, rootdir) {
                    None => kept.push(f),
                    Some(None) => {} // ignored silently
                    Some(Some(reason)) => {
                        // pytest.skip() in the hook: emit a skip report for this file
                        let nodeid = crate::collect::file_nodeid(rootdir, &f);
                        self.session.reports.push(crate::report::TestReport {
                            nodeid,
                            phase: crate::report::Phase::Setup,
                            outcome: crate::report::Outcome::Skipped,
                            duration: std::time::Duration::ZERO,
                            longrepr: Some(reason),
                            location: None,
                            subtest_desc: None,
                            sections: Vec::new(),
                            rerun: false,
                            xfail_longrepr: None,
                            reprcrash_message: None,
                            head_line: None,
                        });
                    }
                }
            }
            *files = kept;
        }
    }

    /// Phase 7: import each Python test file (recording collect errors /
    /// module-level skips), collecting its functions and `--doctest-modules`
    /// doctests; explicit non-Python files are routed to the doctest collector
    /// or recorded as "not found".
    fn collect_files(
        &mut self,
        py: Python<'_>,
        rootdir: &Path,
        files: &[PathBuf],
        errors: &mut Vec<(PathBuf, String)>,
    ) -> Result<(), String> {
        // pytest's catching_logs around pytest_collection: a root handler
        // during import keeps module-level logging calls from triggering
        // logging.basicConfig (issue #6240).
        let log_level_cfg: Option<String> = self
            .config
            .get_value("log-level")
            .map(str::to_string)
            .or_else(|| self.config.get_ini("log_level").map(str::to_string));
        python::log_start_phase(py, "collection", log_level_cfg.as_deref());
        // Expose pytest_pycollect_makeitem hooks to Python for collect_class.
        {
            use pyo3::types::PyAnyMethods;
            let makeitem_hooks: Vec<Py<PyAny>> = self
                .session
                .py_hooks
                .iter()
                .filter(|h| h.name == "pytest_pycollect_makeitem")
                .map(|h| h.func.clone_ref(py))
                .collect();
            let _ = py
                .import("pytest._node")
                .and_then(|m| m.call_method1("set_pycollect_hooks", (makeitem_hooks,)));
        }
        // pytest_collect_directory: conftest hooks may reject directories
        // (return None) to prevent collection. Pre-compute the set of rejected
        // dirs so files in them are skipped.
        let rejected_dirs: std::collections::HashSet<PathBuf> = {
            let mut rejected = std::collections::HashSet::new();
            if python::has_collect_directory_hook(py, &self.session.py_hooks) {
                let mut checked: std::collections::HashSet<PathBuf> = Default::default();
                for file in files {
                    if let Some(parent) = file.parent() {
                        let dir = parent.to_path_buf();
                        if checked.insert(dir.clone())
                            && let python::CollectDirResult::Skip =
                                python::call_collect_directory_hook(py, &dir, rootdir)
                        {
                            rejected.insert(dir);
                        }
                    }
                }
            }
            rejected
        };

        // Explicit non-Python, non-text-doctest file args that no collector handles.
        let mut not_found_files: Vec<PathBuf> = Vec::new();
        for file in files {
            if file
                .parent()
                .is_some_and(|parent| rejected_dirs.contains(parent))
            {
                continue;
            }
            // --maxfail aborts collection once the budget is spent on
            // collection errors, ignoring further files.
            if let Some(m) = self.config.maxfail()
                && errors.len() >= m
            {
                break;
            }
            let is_py = file.extension().and_then(|e| e.to_str()) == Some("py");
            if !is_py {
                // Non-Python files: only text files with doctest content.
                // For explicitly-specified files, collect regardless of --doctest-glob.
                // For scanned files, the glob loop below handles them.
                let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
                let is_text_doctest = matches!(ext, "txt" | "rst" | "md");
                if is_text_doctest {
                    if let Ok(py_config) = python::make_py_config(py, &self.config)
                        && let Err(err) = python::collect_doctests_from_textfile(
                            py,
                            rootdir,
                            file,
                            &py_config,
                            &mut self.session.items,
                        )
                    {
                        errors.push((file.clone(), python::format_exception(py, &err)));
                    }
                } else {
                    // No collector can handle this file type (e.g. .pyc).
                    not_found_files.push(file.clone());
                }
                continue;
            }
            // Import-time output attaches to a failing collect report as
            // "Captured stdout/stderr" sections (pytest's
            // pytest_make_collect_report capture).
            python::capture_collect_begin(py);
            // Where this file's items start: --doctest-modules inserts the
            // module's doctest items BEFORE its functions (upstream order).
            let file_items_start = self.session.items.len();
            let collect_result = python::collect_module(
                py,
                rootdir,
                file,
                &mut self.session.items,
                &mut self.session.registry,
                &mut self.session.py_hooks,
                &python::NameFilters::from_config(py, &self.config),
            );
            let collect_sections = python::capture_collect_end(py);
            let with_sections = |mut message: String| {
                for (title, text) in &collect_sections {
                    message.push_str(&format!(
                        "\n{:-^80}\n{}",
                        format!(" {title} "),
                        text.trim_end_matches('\n')
                    ));
                }
                message
            };
            let module_ok = match collect_result {
                Ok(()) => true,
                Err(ref err) if err.is_instance_of::<pyo3::exceptions::PyKeyboardInterrupt>(py) => {
                    // KeyboardInterrupt during collection stops immediately with
                    // the "!!! KeyboardInterrupt !!!" banner (exit 2).
                    return Err("\x00KEYBOARD_INTERRUPT\x00".to_string());
                }
                Err(ref err) if err.is_instance_of::<pyo3::exceptions::PySystemExit>(py) => {
                    // SystemExit during collection is an INTERNALERROR.
                    let msg = python::format_exception(py, err);
                    return Err(format!("\x00INTERNAL\x00{msg}"));
                }
                Err(err) => {
                    // pytest.skip(..., allow_module_level=True) or
                    // unittest.SkipTest at module import skip the whole module;
                    // a bare pytest.skip there is an error.
                    match python::module_level_skip(py, &err) {
                        Some(Ok(reason)) => {
                            let nodeid = crate::collect::file_nodeid(rootdir, file);
                            // The skip call site (file:line), like pytest.
                            let location = python::raise_location(py, &err)
                                .unwrap_or_else(|| format!("{nodeid}:1"));
                            self.session.skipped_modules.push((
                                nodeid.clone(),
                                reason.clone(),
                                location.clone(),
                            ));
                            self.session.reports.push(crate::report::TestReport {
                                nodeid: nodeid.clone(),
                                phase: crate::report::Phase::Setup,
                                outcome: crate::report::Outcome::Skipped,
                                duration: std::time::Duration::ZERO,
                                longrepr: Some(reason),
                                location: Some(location),
                                subtest_desc: None,
                                sections: Vec::new(),
                                rerun: false,
                                xfail_longrepr: None,
                                reprcrash_message: None,
                                head_line: None,
                            });
                        }
                        Some(Err(message)) => errors.push((file.clone(), with_sections(message))),
                        // CollectError carries a user-facing message, no traceback.
                        None => {
                            match python::collect_error_message(py, &err) {
                                Some(message) => {
                                    errors.push((file.clone(), with_sections(message)))
                                }
                                None if python::is_import_error(py, &err) => {
                                    // A test module that fails to import gets
                                    // pytest's wrapped CollectError header
                                    // (importtestmodule), with a short-style
                                    // traceback. At -vv (verbosity >= 2) pytest
                                    // keeps its own framework frames in the
                                    // traceback; below that it filters them out.
                                    // We import via py.import (user frames only),
                                    // so re-import through import_path at -vv to
                                    // surface the `_pytest` frames faithfully.
                                    let tb = if self.config.global_verbosity() >= 2 {
                                        python::format_import_traceback_verbose(py, rootdir, file)
                                            .unwrap_or_else(|| {
                                                python::format_test_failure(py, &err, "short")
                                            })
                                    } else {
                                        python::format_test_failure(py, &err, "short")
                                    };
                                    let message = format!(
                                        "ImportError while importing test module '{}'.\n\
                                         Hint: make sure your test modules/packages have valid Python names.\n\
                                         Traceback:\n{tb}",
                                        file.display()
                                    );
                                    errors.push((file.clone(), with_sections(message)));
                                }
                                None => errors.push((
                                    file.clone(),
                                    // pytest-style frames + E lines (upstream
                                    // collect errors honor --tb; default "short"
                                    // matches pytest's auto style for collection).
                                    with_sections(python::format_test_failure(
                                        py,
                                        &err,
                                        self.config.get_value("tb").unwrap_or("short"),
                                    )),
                                )),
                            }
                            // Upstream DoctestModule: with --doctest-ignore-import-errors
                            // the doctest collector skips while the Module still errors.
                            if self.config.get_flag("doctest-modules")
                                && self.config.get_flag("doctest-ignore-import-errors")
                            {
                                let nodeid = crate::collect::file_nodeid(rootdir, file);
                                let longrepr = format!(
                                    "unable to import module PosixPath('{}')",
                                    file.display()
                                );
                                python::record_collect_skip(py, &nodeid, &longrepr);
                                self.session.reports.push(crate::report::TestReport {
                                    nodeid: nodeid.clone(),
                                    phase: crate::report::Phase::Setup,
                                    outcome: crate::report::Outcome::Skipped,
                                    duration: std::time::Duration::ZERO,
                                    longrepr: Some(longrepr),
                                    location: Some(format!("{nodeid}:1")),
                                    subtest_desc: None,
                                    sections: Vec::new(),
                                    rerun: false,
                                    xfail_longrepr: None,
                                    reprcrash_message: None,
                                    head_line: None,
                                });
                            }
                        }
                    }
                    false
                }
            };
            // --doctest-modules: collect doctests from each successfully-imported module.
            if module_ok
                && self.config.get_flag("doctest-modules")
                && let Ok(py_config) = python::make_py_config(py, &self.config)
            {
                let doctests_start = self.session.items.len();
                match python::collect_doctests_from_module(
                    py,
                    rootdir,
                    file,
                    &py_config,
                    &mut self.session.items,
                ) {
                    Ok(()) => {
                        // The module's doctests run BEFORE its functions
                        // (upstream collects the DoctestModule first).
                        let n_doctests = self.session.items.len().saturating_sub(doctests_start);
                        self.session.items[file_items_start..].rotate_right(n_doctests);
                    }
                    Err(err) => {
                        // Non-fatal: log as collect error and continue.
                        errors.push((file.clone(), python::format_exception(py, &err)));
                    }
                }
            }
        }

        // Explicit file args with no matching collector → USAGE_ERROR.
        if !not_found_files.is_empty() {
            for file in &not_found_files {
                eprintln!("ERROR: not found: {}", file.display());
                eprintln!("(no match in any of [<Session ''>])");
                eprintln!();
            }
            return Err("\x00USAGE_ERROR\x00".to_string());
        }
        Ok(())
    }

    /// Phase 8: `--doctest-modules` whole-tree scan, ambient text-doctest
    /// files, and plugin `pytest_collect_file` custom collectors.
    fn collect_extra_and_custom(
        &mut self,
        py: Python<'_>,
        rootdir: &Path,
        paths: &[String],
        files: &[PathBuf],
        errors: &mut Vec<(PathBuf, String)>,
    ) -> Result<(), String> {
        // --doctest-modules: also scan ALL .py files (not just test files) for doctests.
        if self.config.get_flag("doctest-modules") {
            let extra_py = crate::collect::collect_all_python_files(
                &self.config.invocation_dir,
                paths,
                self.config.get_flag("collect-in-virtualenv"),
                files,
            );
            if let Ok(py_config) = python::make_py_config(py, &self.config) {
                for extra_file in &extra_py {
                    // Import the module and collect doctests.
                    if let Err(err) = python::collect_doctests_from_module(
                        py,
                        rootdir,
                        extra_file,
                        &py_config,
                        &mut self.session.items,
                    ) {
                        // Import errors skip the module with --doctest-ignore-import-errors.
                        if self.config.get_flag("doctest-ignore-import-errors") {
                            let nodeid = crate::collect::file_nodeid(rootdir, extra_file);
                            let longrepr = format!(
                                "unable to import module PosixPath('{}')",
                                extra_file.display()
                            );
                            python::record_collect_skip(py, &nodeid, &longrepr);
                            self.session.skipped_modules.push((
                                nodeid.clone(),
                                longrepr.clone(),
                                format!("{nodeid}:1"),
                            ));
                            self.session.reports.push(crate::report::TestReport {
                                nodeid: nodeid.clone(),
                                phase: crate::report::Phase::Setup,
                                outcome: crate::report::Outcome::Skipped,
                                duration: std::time::Duration::ZERO,
                                longrepr: Some(longrepr),
                                location: Some(format!("{nodeid}:1")),
                                subtest_desc: None,
                                sections: Vec::new(),
                                rerun: false,
                                xfail_longrepr: None,
                                reprcrash_message: None,
                                head_line: None,
                            });
                        } else {
                            errors.push((extra_file.clone(), python::format_exception(py, &err)));
                        }
                    }
                }
            }
        }

        // Text files matching the glob (default: test*.txt) are always collected
        // even without explicit --doctest-modules or --doctest-glob flags, mirroring
        // upstream pytest's _is_doctest() behavior.
        let scan_text_files = true;
        if scan_text_files && let Ok(py_config) = python::make_py_config(py, &self.config) {
            let text_files =
                crate::collect::collect_doctest_textfiles(&self.config.invocation_dir, paths);
            for tf in text_files {
                // Skip files already collected in the explicit-file loop above.
                if files.contains(&tf) {
                    continue;
                }
                if let Ok(true) = python::is_doctest_textfile(py, &tf, &py_config)
                    && let Err(err) = python::collect_doctests_from_textfile(
                        py,
                        rootdir,
                        &tf,
                        &py_config,
                        &mut self.session.items,
                    )
                {
                    errors.push((tf.clone(), python::format_exception(py, &err)));
                }
            }
        }

        // Custom collectors: plugins like pytest-ruff / pytest-mypy collect
        // non-test files via pytest_collect_file -> pytest.File.collect().
        // Only walk the (broader) candidate file set when such a hook exists.
        if python::has_collect_file_hook(py, &self.session.py_hooks) {
            let candidate = crate::collect::collect_all_files(
                &self.config.invocation_dir,
                paths,
                self.config.get_flag("collect-in-virtualenv"),
            );
            let hooks = std::mem::take(&mut self.session.py_hooks);
            let result = python::collect_custom_files(
                py,
                rootdir,
                &candidate,
                &hooks,
                &mut self.session.items,
            );
            self.session.py_hooks = hooks;
            match result {
                Ok(collect_result) => {
                    if !collect_result.skipped.is_empty() {
                        let skipped_set: std::collections::HashSet<&PathBuf> =
                            collect_result.skipped.iter().map(|(p, _)| p).collect();
                        self.session
                            .items
                            .retain(|item| !skipped_set.contains(&item.path));
                        self.session.collect_file_skips.extend(
                            collect_result.skipped.into_iter().map(|(p, reason)| {
                                (crate::collect::file_nodeid(rootdir, &p), reason)
                            }),
                        );
                    }
                    for (path, longrepr) in collect_result.errors {
                        errors.push((path, longrepr));
                    }
                }
                Err(err) => {
                    errors.push((rootdir.to_path_buf(), python::format_exception(py, &err)));
                }
            }
        }

        // Collection over: close its catching_logs phase.
        python::log_end_phase(py);
        Ok(())
    }

    /// Phase 9: expand parametrized-fixture closures, record closure
    /// fixturenames, apply node-id arg selection, and `--lf` filtering.
    fn finalize_items(
        &mut self,
        py: Python<'_>,
        rootdir: &Path,
        paths: &[String],
    ) -> Result<(), String> {
        // Expand items over parametrized fixtures in their closure; plugins
        // first get to inject closure-affecting marks (anyio's usefixtures).
        let mut items = std::mem::take(&mut self.session.items);
        {
            let mut ctx = HookContext {
                py,
                session: &mut self.session,
                config: &self.config,
            };
            for plugin in &self.plugins {
                if let Err(err) = plugin.pytest_collection_preexpand(&mut ctx, &mut items) {
                    self.session.items = items;
                    return Err(python::format_exception(py, &err));
                }
            }
        }
        match python::expand_fixture_params(py, items, &self.session.registry) {
            Ok(expanded) => self.session.items = expanded,
            Err(err) => return Err(python::format_exception(py, &err)),
        }

        // Scope-based item reordering: when metafunc.parametrize(scope=...)
        // uses a scope higher than function, items must be reordered so
        // that the high-scope parameter value changes as infrequently as
        // possible (matching real pytest's reorder_items).
        reorder_items_by_param_scope(&mut self.session.items);

        // request.fixturenames must list the item's whole fixture closure
        // (transitive deps + autouse), not just its direct params — plugins
        // probe it (pytest-django: "transactional_db" in request.fixturenames,
        // pulled in transitively by django_db_reset_sequences). Record the
        // closure-only names as extra fixturenames (display only; the fixtures
        // themselves resolve through the dependency chain).
        for item in &mut self.session.items {
            let mut direct: Vec<String> = item.fixture_names.clone();
            direct.extend(item.extra_fixture_names.iter().cloned());
            // Directly-parametrized argnames shadow a same-named fixture
            // (PseudoFixtureDef): keep them in the closure but don't expand
            // their dependencies.
            let ignore: std::collections::HashSet<String> =
                item.callspec.iter().map(|(name, _)| name.clone()).collect();
            let closure = self
                .session
                .registry
                .closure_for(&item.nodeid, &direct, &ignore);
            for def in closure {
                if !item.fixture_names.contains(&def.name)
                    && !item.extra_fixture_names.contains(&def.name)
                {
                    item.extra_fixture_names.push(def.name.clone());
                }
            }
        }

        // Node-id args ("file.py::TestCls::test_a") restrict collection to
        // matching items; unlike -k/-m this is not a deselection.
        enum ArgSel {
            Path(PathBuf),
            NodeId(String),
        }
        if paths.iter().any(|arg| arg.contains("::")) {
            let arg_sels: Vec<ArgSel> = paths
                .iter()
                .map(|arg| match arg.split_once("::") {
                    Some((file_part, rest)) => {
                        let path = self.config.invocation_dir.join(file_part);
                        let path = std::fs::canonicalize(&path).unwrap_or(path);
                        ArgSel::NodeId(format!(
                            "{}::{}",
                            crate::collect::file_nodeid(rootdir, &path),
                            rest
                        ))
                    }
                    None => {
                        let path = self.config.invocation_dir.join(arg);
                        ArgSel::Path(std::fs::canonicalize(&path).unwrap_or(path))
                    }
                })
                .collect();
            self.session.items.retain(|item| {
                arg_sels.iter().any(|sel| match sel {
                    ArgSel::Path(path) => item.path.starts_with(path),
                    ArgSel::NodeId(sel) => {
                        item.nodeid == *sel
                            || item
                                .nodeid
                                .strip_prefix(sel.as_str())
                                .is_some_and(|rest| rest.starts_with('[') || rest.starts_with("::"))
                    }
                })
            });
            // Emit "not found" error to stderr for NodeId args that matched nothing.
            for sel in &arg_sels {
                if let ArgSel::NodeId(nodeid) = sel {
                    let matched = self.session.items.iter().any(|item| {
                        item.nodeid == *nodeid
                            || item
                                .nodeid
                                .strip_prefix(nodeid.as_str())
                                .is_some_and(|r| r.starts_with('[') || r.starts_with("::"))
                    });
                    if !matched {
                        eprintln!("ERROR: not found: {nodeid}");
                    }
                }
            }
        }

        // --lf drops failure-free files (and non-failed top-level functions
        // of failed files) at collection time.
        if let Some(cache) = &mut self.cache {
            cache.filter_collected_items(
                rootdir,
                &self.config.invocation_dir,
                paths,
                &mut self.session.items,
            );
        }
        Ok(())
    }
}

/// A high-scope parametrization identity: items sharing one are grouped so
/// the fixture set up for that value is reused. Mirrors pytest's ParamArgKey
/// (argname, param_index, scoped_path/cls) — the boundary string folds the
/// path/class component.
type ParamArgKey = (String, usize, String);

/// High scopes, outermost first (pytest's HIGH_SCOPES).
const HIGH_SCOPES: [crate::fixture::Scope; 4] = [
    crate::fixture::Scope::Session,
    crate::fixture::Scope::Package,
    crate::fixture::Scope::Module,
    crate::fixture::Scope::Class,
];

fn next_lower_scope(scope: crate::fixture::Scope) -> crate::fixture::Scope {
    use crate::fixture::Scope;
    match scope {
        Scope::Session => Scope::Package,
        Scope::Package => Scope::Module,
        Scope::Module => Scope::Class,
        _ => Scope::Function,
    }
}

/// Order-preserving dedup (pytest's `dict.fromkeys`).
fn dedup_keys(keys: Vec<ParamArgKey>) -> Vec<ParamArgKey> {
    let mut seen = std::collections::HashSet::new();
    keys.into_iter()
        .filter(|k| seen.insert(k.clone()))
        .collect()
}

/// Reorder items so higher-scoped parametrized fixtures change as
/// infrequently as possible — a faithful port of pytest's `reorder_items`,
/// recursively grouping by Session→Package→Module→Class param values.
fn reorder_items_by_param_scope(items: &mut Vec<crate::collect::TestItem>) {
    use crate::fixture::Scope;
    use std::collections::HashMap;

    if items
        .iter()
        .all(|item| item.max_param_scope == Scope::Function)
    {
        return;
    }

    // Per scope: each item's ParamArgKeys, and items grouped by argkey (in
    // item order). `items_by_argkey` is mutated during reordering to keep
    // lower-scope grouping consistent with higher-scope decisions.
    let mut argkeys_by_item: HashMap<Scope, HashMap<usize, Vec<ParamArgKey>>> = HashMap::new();
    let mut items_by_argkey: HashMap<Scope, HashMap<ParamArgKey, Vec<usize>>> = HashMap::new();
    for &scope in &HIGH_SCOPES {
        let mut abi: HashMap<usize, Vec<ParamArgKey>> = HashMap::new();
        let mut iba: HashMap<ParamArgKey, Vec<usize>> = HashMap::new();
        for (idx, item) in items.iter().enumerate() {
            let keys = dedup_keys(
                item.scope_sort_keys
                    .iter()
                    .filter(|(_, s, _)| *s == scope)
                    .map(|(arg, _, i)| (arg.clone(), *i, scope_boundary(&item.nodeid, scope)))
                    .collect(),
            );
            if !keys.is_empty() {
                for k in &keys {
                    iba.entry(k.clone()).or_default().push(idx);
                }
                abi.insert(idx, keys);
            }
        }
        argkeys_by_item.insert(scope, abi);
        items_by_argkey.insert(scope, iba);
    }

    let initial: Vec<usize> = (0..items.len()).collect();
    let ordered = reorder_items_atscope(
        &initial,
        &argkeys_by_item,
        &mut items_by_argkey,
        Scope::Session,
    );
    // Safety: only apply a full permutation (every item exactly once).
    if ordered.len() != items.len() {
        return;
    }
    let mut taken: Vec<Option<crate::collect::TestItem>> = items.drain(..).map(Some).collect();
    *items = ordered
        .into_iter()
        .map(|i| taken[i].take().expect("each index used once"))
        .collect();
}

fn reorder_items_atscope(
    items: &[usize],
    argkeys_by_item: &std::collections::HashMap<
        crate::fixture::Scope,
        std::collections::HashMap<usize, Vec<ParamArgKey>>,
    >,
    items_by_argkey: &mut std::collections::HashMap<
        crate::fixture::Scope,
        std::collections::HashMap<ParamArgKey, Vec<usize>>,
    >,
    scope: crate::fixture::Scope,
) -> Vec<usize> {
    use crate::fixture::Scope;
    use std::collections::{HashSet, VecDeque};

    if scope == Scope::Function || items.len() < 3 {
        return items.to_vec();
    }
    let items_set: HashSet<usize> = items.iter().copied().collect();
    let mut ignore: HashSet<ParamArgKey> = HashSet::new();
    let mut deque: VecDeque<usize> = items.iter().copied().collect();
    let mut items_done: Vec<usize> = Vec::new();
    let mut done_set: HashSet<usize> = HashSet::new();

    while !deque.is_empty() {
        let mut no_argkey_items: Vec<usize> = Vec::new();
        let mut no_argkey_set: HashSet<usize> = HashSet::new();
        let mut slicing_argkey: Option<ParamArgKey> = None;
        while let Some(item) = deque.pop_front() {
            if done_set.contains(&item) || no_argkey_set.contains(&item) {
                continue;
            }
            let argkeys = dedup_keys(
                argkeys_by_item[&scope]
                    .get(&item)
                    .map(|ks| {
                        ks.iter()
                            .filter(|k| !ignore.contains(*k))
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default(),
            );
            if argkeys.is_empty() {
                no_argkey_items.push(item);
                no_argkey_set.insert(item);
            } else {
                // pytest's popitem() pops the last key.
                let sk = argkeys.last().cloned().expect("non-empty");
                slicing_argkey = Some(sk.clone());
                let matching: Vec<usize> = items_by_argkey[&scope][&sk]
                    .iter()
                    .copied()
                    .filter(|i| items_set.contains(i))
                    .collect();
                for &i in matching.iter().rev() {
                    deque.push_front(i);
                    // Move i to the front of every argkey list it belongs to,
                    // across all high scopes (pytest's move_to_end last=False).
                    for &other_scope in &HIGH_SCOPES {
                        if let Some(keys) = argkeys_by_item[&other_scope].get(&i) {
                            let keys = keys.clone();
                            let scoped = items_by_argkey.get_mut(&other_scope).expect("scope");
                            for argkey in &keys {
                                if let Some(v) = scoped.get_mut(argkey) {
                                    v.retain(|&x| x != i);
                                    v.insert(0, i);
                                }
                            }
                        }
                    }
                }
                break;
            }
        }
        if !no_argkey_items.is_empty() {
            let reordered = reorder_items_atscope(
                &no_argkey_items,
                argkeys_by_item,
                items_by_argkey,
                next_lower_scope(scope),
            );
            for i in reordered {
                if done_set.insert(i) {
                    items_done.push(i);
                }
            }
        }
        if let Some(sk) = slicing_argkey {
            ignore.insert(sk);
        }
    }
    items_done
}

/// Extract the scope boundary key from a nodeid.
/// Session: "" (all items grouped together)
/// Module: "file.py" (everything before the first "::")
/// Class: "file.py::ClassName" (everything before the last "::" if there's
///        a class, otherwise the module)
fn scope_boundary(nodeid: &str, scope: crate::fixture::Scope) -> String {
    use crate::fixture::Scope;
    let module_path = || nodeid.split_once("::").map(|(m, _)| m).unwrap_or(nodeid);
    match scope {
        Scope::Session => String::new(),
        // Package scope groups by the module's directory.
        Scope::Package => module_path()
            .rsplit_once('/')
            .map(|(dir, _)| dir.to_string())
            .unwrap_or_default(),
        Scope::Module => module_path().to_string(),
        Scope::Class => {
            // file.py::Class::func[params] → "file.py::Class"
            // file.py::func[params] → "file.py" (no class)
            let base = nodeid.split('[').next().unwrap_or(nodeid);
            let parts: Vec<&str> = base.splitn(3, "::").collect();
            if parts.len() >= 3 {
                format!("{}::{}", parts[0], parts[1])
            } else {
                parts[0].to_string()
            }
        }
        Scope::Function => nodeid.to_string(),
    }
}
