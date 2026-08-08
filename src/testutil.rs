//! Test-only helpers: real git fixtures in an isolated temp directory.
//!
//! Fixture repos set user.name/user.email locally and neutralize global git
//! config for every fixture-driven git call, so tests do not depend on the
//! machine's git configuration.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

static SEQ: AtomicU32 = AtomicU32::new(0);

/// Unique temp directory, removed on drop (best effort).
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new() -> TempDir {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!("wtree-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A real git repo (`repo/`, branch `main`, one commit) plus room for
/// worktrees, all inside one TempDir.
pub struct Fixture {
    pub tmp: TempDir,
    pub repo: PathBuf,
}

impl Fixture {
    pub fn new() -> Fixture {
        let tmp = TempDir::new();
        let repo = tmp.0.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let fx = Fixture { tmp, repo };
        fx.git(&fx.repo, &["init", "-q", "-b", "main"]);
        fx.git(&fx.repo, &["config", "user.name", "wtree-test"]);
        fx.git(
            &fx.repo,
            &["config", "user.email", "wtree-test@example.invalid"],
        );
        fx.git(&fx.repo, &["config", "commit.gpgsign", "false"]);
        fx.commit(&fx.repo, "init");
        fx
    }

    pub fn git(&self, dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .output()
            .expect("failed to spawn git");
        assert!(
            out.status.success(),
            "git {args:?} in {} failed: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Append to a file and commit it — one new commit on the current branch
    /// of `dir`.
    pub fn commit(&self, dir: &Path, msg: &str) {
        let f = dir.join("f.txt");
        let mut content = std::fs::read_to_string(&f).unwrap_or_default();
        content.push_str(msg);
        content.push('\n');
        std::fs::write(&f, content).unwrap();
        self.git(dir, &["add", "-A"]);
        self.git(dir, &["commit", "-q", "-m", msg]);
    }

    /// `git worktree add -b <branch> <path> <from>` — new branch, new worktree.
    pub fn add_worktree(&self, branch: &str, from: &str) -> PathBuf {
        let path = self
            .tmp
            .0
            .join(format!("wtree-{}", branch.replace('/', "-")));
        self.git(
            &self.repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                branch,
                path.to_str().unwrap(),
                from,
            ],
        );
        path
    }

    /// Detached worktree at `from` (no branch checked out).
    pub fn add_worktree_detached(&self, name: &str, from: &str) -> PathBuf {
        let path = self.tmp.0.join(format!("wtree-{name}"));
        self.git(
            &self.repo,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                path.to_str().unwrap(),
                from,
            ],
        );
        path
    }

    /// Write a wtree state record for `worktree` — simulates what the
    /// `new`/`adopt` verbs will do in later stages.
    pub fn write_state(&self, worktree: &Path, branch: &str, kind: &str, parent: &str) {
        let dir = crate::repo::private_git_dir(worktree).unwrap();
        let state = crate::state::State {
            branch: branch.to_string(),
            kind: crate::state::Kind::parse(kind).unwrap(),
            parent: parent.to_string(),
        };
        crate::state::write(&dir, &state).unwrap();
    }

    /// Make the worktree dirty with an untracked file.
    pub fn make_dirty(&self, worktree: &Path) {
        std::fs::write(worktree.join("scratch.txt"), "wip\n").unwrap();
    }
}
