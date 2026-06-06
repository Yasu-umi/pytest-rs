//! `pytest-rs` console launcher.
//!
//! The engine binary (`pytest-rs-bin`) links libpython dynamically, so a
//! prebuilt wheel cannot know where the user's interpreter lives. This
//! launcher (no Python linkage — always starts) asks the environment's
//! python for its lib directory, puts it on the loader path, and execs the
//! engine. The dynamic loader resolves libpython by leaf name from
//! (DY)LD_LIBRARY_PATH even when the engine's recorded path doesn't exist.

use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(target_os = "macos")]
const LOADER_PATH: &str = "DYLD_LIBRARY_PATH";
#[cfg(not(target_os = "macos"))]
const LOADER_PATH: &str = "LD_LIBRARY_PATH";

/// LIBDIR of the python sitting next to this launcher (the venv the wheel
/// was installed into). None when there is no python sibling (running from
/// a cargo target dir, where the engine's recorded libpython path works).
fn libpython_dir(bindir: &Path) -> Option<String> {
    let python = ["python3", "python"]
        .iter()
        .map(|name| bindir.join(name))
        .find(|path| path.exists())?;
    let output = Command::new(python)
        .args([
            "-c",
            "import sysconfig; print(sysconfig.get_config_var('LIBDIR') or '')",
        ])
        .output()
        .ok()?;
    let dir = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!dir.is_empty()).then_some(dir)
}

fn main() {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("pytest-rs"));
    let bindir = exe.parent().map(Path::to_path_buf).unwrap_or_default();
    let engine = bindir.join("pytest-rs-bin");
    let mut command = Command::new(&engine);
    command.args(std::env::args_os().skip(1));
    if let Some(dir) = libpython_dir(&bindir) {
        let existing = std::env::var(LOADER_PATH).unwrap_or_default();
        let value = if existing.is_empty() {
            dir
        } else {
            format!("{dir}:{existing}")
        };
        command.env(LOADER_PATH, value);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = command.exec();
        eprintln!("error: failed to launch {}: {err}", engine.display());
        std::process::exit(1);
    }
    #[cfg(not(unix))]
    {
        eprintln!("pytest-rs only supports unix platforms");
        std::process::exit(1);
    }
}
