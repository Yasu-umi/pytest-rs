use std::path::{Path, PathBuf};

use pyo3::prelude::*;

use super::super::Engine;
use crate::python;

impl Engine {
    pub(crate) fn load_cmdline_and_entrypoint_plugins(
        &mut self,
        py: Python<'_>,
    ) -> Result<(), String> {
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
    pub(crate) fn discover_conftests(
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
}
