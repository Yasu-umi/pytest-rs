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
    /// `pytest_collection_modifyitems` hookwrapper impls registered directly
    /// on a plugin instance (e.g. pytest-order's `--order-after-ff`
    /// OrderingPlugin) rather than as a conftest function. Deferred out of
    /// `run_py_modifyitems` so they observe the item list *after* the native
    /// --lf/--ff/--nf/--sw reorder (`cache.modify_items`), mirroring
    /// upstream's true pluggy hookwrapper nesting where LFPlugin (itself a
    /// tryfirst hookwrapper whose reorder is entirely post-yield) settles
    /// first and an outer wrapper's post-yield mutation applies on top. See
    /// `fire_deferred_modifyitems_wrappers` in `engine/hooks.rs`.
    pending_modifyitems_wrapper_hooks: Vec<Py<PyAny>>,
    /// The testrun_uid for this dist run, generated at the
    /// `collect_pre_configure` checkpoint (before any fork) and consumed by
    /// `run_dist`. `None` when not distributing.
    #[cfg(feature = "xdist")]
    pub(crate) dist_testrun_uid: Option<String>,
    /// Workers already forked at the `collect_pre_configure` checkpoint (or
    /// left empty when spawning), consumed by `run_dist`.
    #[cfg(feature = "xdist")]
    pub(crate) forked_workers: Vec<Option<crate::dist::WorkerProc>>,
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

/// `--debug`: pytest's debug trace file (minimal: create the file and
/// announce it on stderr like upstream). The "wrote" message fires on drop
/// so every exit path (early returns, NO_TESTS_COLLECTED, etc.) emits it.
/// Shared by both the top-level run and a nested run (`pytester.runpytest`),
/// so `--debug` works the same in either.
pub(crate) struct DebugGuard(Option<std::path::PathBuf>);

impl Drop for DebugGuard {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            eprintln!("wrote pytest debug information to {}", path.display());
        }
    }
}

pub(crate) fn install_debug_guard(py: Python<'_>, config: &Config) -> DebugGuard {
    let Some(name) = config.get_value("debug") else {
        return DebugGuard(None);
    };
    let path = config.invocation_dir.join(name);
    let _ = std::fs::write(
        &path,
        format!(
            "versions pytest-rs-{}, python-{}\n\
             pytest_configure\n\
             pytest_sessionstart\n",
            env!("CARGO_PKG_VERSION"),
            py.version().split_whitespace().next().unwrap_or("")
        ),
    );
    eprintln!("writing pytest debug information to {}", path.display());
    DebugGuard(Some(path))
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

/// Runs `f` (a `pytest_unconfigure`/`pytest_sessionfinish` hook dispatch)
/// with capture resumed around it, then flushes any output the hooks
/// printed to the real streams. These dispatch points sit outside
/// `_capture`'s active periods (only `start_phase("setup")` and
/// `collect_begin`/`collect_end` resume it) — without this, a conftest's
/// `pytest_unconfigure`/`pytest_sessionfinish` print() leaks into whichever
/// capture object happens to be ambient (e.g. an outer nested run's own
/// capture) instead of this run's own stdout/stderr.
pub(crate) fn flush_hook_output<T>(py: Python<'_>, f: impl FnOnce() -> T) -> T {
    crate::python::capture_collect_begin(py);
    let result = f();
    for (title, text) in crate::python::capture_collect_end(py) {
        if title == "Captured stderr" {
            eprint!("{text}");
        } else {
            print!("{text}");
        }
    }
    result
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
