//! `[tool.pytest-rs.affected]` in the rootdir's pyproject.toml
//! (docs/affected.md). Kept out of `[tool.pytest.ini_options]`:
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

/// What a change under a `file_triggers` key runs.
#[derive(Debug, Clone)]
pub enum Trigger {
    /// The tests matching these globs (by test file path).
    Tests(Globs),
    /// Trust the recorded dependencies (nothing extra).
    Recorded,
    /// Everything.
    All,
}

/// `incomplete`: what to do with a test whose dependencies are incomplete
/// (it, or a context it depends on, started a subprocess).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Incomplete {
    Run,
    Trust,
}

#[derive(Debug, Clone)]
pub struct AffectedConfig {
    pub full_run_on: Globs,
    /// A changed non-`.py` file that is neither a recorded dependency nor
    /// matched by any rule: ignored, or a full run.
    pub unknown_files: UnknownFiles,
    pub always_changed: Globs,
    pub always_run: Globs,
    pub ignore: Globs,
    /// Every key matching a changed path applies: `all` wins, test globs
    /// add up (TOML tables carry no reliable key order, so the result must
    /// not depend on it).
    pub file_triggers: Vec<(Globs, Trigger)>,
    pub incomplete: Incomplete,
}

impl Default for AffectedConfig {
    fn default() -> Self {
        Self {
            full_run_on: Globs::builtin(&[]),
            unknown_files: UnknownFiles::Ignore,
            always_changed: Globs::builtin(&[]),
            always_run: Globs::builtin(&[]),
            ignore: Globs::builtin(&[]),
            file_triggers: Vec::new(),
            incomplete: Incomplete::Run,
        }
    }
}

impl AffectedConfig {
    /// Every `file_triggers` entry matching `rel`.
    pub fn triggers_for(&self, rel: &str) -> Vec<&Trigger> {
        self.file_triggers
            .iter()
            .filter(|(key, _)| key.matches(rel))
            .map(|(_, trigger)| trigger)
            .collect()
    }

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
            "file_triggers",
            "incomplete",
        ];
        if let Some(unknown) = table.keys().find(|k| !known.contains(&k.as_str())) {
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
        let incomplete = match table.get("incomplete").map(|v| v.as_str()) {
            None | Some(Some("run")) => Incomplete::Run,
            Some(Some("trust")) => Incomplete::Trust,
            _ => {
                return Err(
                    "[tool.pytest-rs.affected] incomplete must be \"run\" or \"trust\"".into(),
                );
            }
        };
        let file_triggers = match table.get("file_triggers") {
            None => Vec::new(),
            Some(value) => {
                let triggers = value
                    .as_table()
                    .ok_or("[tool.pytest-rs.affected] file_triggers must be a table")?;
                triggers
                    .iter()
                    .map(|(key, value)| {
                        let glob = Globs::new(std::slice::from_ref(key))?;
                        let trigger = match value {
                            toml::Value::String(s) if s == "recorded" => Trigger::Recorded,
                            toml::Value::String(s) if s == "all" => Trigger::All,
                            toml::Value::Array(items) => {
                                let patterns: Vec<String> = items
                                    .iter()
                                    .map(|item| item.as_str().map(str::to_string))
                                    .collect::<Option<_>>()
                                    .ok_or(format!(
                                        "[tool.pytest-rs.affected.file_triggers] {key:?}: \
                                         a list must hold test globs"
                                    ))?;
                                Trigger::Tests(Globs::new(&patterns)?)
                            }
                            _ => {
                                return Err(format!(
                                    "[tool.pytest-rs.affected.file_triggers] {key:?} must be \
                                     a list of test globs, \"recorded\" or \"all\""
                                ));
                            }
                        };
                        Ok((glob, trigger))
                    })
                    .collect::<Result<Vec<_>, String>>()?
            }
        };
        Ok(Self {
            full_run_on: list("full_run_on")?,
            unknown_files,
            always_changed: list("always_changed")?,
            always_run: list("always_run")?,
            ignore: list("ignore")?,
            file_triggers,
            incomplete,
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

incomplete = "trust"

[tool.pytest-rs.affected.file_triggers]
"migrations/**" = ["tests/db/**"]
"data/**" = "recorded"
"assets/**" = "all"
"#,
        )
        .expect("parses");
        assert_eq!(config.incomplete, Incomplete::Trust);
        assert!(matches!(
            config.triggers_for("migrations/0001.py")[..],
            [Trigger::Tests(globs)] if globs.matches("tests/db/test_x.py")
        ));
        assert!(matches!(
            config.triggers_for("data/a.json")[..],
            [Trigger::Recorded]
        ));
        assert!(matches!(
            config.triggers_for("assets/a.png")[..],
            [Trigger::All]
        ));
        assert!(config.triggers_for("other.txt").is_empty());
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
        assert!(AffectedConfig::parse("[tool.pytest-rs.affected]\nincomplete = \"x\"\n").is_err());
        assert!(
            AffectedConfig::parse("[tool.pytest-rs.affected.file_triggers]\n\"a/**\" = 1\n")
                .is_err()
        );
    }
}
