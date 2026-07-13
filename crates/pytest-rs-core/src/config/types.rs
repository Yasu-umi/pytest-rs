use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressKind {
    /// "[ 50%]" (the "progress" style, default).
    Percent,
    /// "[10/20]" (the "count" style).
    Count,
    /// A per-file node duration (the "times" style).
    Times,
    /// No field at all ("classic", capture-off, or unknown styles).
    Hidden,
}

/// Frozen CLI + ini configuration, immutable after parsing.
#[derive(Debug)]
pub struct Config {
    pub paths: Vec<String>,
    pub verbose: u8,
    pub quiet: bool,
    /// -q occurrences (-qq folds collect-only output per file).
    pub quiet_level: u8,
    pub exitfirst: bool,
    pub collect_only: bool,
    pub rootdir: PathBuf,
    /// The directory the runner was invoked from: relative CLI paths (and
    /// bare collection) resolve against this, not rootdir.
    pub invocation_dir: PathBuf,
    /// -W warning filter specs, applied at session start.
    pub w_options: Vec<String>,
    /// -p specs (e.g. `no:terminal` disables all terminal output).
    pub plugin_opts: Vec<String>,
    pub(crate) flags: HashSet<String>,
    pub(crate) values: HashMap<String, String>,
    /// -o name=value overrides; take precedence over file values.
    pub(crate) ini_overrides: HashMap<String, String>,
    /// Values from pytest.ini / pyproject.toml / tox.ini / setup.cfg.
    pub(crate) ini_file: HashMap<String, String>,
    /// Original TOML type tags for TOML-sourced ini values (key →
    /// "string"/"int"/"float"/"bool"/"array"). Empty for non-TOML sources.
    pub(crate) toml_types: HashMap<String, String>,
    /// The config file's basename, for the "configfile:" header line.
    pub config_file_name: Option<String>,
    /// Lower-priority config files in the rootdir that also hold pytest config
    /// but were ignored — pytest appends a "(WARNING: ignoring ...)" note.
    pub ignored_config_files: Vec<String>,
    /// The full argument list after addopts splicing — for plugins with
    /// position-sensitive options (pytest-cov's --cov-reset / --no-cov).
    pub effective_args: Vec<String>,
    /// Unknown `--flag[=value]` tokens deferred for python-plugin option
    /// specs (pytest_addoption): applied after plugin load, where clap
    /// cannot know them. Space-separated values are not supported.
    pub plugin_args: Vec<String>,
    /// The verbatim args passed to `pytest.main()`/the CLI, before addopts
    /// splicing — for `config.invocation_params.args`.
    pub invocation_args: Vec<String>,
    /// A plugin replaced the 'terminalreporter' plugin during configure
    /// (pytest-sugar/pytest-pretty): native terminal output is suppressed
    /// and the engine drives the replacement object instead. Set once,
    /// after python pytest_configure hooks fire.
    pub(crate) reporter_delegated: std::sync::atomic::AtomicBool,
    /// `-h`/`--help`'s pre-rendered, argparse-styled help text (`Some` only
    /// when the flag was given). Unlike `--version`, `--help` can't be
    /// resolved before conftest/plugin option registration — a plugin's
    /// addoption might itself be malformed (upstream still shows help, in
    /// a reduced "minimal help" form, when that happens) — so printing is
    /// deferred to after `load_and_validate_config` succeeds or fails.
    pub help_text: Option<String>,
}

/// pytest's rootdir-discovery inputs: explicit filesystem path args, falling
/// back to cwd when no path args exist (mirrors pytest's get_dirs_from_args).
pub(crate) fn dirs_from_args(cwd: &Path, argv: &[String]) -> Vec<PathBuf> {
    let mut dirs = vec![];
    for arg in argv.iter().skip(1) {
        if arg.starts_with('-') {
            continue;
        }
        let candidate = arg.split("::").next().unwrap_or(arg);
        let path = Path::new(candidate);
        if !path.exists() {
            continue;
        }
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        dirs.push(if path.is_file() {
            path.parent().map(Path::to_path_buf).unwrap_or(path)
        } else {
            path
        });
    }
    if dirs.is_empty() {
        dirs.push(cwd.to_path_buf());
    }
    dirs
}

pub(crate) fn common_ancestor(dirs: &[PathBuf]) -> PathBuf {
    let mut ancestor = dirs[0].clone();
    for dir in &dirs[1..] {
        while !dir.starts_with(&ancestor) {
            let Some(parent) = ancestor.parent() else {
                break;
            };
            ancestor = parent.to_path_buf();
        }
    }
    ancestor
}

/// Python shlex.split (posix): whitespace-separated tokens with '...'
/// (literal) and "..." (backslash escapes \\ and \") quoting.
/// A token is a positional test-path arg if it looks like a filesystem path
/// (contains `/`, `\`, or a Python-file extension). Used to avoid greedily
/// consuming test-path positionals as values for deferred plugin flags.
pub(crate) fn looks_like_path(s: &str) -> bool {
    s.contains('/')
        || s.contains('\\')
        || s.ends_with(".py")
        || s.ends_with(".txt")
        || s.ends_with(".toml")
        || s.ends_with(".cfg")
        || s.ends_with(".ini")
        || s.starts_with('.') // ./relative or ../up
}

/// A token that is a negative number (e.g. `-1`, `-2.5`), not a flag — so it
/// can be consumed as a deferred plugin option's value (`--reruns-delay -1`)
/// rather than mistaken for an option and rejected by clap.
pub(crate) fn is_negative_number(s: &str) -> bool {
    s.strip_prefix('-').is_some_and(|rest| {
        !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit() || c == '.')
    })
}

pub(crate) fn shlex_split(input: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if in_word {
                    parts.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                for inner in chars.by_ref() {
                    if inner == '\'' {
                        break;
                    }
                    current.push(inner);
                }
            }
            '"' => {
                in_word = true;
                while let Some(inner) = chars.next() {
                    match inner {
                        '"' => break,
                        '\\' => match chars.next() {
                            Some(esc @ ('"' | '\\')) => current.push(esc),
                            Some(other) => {
                                current.push('\\');
                                current.push(other);
                            }
                            None => current.push('\\'),
                        },
                        other => current.push(other),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(esc) = chars.next() {
                    current.push(esc);
                }
            }
            other => {
                in_word = true;
                current.push(other);
            }
        }
    }
    if in_word {
        parts.push(current);
    }
    parts
}
