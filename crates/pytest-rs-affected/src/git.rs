//! The git fast path (docs/affected.md): git only tells which
//! files may have changed, so unchanged ones need not be read. Correctness
//! never depends on it: a file git cannot vouch for (untracked, ignored,
//! dirty when recorded, recorded at a commit that is not available, no git
//! at all) is hashed from disk as before.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Git {
    /// The directory git runs in (the rootdir); every path below is
    /// relative to it, `/`-separated.
    dir: PathBuf,
    head: String,
    tracked: HashSet<String>,
}

impl Git {
    /// The repository state around `dir`, or None without git, outside a
    /// repository, or before the first commit.
    pub fn open(dir: &Path) -> Option<Self> {
        let head = Self::run(dir, &["rev-parse", "--verify", "-q", "HEAD"])?
            .trim()
            .to_string();
        if head.is_empty() {
            return None;
        }
        let tracked = Self::paths(dir, &["ls-files", "-z"])?;
        Some(Self {
            dir: dir.to_path_buf(),
            head,
            tracked,
        })
    }

    pub fn head(&self) -> &str {
        &self.head
    }

    /// Tracked files under `dir` whose worktree content differs from
    /// `commit` (staged or not), or None when the commit is not available.
    pub fn changed_since(&self, commit: &str) -> Option<HashSet<String>> {
        Self::run(
            &self.dir,
            &["cat-file", "-e", &format!("{commit}^{{commit}}")],
        )?;
        Self::paths(
            &self.dir,
            &[
                "diff",
                "--name-only",
                "--relative",
                "--no-renames",
                "-z",
                commit,
            ],
        )
    }

    /// Files that are tracked and identical to HEAD right now: a state
    /// recorded from them is HEAD's content.
    pub fn clean_at_head(&self) -> Option<HashSet<String>> {
        let dirty = self.changed_since(&self.head)?;
        Some(
            self.tracked
                .iter()
                .filter(|path| !dirty.contains(*path))
                .cloned()
                .collect(),
        )
    }

    pub fn is_tracked(&self, path: &str) -> bool {
        self.tracked.contains(path)
    }

    fn run(dir: &Path, args: &[&str]) -> Option<String> {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn paths(dir: &Path, args: &[&str]) -> Option<HashSet<String>> {
        Some(
            Self::run(dir, args)?
                .split('\0')
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect(),
        )
    }
}

/// For the stored states that name a commit: which of their files git
/// vouches for as unchanged since then. Built once per selection; one
/// `git diff` per distinct commit.
pub struct Unchanged {
    by_commit: HashMap<String, Option<HashSet<String>>>,
}

impl Unchanged {
    pub fn new<'a>(git: Option<&Git>, commits: impl IntoIterator<Item = &'a str>) -> Self {
        let by_commit = match git {
            None => HashMap::new(),
            Some(git) => commits
                .into_iter()
                .collect::<HashSet<&str>>()
                .into_iter()
                .map(|commit| (commit.to_string(), git.changed_since(commit)))
                .collect(),
        };
        Self { by_commit }
    }

    /// Whether `path`, recorded clean at `commit`, is still the same: the
    /// commit is available, the file still tracked, and not changed since.
    pub fn contains(&self, git: Option<&Git>, commit: Option<&str>, path: &str) -> bool {
        let (Some(git), Some(commit)) = (git, commit) else {
            return false;
        };
        git.is_tracked(path)
            && self
                .by_commit
                .get(commit)
                .and_then(Option::as_ref)
                .is_some_and(|changed| !changed.contains(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn tracks_changes_since_a_commit() {
        let dir = std::env::temp_dir().join(format!("affected-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).expect("mkdir");
        assert!(Git::open(&dir).is_none());
        git(&dir, &["init", "-q"]);
        assert!(Git::open(&dir).is_none(), "no commit yet");
        std::fs::write(dir.join("a.py"), "x = 1\n").expect("write");
        std::fs::write(dir.join("sub/b.py"), "y = 1\n").expect("write");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "one"]);
        let repo = Git::open(&dir).expect("repo");
        let first = repo.head().to_string();
        assert_eq!(
            repo.clean_at_head().expect("clean"),
            HashSet::from(["a.py".to_string(), "sub/b.py".to_string()])
        );

        std::fs::write(dir.join("a.py"), "x = 2\n").expect("write");
        std::fs::write(dir.join("new.txt"), "n").expect("write");
        let repo = Git::open(&dir).expect("repo");
        assert_eq!(
            repo.clean_at_head().expect("clean"),
            HashSet::from(["sub/b.py".to_string()])
        );
        let unchanged = Unchanged::new(Some(&repo), [first.as_str(), "0000000"]);
        assert!(unchanged.contains(Some(&repo), Some(&first), "sub/b.py"));
        assert!(!unchanged.contains(Some(&repo), Some(&first), "a.py"));
        assert!(
            !unchanged.contains(Some(&repo), Some(&first), "new.txt"),
            "untracked"
        );
        assert!(
            !unchanged.contains(Some(&repo), Some("0000000"), "sub/b.py"),
            "unknown commit"
        );
        assert!(
            !unchanged.contains(None, Some(&first), "sub/b.py"),
            "no git"
        );

        // Paths are relative to the directory git runs in.
        let sub = Git::open(&dir.join("sub")).expect("repo");
        assert!(sub.is_tracked("b.py"));
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
