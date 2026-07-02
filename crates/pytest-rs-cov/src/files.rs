//! coverage.py `[paths]` aliasing: a faithful port of `coverage/files.py`'s
//! `_glob_to_regex` + `globs_to_regex` + `PathAliases` (add/map).
//!
//! Two uses:
//!   - tracked judgment (native `LineCollector` + subprocess `_child.py`):
//!     a file whose path prefix-matches any alias is measured, so a file
//!     rsync'd to a worker dir (`/tmp/.../dir1/x.py`) is traced even though
//!     `--cov` pointed at the original tree.
//!   - report-time remap: the alias prefix is rewritten to the canonical
//!     path so the report collapses worker-dir and canonical rows.

use std::path::Path;

use regex::{Regex, RegexBuilder};

/// One compiled rule from coverage.py's `G2RX_TOKENS`. `sub == None` means
/// the matched text is disallowed (`***`, `x**`, standalone `[`/`]`); we then
/// fall through to a narrower rule rather than erroring.
struct GlobRule {
    re: Regex,
    sub: Option<&'static str>,
}

fn glob_rules() -> &'static [GlobRule] {
    static RULES: std::sync::OnceLock<Vec<GlobRule>> = std::sync::OnceLock::new();
    RULES.get_or_init(|| {
        let make = |rx: &'static str, sub: Option<&'static str>| GlobRule {
            re: Regex::new(rx).expect("hardcoded glob rule regex"),
            sub,
        };
        vec![
            make(r"\*\*\*+", None),
            make(r"[^/]+\*\*+", None),
            make(r"\*\*+[^/]+", None),
            make(r"\*\*/\*\*", None),
            // ^*/ matches any prefix-with-separator, or nothing.
            make(r"^\*+/", Some(r"(.*[/\\])?")),
            // /*$ matches any separator-then-anything at the end.
            make(r"/\*+$", Some(r"[/\\].*")),
            // **/ matches any subdirs, including none.
            make(r"\*\*/", Some(r"(.*[/\\])?")),
            make(r"/", Some(r"[/\\]")),
            make(r"\*", Some(r"[^/\\]*")),
            make(r"\?", Some(r"[^/\\]")),
            // [a-f] char classes pass through verbatim.
            make(r"\[.*?\]", Some(r"\g<0>")),
            // word characters pass through verbatim.
            make(r"[a-zA-Z0-9_-]+", Some(r"\g<0>")),
            make(r"[\[\]]", None),
            // Anything else is escaped.
            make(r".", Some(r"\\\g<0>")),
        ]
    })
}

