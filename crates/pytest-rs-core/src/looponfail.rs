//! Native `-f`/`--looponfail`, ported from pytest-xdist's `xdist/looponfail.py`:
//! re-run the test session, waiting for filesystem changes between runs.
//!
//! Each iteration re-execs this same binary as a fresh subprocess rather than
//! looping inside the long-lived controller process. This mirrors upstream's
//! own rationale (a fresh execnet gateway subprocess per run): edited
//! application code can only ever crash the disposable child, never the
//! controlling loop, and there's no risk of stale `sys.modules` entries
//! masking an edit the way an in-process rerun would have to guard against.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use crate::config::Config;
use crate::engine::center_with;
use crate::report::exit_code;

type Snapshot = HashMap<PathBuf, (SystemTime, u64)>;

/// Drive the looponfail control loop. Only returns on a fatal spawn error;
/// normal exit is via the process being killed (matching upstream, which
/// loops until SIGINT).
pub fn run(config: &Config, argv: &[String]) -> i32 {
    crate::tw::set_enabled(crate::tw::should_colorize(config.get_value("color")));
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from(&argv[0]));
    let child_args: Vec<String> = argv[1..]
        .iter()
        .filter(|a| a.as_str() != "-f" && a.as_str() != "--looponfail")
        .cloned()
        .collect();
    let watch_root = config.invocation_dir.clone();

    let mut snapshot = scan(&watch_root);
    let mut wasfailing = false;
    loop {
        let status = Command::new(&exe)
            .args(&child_args)
            .stdin(Stdio::null())
            .status();
        let failed = matches!(status, Ok(s) if s.code() == Some(exit_code::TESTS_FAILED));
        if !failed && wasfailing {
            // The previously-failing run now passes in full: rerun
            // immediately rather than waiting for another change.
            wasfailing = false;
            continue;
        }
        wasfailing = failed;
        if failed {
            println!(
                "{}",
                crate::tw::markup(&center_with("LOOPONFAILING", '#'), &[crate::tw::BOLD])
            );
        }
        println!(
            "{}",
            crate::tw::markup(&center_with("waiting for changes", '#'), &[crate::tw::BOLD])
        );
        println!("### Watching:   {}", watch_root.display());
        wait_for_change(&watch_root, &mut snapshot);
    }
}

/// Recursively stat every non-hidden, non-`.pyc` file under `root`.
fn scan(root: &Path) -> Snapshot {
    let mut map = HashMap::new();
    scan_into(root, &mut map);
    map
}

fn scan_into(dir: &Path, map: &mut Snapshot) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            scan_into(&path, map);
        } else if meta.is_file()
            && path.extension().is_none_or(|ext| ext != "pyc")
            && let Ok(modified) = meta.modified()
        {
            map.insert(path, (modified, meta.len()));
        }
    }
}

/// Poll every 2s (upstream's `StatRecorder.waitonchange` default) until a
/// file under `root` is added, removed, or its (mtime, size) changes.
fn wait_for_change(root: &Path, prev: &mut Snapshot) {
    loop {
        std::thread::sleep(Duration::from_secs(2));
        let cur = scan(root);
        if cur != *prev {
            *prev = cur;
            return;
        }
    }
}
