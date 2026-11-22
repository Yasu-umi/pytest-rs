//! `[tool.pytest-rs.affected]` in the rootdir's pyproject.toml
//! (docs/AFFECTED_TESTS.md §6). Kept out of `[tool.pytest.ini_options]`:
//! real pytest warns on unknown ini keys.

use std::path::Path;

use glob::{MatchOptions, Pattern};

/// Files that influence tests without being read through Python's `open`
/// (config read by the Rust loader, environment definitions, native code):
/// a change to any of them means a full run.
pub const BUILTIN_FULL_RUN_ON: &[&str] = &[
    "**/pytest.ini",
    "**/pyproject.toml",
    "**/tox.ini",
    "**/setup.cfg",
    "**/uv.lock",
    "**/poetry.lock",
    "**/Pipfile.lock",
    "**/requirements*.txt",
    "**/constraints*.txt",
    "**/setup.py",
    "**/*.so",
    "**/*.pyd",
    "**/*.dylib",
    "**/*.pyx",
    "**/*.pxd",
    "**/*.c",
    "**/*.cc",
    "**/*.cpp",
    "**/*.h",
    "**/*.rs",
    "**/*.pth",
];

/// Files never taken into the non-`.py` snapshot: outputs that pytest-rs
/// and its bundled plugins write into the rootdir on every run, and OS
/// metadata. Without these a run would trigger a full run the next time.
pub const BUILTIN_IGNORE: &[&str] = &[
    "**/.coverage",
    "**/.coverage.*",
    "**/.test_durations",
    "**/.DS_Store",
    "**/*.pyc",
    "**/*.pyo",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownFiles {
    Ignore,
    All,
}

#[derive(Debug, Clone)]
pub struct Globs {
    patterns: Vec<Pattern>,
}

impl Globs {
    const OPTIONS: MatchOptions = MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: false,
    };

    pub fn new(patterns: &[String]) -> Result<Self, String> {
        patterns
            .iter()
            .map(|p| Pattern::new(p).map_err(|e| format!("invalid glob {p:?}: {e}")))
            .collect::<Result<Vec<_>, _>>()
            .map(|patterns| Self { patterns })
    }

    pub fn builtin(patterns: &[&str]) -> Self {
        Self {
            patterns: patterns
                .iter()
                .map(|p| Pattern::new(p).expect("builtin glob is valid"))
                .collect(),
        }
    }

    /// Match a rootdir-relative, `/`-separated path. A leading `**/` also
    /// matches at the top level.
    pub fn matches(&self, rel: &str) -> bool {
        self.patterns.iter().any(|pattern| {
            pattern.matches_with(rel, Self::OPTIONS)
                || pattern
                    .as_str()
                    .strip_prefix("**/")
                    .and_then(|rest| Pattern::new(rest).ok())
                    .is_some_and(|top| top.matches_with(rel, Self::OPTIONS))
        })
    }

    /// Whether a whole directory is covered (a pattern `dir/**` or
    /// `**/dir/**` that matches the directory itself), so a walk can skip it.
    pub fn covers_dir(&self, rel_dir: &str) -> bool {
        self.patterns.iter().any(|pattern| {
            pattern
                .as_str()
                .strip_suffix("/**")
                .is_some_and(|dir| Self::new(&[dir.to_string()]).is_ok_and(|g| g.matches(rel_dir)))
        })
    }
}

#[derive(Debug, Clone)]
pub struct AffectedConfig {
    pub full_run_on: Globs,
    /// Parsed and validated, but not acted on yet: without recorded file
    /// dependencies (the audit hook, PR 2) every unrecorded non-`.py`
    /// change runs everything, as with `"all"`.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "takes effect with recorded file dependencies (PR 2)"
        )
    )]
    pub unknown_files: UnknownFiles,
    pub always_changed: Globs,
    pub always_run: Globs,
    pub ignore: Globs,
}

impl Default for AffectedConfig {
    fn default() -> Self {
        Self {
            full_run_on: Globs::builtin(&[]),
            unknown_files: UnknownFiles::Ignore,
            always_changed: Globs::builtin(&[]),
            always_run: Globs::builtin(&[]),
            ignore: Globs::builtin(&[]),
        }
    }
}

impl AffectedConfig {
    /// Keys accepted for forward compatibility but not acted on yet
    /// (§10: file_triggers and incomplete arrive in PR 2). Ignoring them
    /// can only make a run larger in PR 1, where any non-`.py` change is a
    /// full run and subprocesses are not tracked.
    const LATER_KEYS: &[&str] = &["file_triggers", "incomplete"];

