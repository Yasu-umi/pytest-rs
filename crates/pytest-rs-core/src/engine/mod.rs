use pyo3::prelude::*;

use crate::config::Config;
use crate::hooks::Plugin;
use crate::session::Session;

/// Marks owned by the core or bundled plugins.
pub(crate) const BUILTIN_MARKS: [&str; 14] = [
    "skip",
    "skipif",
    "xfail",
    "parametrize",
    "usefixtures",
    "filterwarnings",
    "tryfirst",
    "trylast",
    "asyncio",
    "anyio",
    "benchmark",
    "no_cover",
    "xdist_group",
    // Registered by the bundled pytester plugin (pytest.mark.pytester_example_path).
    "pytester_example_path",
];

pub struct Engine {
    pub plugins: Vec<Box<dyn Plugin>>,
    pub session: Session,
    pub config: Config,
    /// cacheprovider state (--lf/--ff/--nf, lastfailed persistence).
    cache: Option<crate::cache::CacheState>,
}

mod collect;
mod hooks;
pub mod inprocess;
mod lifecycle;
mod nested;
mod reporting;
mod selection;
mod session;
mod terminal;

/// E-prefixed explanation line, else the exception line.
fn short_message(longrepr: &str) -> Option<String> {
    let from_e_line = longrepr.lines().find_map(|line| {
        line.strip_prefix("E ")
            .map(|rest| rest.trim_start().to_string())
    });
    from_e_line
        .or_else(|| {
            // Native exception-group repr: the group's own message line
            // ("ExceptionGroup: ... (2 sub-exceptions)"), not the box art.
            longrepr
                .lines()
                .find(|line| line.trim_end().ends_with("sub-exceptions)"))
                .or_else(|| {
                    longrepr
                        .lines()
                        .find(|line| line.trim_end().ends_with("sub-exception)"))
                })
                .map(|line| line.trim().trim_start_matches('|').trim().to_string())
        })
        .or_else(|| {
            longrepr
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .map(|line| line.trim().to_string())
        })
        .filter(|message| !message.is_empty())
}

pub fn center_banner(label: &str) -> String {
    center_with(label, '=')
}

fn center_named(label: &str) -> String {
    center_with(label, '_')
}

pub fn center_with(label: &str, fill: char) -> String {
    const WIDTH: usize = 80;
    let label = format!(" {label} ");
    let pad = WIDTH.saturating_sub(label.len());
    let left = (pad / 2).max(1);
    let right = (pad - pad / 2).max(1);
    format!(
        "{}{}{}",
        fill.to_string().repeat(left),
        label,
        fill.to_string().repeat(right)
    )
}

/// Scan the collection start directories (and their subdirs) for conftest.py
/// files not already loaded. If any non-top-level conftest contains
/// `pytest_plugins`, add an error — upstream reports this since pytest 7.
fn scan_nontoplevel_pytest_plugins(
    rootdir: &std::path::Path,
    start_dirs: &[std::path::PathBuf],
    skip_loaded: &[std::path::PathBuf],
    errors: &mut Vec<(std::path::PathBuf, String)>,
) {
    fn walk(
        dir: &std::path::Path,
        rootdir: &std::path::Path,
        skip_loaded: &[std::path::PathBuf],
        errors: &mut Vec<(std::path::PathBuf, String)>,
    ) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut children: Vec<std::path::PathBuf> =
            entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        children.sort();
        for child in children {
            if child.is_dir() {
                // Don't descend into hidden dirs or known skip dirs.
                if child
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with('.') || n == "__pycache__" || n == "node_modules")
                    .unwrap_or(false)
                {
                    continue;
                }
                walk(&child, rootdir, skip_loaded, errors);
            } else if child.file_name().and_then(|n| n.to_str()) == Some("conftest.py") {
                // Top-level (rootdir/conftest.py) is exempt.
                if child.parent() == Some(rootdir) {
                    continue;
                }
                // Conftests in the ascending chain from explicit test paths are
                // loaded before configure in real pytest and are exempt.
                if skip_loaded.contains(&child) {
                    continue;
                }
                // Quick text scan for `pytest_plugins` assignment.
                if let Ok(content) = std::fs::read_to_string(&child)
                    && content.contains("pytest_plugins")
                {
                    let rel = child
                        .strip_prefix(rootdir)
                        .unwrap_or(&child)
                        .components()
                        .map(|c| c.as_os_str().to_string_lossy())
                        .collect::<Vec<_>>()
                        .join(std::path::MAIN_SEPARATOR_STR);
                    errors.push((
                        child,
                        format!(
                            "Defining 'pytest_plugins' in a non-top-level conftest is \
                                 no longer supported: please remove it from {rel}"
                        ),
                    ));
                }
            }
        }
    }
    for start in start_dirs {
        walk(start, rootdir, skip_loaded, errors);
    }
}