/// Apply a `G2RX_TOKENS` substitution. Unlike Python's `re.Match.expand`,
/// only `\g<0>` (whole match) is expanded; every other byte is emitted as-is
/// so the substitution strings are already-valid regex fragments (e.g.
/// `[/\\]` stays `[/\\]`, matching `/` or `\`).
fn apply_sub(sub: &str, matched: &str) -> String {
    let mut out = String::new();
    let bytes = sub.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if sub[i..].starts_with(r"\g<0>") {
            out.push_str(matched);
            i += r"\g<0>".len();
        } else {
            let ch = sub[i..].chars().next().expect("non-empty");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Port of coverage.py `_glob_to_regex`: turn a single glob into an unanchored
/// regex string. Backslashes are folded to `/`; a pattern with no separator is
/// treated as `**/<pattern>` (matches a filename anywhere).
fn glob_to_regex(pattern: &str) -> String {
    let pattern = pattern.replace('\\', "/");
    let pattern = if !pattern.contains('/') {
        format!("**/{pattern}")
    } else {
        pattern
    };
    let rules = glob_rules();
    let mut out = String::new();
    let mut rest = pattern.as_str();
    while !rest.is_empty() {
        let mut advanced = false;
        for rule in rules {
            // A rule whose `sub` is None marks a disallowed token; skip it so
            // the loop falls through to a narrower rule (e.g. `***` is
            // consumed one `*` at a time by the `\*` rule) rather than
            // raising like coverage.py.
            if let Some(m) = rule.re.find(rest)
                && m.start() == 0
                && let Some(sub) = rule.sub
            {
                out.push_str(&apply_sub(sub, m.as_str()));
                rest = &rest[m.end()..];
                advanced = true;
                break;
            }
        }
        if !advanced {
            // No rule matched this position (shouldn't happen: the `.` rule
            // matches any single byte start). Escape and advance to be safe.
            let ch = rest.chars().next().expect("non-empty rest");
            out.push_str(&regex::escape(&ch.to_string()));
            rest = &rest[ch.len_utf8()..];
        }
    }
    out
}

/// Compile a glob to an anchored, case-insensitive prefix regex (coverage.py
/// `globs_to_regex([pat], case_insensitive=True, partial=True)` plus a leading
/// `^` so Rust's `find` acts like Python's `re.match`).
fn compile_alias(pattern: &str) -> Option<Regex> {
    let rx = format!("^{}", glob_to_regex(pattern));
    RegexBuilder::new(&rx).case_insensitive(true).build().ok()
}

/// A compiled `[paths]` alias: paths matching `regex` (a prefix) report as
/// `result` (the canonical path, with a trailing separator).
struct AliasRule {
    regex: Regex,
    result: String,
}

/// coverage.py `PathAliases`, restricted to the combine/remap and trace-accept
/// operations pytest-rs needs. Built from `[paths]` groups (canonical first).
#[derive(Default)]
pub struct PathAliases {
    rules: Vec<AliasRule>,
}

impl PathAliases {
    /// Build from coverage `[paths]` groups; each group is
    /// `[canonical, alias1, alias2, ...]`. A pattern ending in a wildcard
    /// component is skipped (coverage.py rejects it; we ignore it).
    ///
    /// `base` is the directory relative patterns are resolved against
    /// (coverage.py's `abs_file`, which uses the process cwd): a pattern with
    /// no leading wildcard and no absolute prefix — e.g. a literal `aliased`
    /// rather than a glob like `*/dir1` — only matches an absolute traced
    /// filename once it too is made absolute against `base`.
    pub fn from_groups(groups: &[Vec<String>], base: &Path) -> Self {
        let mut rules = Vec::new();
        for group in groups {
            if group.len() < 2 {
                continue;
            }
            let canonical = Self::resolve(&group[0], base)
                .trim_end_matches(['/', '\\'])
                .to_string()
                + "/";
            for raw in &group[1..] {
                let pat = raw.trim_end_matches(['/', '\\']);
                if pat.is_empty() || pat.ends_with('*') {
                    continue;
                }
                let pat = if pat.starts_with('*') {
                    pat.to_string()
                } else {
                    Self::resolve(pat, base)
                };
                // coverage.py forces a trailing separator so an alias matches
                // a directory root, not a mid-filename prefix.
                if let Some(regex) = compile_alias(&format!("{pat}/")) {
                    rules.push(AliasRule {
                        regex,
                        result: canonical.clone(),
                    });
                }
            }
        }
        Self { rules }
    }

    /// coverage.py `abs_file`: a pattern with no leading wildcard is resolved
    /// against `base` unless it is already absolute.
    fn resolve(pattern: &str, base: &Path) -> String {
        if Path::new(pattern).is_absolute() {
            pattern.to_string()
        } else {
            base.join(pattern).to_string_lossy().to_string()
        }
    }

    /// Trace-accept check: does `filename` start with any alias pattern?
    pub fn matches(&self, filename: &str) -> bool {
        self.rules.iter().any(|rule| rule.regex.is_match(filename))
    }

    /// Report remap: the first alias whose prefix matches rewrites the matched
    /// prefix to its canonical result. The canonical path must exist on disk
    /// (coverage.py skips a rule whose result doesn't exist). Returns `None`
    /// if no rule applies.
    pub fn map(&self, path: &str) -> Option<String> {
        for rule in &self.rules {
            if let Some(m) = rule.regex.find(path)
                && m.start() == 0
            {
                let new = format!("{}{}", rule.result, &path[m.end()..]);
                if Path::new(&new).exists() {
                    return Some(new);
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn star_slash_alias_matches_worker_dir() {
        // `*/dir1` (with the forced trailing sep) must match an absolute path
        // nested under any .../dir1/ tree — the whole point of the port.
        let aliases = PathAliases::from_groups(
            &[vec![
                "/orig/src".to_string(),
                "*/dir1".to_string(),
                "*/dir2".to_string(),
            ]],
            &std::env::temp_dir(),
        );
        assert!(aliases.matches("/tmp/whatever/dir1/child_script.py"));
        assert!(aliases.matches("/tmp/whatever/dir2/child_script.py"));
        assert!(!aliases.matches("/tmp/whatever/dir3/child_script.py"));
    }

    #[test]
    fn map_rewrites_to_canonical() {
        // map() needs the canonical result to exist; use a temp dir.
        let tmp = std::env::temp_dir();
        let canonical = tmp.to_string_lossy().trim_end_matches('/').to_string();
        let aliases = PathAliases::from_groups(
            &[vec![canonical.clone(), "*/cov_alias_test".to_string()]],
            &tmp,
        );
        std::fs::write(std::path::Path::new(&canonical).join("x.py"), "x\n").unwrap();
        let mapped = aliases
            .map("/tmp/whatever/cov_alias_test/x.py")
            .expect("alias maps");
        assert!(mapped.ends_with("/x.py"));
        assert!(mapped.contains(&canonical));
    }

    #[test]
    fn literal_alias_resolves_against_base() {
        // A non-wildcard alias (e.g. `[coverage:paths] source = src \n
        // aliased`, no glob) only matches an absolute traced filename once
        // it too is resolved against `base` (coverage.py's `abs_file`,
        // which uses the process cwd) — it has no leading `*/` to skip an
        // arbitrary prefix like `*/dir1` does.
        let base = std::env::temp_dir().join("cov_literal_alias_test");
        std::fs::create_dir_all(base.join("src")).unwrap();
        std::fs::write(base.join("src").join("mod.py"), "x\n").unwrap();
        let aliases =
            PathAliases::from_groups(&[vec!["src".to_string(), "aliased".to_string()]], &base);
        let absolute = base.join("aliased").join("mod.py");
        assert!(aliases.matches(&absolute.to_string_lossy()));
        let mapped = aliases
            .map(&absolute.to_string_lossy())
            .expect("alias maps");
        assert_eq!(mapped, base.join("src").join("mod.py").to_string_lossy());
    }
}