    pub fn load(rootdir: &Path) -> Result<Self, String> {
        let path = rootdir.join("pyproject.toml");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Ok(Self::default());
        };
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let doc: toml::Table = text.parse().map_err(|e| format!("pyproject.toml: {e}"))?;
        let Some(table) = doc
            .get("tool")
            .and_then(|t| t.get("pytest-rs"))
            .and_then(|t| t.get("affected"))
        else {
            return Ok(Self::default());
        };
        let table = table
            .as_table()
            .ok_or("[tool.pytest-rs.affected] must be a table")?;
        let known = [
            "full_run_on",
            "unknown_files",
            "always_changed",
            "always_run",
            "ignore",
        ];
        if let Some(unknown) = table
            .keys()
            .find(|k| !known.contains(&k.as_str()) && !Self::LATER_KEYS.contains(&k.as_str()))
        {
            return Err(format!(
                "[tool.pytest-rs.affected]: unknown key {unknown:?}"
            ));
        }
        let list = |key: &str| -> Result<Globs, String> {
            match table.get(key) {
                None => Ok(Globs::builtin(&[])),
                Some(value) => {
                    let items = value
                        .as_array()
                        .ok_or(format!("[tool.pytest-rs.affected] {key} must be a list"))?;
                    let patterns: Vec<String> = items
                        .iter()
                        .map(|item| {
                            item.as_str().map(str::to_string).ok_or(format!(
                                "[tool.pytest-rs.affected] {key} must be a list of strings"
                            ))
                        })
                        .collect::<Result<_, _>>()?;
                    Globs::new(&patterns)
                }
            }
        };
        let unknown_files = match table.get("unknown_files").map(|v| v.as_str()) {
            None | Some(Some("ignore")) => UnknownFiles::Ignore,
            Some(Some("all")) => UnknownFiles::All,
            _ => {
                return Err(
                    "[tool.pytest-rs.affected] unknown_files must be \"ignore\" or \"all\"".into(),
                );
            }
        };
        Ok(Self {
            full_run_on: list("full_run_on")?,
            unknown_files,
            always_changed: list("always_changed")?,
            always_run: list("always_run")?,
            ignore: list("ignore")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_match_relative_paths() {
        let globs =
            Globs::new(&["docs/**".into(), "**/*.md".into(), "data/*.json".into()]).expect("valid");
        assert!(globs.matches("docs/index.rst"));
        assert!(globs.matches("docs/a/b.txt"));
        assert!(globs.matches("README.md"));
        assert!(globs.matches("pkg/notes.md"));
        assert!(globs.matches("data/x.json"));
        assert!(!globs.matches("data/sub/x.json"));
        assert!(!globs.matches("src/app.py"));
        assert!(globs.covers_dir("docs"));
        assert!(!globs.covers_dir("data"));
    }

    #[test]
    fn builtin_lists_match_top_level_and_nested() {
        let full = Globs::builtin(BUILTIN_FULL_RUN_ON);
        assert!(full.matches("pyproject.toml"));
        assert!(full.matches("sub/pyproject.toml"));
        assert!(full.matches("requirements-dev.txt"));
        assert!(full.matches("setup.py"));
        assert!(full.matches("ext/fast.c"));
        assert!(!full.matches("notes.txt"));
        let ignore = Globs::builtin(BUILTIN_IGNORE);
        assert!(ignore.matches(".coverage"));
        assert!(ignore.matches(".coverage.host.123"));
        assert!(ignore.matches("pkg/.DS_Store"));
    }

    #[test]
    fn parse_table() {
        let config = AffectedConfig::parse(
            r#"
[tool.pytest-rs.affected]
full_run_on = ["docker/**"]
unknown_files = "all"
always_changed = ["src/settings.py"]
always_run = ["tests/smoke/**"]
ignore = ["docs/**"]

[tool.pytest-rs.affected.file_triggers]
"data/**" = "recorded"
"#,
        )
        .expect("parses");
        assert!(config.full_run_on.matches("docker/Dockerfile"));
        assert_eq!(config.unknown_files, UnknownFiles::All);
        assert!(config.always_changed.matches("src/settings.py"));
        assert!(config.always_run.matches("tests/smoke/test_a.py"));
        assert!(config.ignore.matches("docs/a.md"));
    }

    #[test]
    fn missing_table_is_default() {
        let config = AffectedConfig::parse("[tool.other]\nx = 1\n").expect("parses");
        assert_eq!(config.unknown_files, UnknownFiles::Ignore);
        assert!(!config.ignore.matches("anything"));
    }

    #[test]
    fn invalid_values_are_errors() {
        assert!(AffectedConfig::parse("[tool.pytest-rs.affected]\ntypo = []\n").is_err());
        assert!(AffectedConfig::parse("[tool.pytest-rs.affected]\nignore = \"docs\"\n").is_err());
        assert!(
            AffectedConfig::parse("[tool.pytest-rs.affected]\nunknown_files = \"some\"\n").is_err()
        );
    }
}
