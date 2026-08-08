//! Git fact layer — read-only queries answered by shelling out to `git`.
//!
//! Nothing here mutates the repo; verbs (later stages) will run their own git
//! commands. Everything the judgment core needs is collected into a `World`
//! snapshot up front, so the judgment itself stays pure over plain data.

use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::rules::{Rules, SectionKind};
use crate::state::{self, StateRead};

pub(crate) fn run_git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Exit code plus captured stdout/stderr; `Err` only when git cannot be
/// spawned or dies to a signal. For commands whose non-zero exit is an
/// answer, not a failure (`merge-tree --write-tree`, `diff --quiet`).
pub(crate) fn run_git_code(dir: &Path, args: &[&str]) -> Result<(i32, String, String), String> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    let code = out
        .status
        .code()
        .ok_or_else(|| format!("git {} was killed by a signal", args.join(" ")))?;
    Ok((
        code,
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

/// `git merge-base --is-ancestor` as a boolean.
pub(crate) fn is_ancestor(dir: &Path, ancestor: &str, descendant: &str) -> bool {
    git_ok(dir, &["merge-base", "--is-ancestor", ancestor, descendant])
}

/// Handle on a repository, addressed by its common git dir (shared by every
/// worktree of the clone).
pub struct Repo {
    /// Absolute path of `<main worktree>/.git` (or the bare dir).
    pub common_dir: PathBuf,
}

impl Repo {
    pub fn discover(dir: &Path) -> Result<Repo, String> {
        let out = run_git(dir, &["rev-parse", "--git-common-dir"])?;
        let raw = PathBuf::from(out.trim());
        // --git-common-dir may print a path relative to cwd.
        let abs = if raw.is_absolute() {
            raw
        } else {
            dir.join(raw)
        };
        let common_dir = abs
            .canonicalize()
            .map_err(|e| format!("cannot resolve git common dir {}: {e}", abs.display()))?;
        Ok(Repo { common_dir })
    }

    pub fn branches(&self) -> Result<BTreeSet<String>, String> {
        let out = run_git(
            &self.common_dir,
            &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
        )?;
        Ok(out.lines().map(str::to_string).collect())
    }

    pub fn branch_exists(&self, name: &str) -> bool {
        git_ok(
            &self.common_dir,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{name}"),
            ],
        )
    }

    pub fn merge_base_exists(&self, a: &str, b: &str) -> bool {
        git_ok(&self.common_dir, &["merge-base", a, b])
    }

    /// Whether `branch` still carries work that `parent` does not have — the
    /// "would destroying this lose anything?" half of the work-loss layer.
    ///
    /// Port of wtree.sh's `commits_integrated`: four tests, cheapest first, any
    /// one of which is sufficient. They catch the same "already in the parent"
    /// state reached by different routes, and the last one is what makes the
    /// judgment squash-aware — after a squash merge the commits are reflected
    /// in the parent's content but not in its ancestry, so ancestry alone
    /// (tests 1-2) would report landed work as unreflected.
    ///
    /// Limits: it is a content judgment, so a branch whose changes were
    /// reverted in the parent also reads as reflected, and a branch that
    /// merely happens to add nothing (no commits, or commits that cancel out)
    /// reads as reflected whether or not it was ever merged. Test 4 needs
    /// git >= 2.38 for `merge-tree --write-tree`; where it is unavailable it
    /// simply does not fire and the confirmation key takes over.
    pub fn has_unreflected_commits(&self, parent: &str, branch: &str) -> Result<bool, String> {
        Ok(!self.reflected_in_parent(parent, branch)?)
    }

    fn reflected_in_parent(&self, parent: &str, branch: &str) -> Result<bool, String> {
        let dir = &self.common_dir;
        let oid = |rev: &str| run_git(dir, &["rev-parse", rev]).map(|s| s.trim().to_string());

        // 1. Same commit — a fresh worktree still on the parent's tip, and a
        //    worktree just merged, which leaves branch and parent equal.
        if oid(parent)? == oid(branch)? {
            return Ok(true);
        }
        // 2. Ancestor — as above, but the parent has since moved on.
        if is_ancestor(dir, branch, parent) {
            return Ok(true);
        }
        // 3. Nothing added since the fork point (`diff --quiet parent...branch`
        //    as a tree comparison, which needs no working tree). This measures
        //    FROM the merge base, so it says "this branch changed nothing", not
        //    "the parent already has this" — a squashed branch still looks like
        //    it added its own changes here, which is what test 4 is for.
        let base = run_git(dir, &["merge-base", parent, branch])?
            .trim()
            .to_string();
        if oid(&format!("{base}^{{tree}}"))? == oid(&format!("{branch}^{{tree}}"))? {
            return Ok(true);
        }
        // 4. Merging would add nothing: the merged tree equals the parent's
        //    tree as it stands. This is the one that recognizes content the
        //    parent absorbed under a different commit — a squash merge — and it
        //    keeps holding after the parent advances with unrelated changes.
        if let Ok((0, stdout, _)) =
            run_git_code(dir, &["merge-tree", "--write-tree", parent, branch])
        {
            let merged = stdout.lines().next().unwrap_or_default().trim();
            if !merged.is_empty() && merged == oid(&format!("{parent}^{{tree}}"))? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn list_worktrees(&self) -> Result<Vec<WorktreeInfo>, String> {
        let out = run_git(&self.common_dir, &["worktree", "list", "--porcelain"])?;
        let mut result = Vec::new();
        let mut cur: Option<WorktreeInfo> = None;
        for line in out.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                if let Some(wt) = cur.take() {
                    result.push(wt);
                }
                cur = Some(WorktreeInfo {
                    path: PathBuf::from(p),
                    head_branch: None,
                    bare: false,
                });
            } else if let Some(wt) = cur.as_mut() {
                if let Some(b) = line.strip_prefix("branch ") {
                    wt.head_branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
                } else if line == "bare" {
                    wt.bare = true;
                }
                // "HEAD <oid>", "detached", "prunable ..." etc. are ignored.
            }
        }
        if let Some(wt) = cur.take() {
            result.push(wt);
        }
        Ok(result)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    /// Short branch name of HEAD; `None` = detached (or bare).
    pub head_branch: Option<String>,
    pub bare: bool,
}

/// The worktree's own private git dir (`.git/worktrees/<name>`, or the common
/// dir itself for the primary worktree) — where the wtree state file lives.
pub fn private_git_dir(worktree: &Path) -> Result<PathBuf, String> {
    let out = run_git(worktree, &["rev-parse", "--absolute-git-dir"])?;
    Ok(PathBuf::from(out.trim()))
}

/// Short branch name of the worktree's HEAD; `Ok(None)` = detached.
pub fn head_branch(worktree: &Path) -> Result<Option<String>, String> {
    let out = Command::new("git")
        .current_dir(worktree)
        .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if out.status.success() {
        Ok(Some(
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
        ))
    } else if out.stderr.is_empty() {
        Ok(None) // symbolic-ref exits 1 quietly when HEAD is detached
    } else {
        Err(format!(
            "git symbolic-ref failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Uncommitted changes, staged or not, including untracked files.
pub fn is_dirty(worktree: &Path) -> Result<bool, String> {
    Ok(!run_git(worktree, &["status", "--porcelain"])?
        .trim()
        .is_empty())
}

/// First 5 hex chars of a hash over (HEAD oid, diff vs HEAD, untracked file
/// list) — the destroy confirmation key. Any commit, tracked edit or new
/// untracked file changes the key, invalidating a stale confirmation.
/// Hashing is delegated to `git hash-object` to avoid a hash crate.
pub fn confirmation_key(worktree: &Path) -> Result<String, String> {
    let head = run_git(worktree, &["rev-parse", "HEAD"])?;
    let diff = run_git(worktree, &["diff", "HEAD"])?;
    let untracked = run_git(worktree, &["ls-files", "--others", "--exclude-standard"])?;
    let material = format!("{head}\0{diff}\0{untracked}");
    let mut child = Command::new("git")
        .current_dir(worktree)
        .args(["hash-object", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run git hash-object: {e}"))?;
    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(material.as_bytes())
        .map_err(|e| format!("cannot write to git hash-object: {e}"))?;
    let out = child
        .wait_with_output()
        .map_err(|e| format!("git hash-object failed: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git hash-object failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let hex = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok(hex.chars().take(5).collect())
}

/// Facts about one worktree, as needed by the judgment core.
#[derive(Debug)]
pub struct WtFact {
    pub path: PathBuf,
    /// Short branch name of HEAD; `None` = detached.
    pub head: Option<String>,
    pub state: StateRead,
    pub dirty: bool,
    /// Commits not reflected in the derived parent (recorded parent for
    /// group/free records, rules-derived for fixed). `false` when there is
    /// no derivable/existing parent.
    pub unreflected: bool,
    /// `None` when it cannot be computed (e.g. unborn HEAD).
    pub confirmation_key: Option<String>,
}

/// Snapshot of every fact the judgment core needs — gathered once so the
/// judgment itself is pure.
#[derive(Debug)]
pub struct World {
    pub facts: Vec<WtFact>,
    /// Index into `facts` of the worktree containing the invoking cwd.
    pub current: usize,
    /// Index into `facts` of the primary worktree — the checkout that sits at
    /// `<common dir>/..`. `None` in a bare repo, which has none. git refuses to
    /// remove it, so `close` has to know which one it is.
    pub primary: Option<usize>,
    /// All local branches.
    pub branches: BTreeSet<String>,
    /// Branches that share a merge-base with the current worktree's HEAD
    /// (adopt's orphan-history check).
    pub shares_base_with_head: BTreeSet<String>,
}

impl World {
    pub fn current(&self) -> &WtFact {
        &self.facts[self.current]
    }
}

/// Gather the world as seen from `cwd`. `cfg` is needed only to derive the
/// parent of fixed branches for the `unreflected` fact.
pub fn gather(cwd: &Path, cfg: &Rules) -> Result<World, String> {
    let repo = Repo::discover(cwd)?;
    let branches = repo.branches()?;
    let toplevel_raw = PathBuf::from(run_git(cwd, &["rev-parse", "--show-toplevel"])?.trim());
    let toplevel = toplevel_raw.canonicalize().map_err(|e| {
        format!(
            "cannot resolve worktree root {}: {e}",
            toplevel_raw.display()
        )
    })?;

    let mut facts = Vec::new();
    for wt in repo.list_worktrees()? {
        if wt.bare {
            continue; // a bare entry has no checkout to judge
        }
        let path = wt.path.canonicalize().unwrap_or(wt.path);
        let head = wt.head_branch;
        let state = match private_git_dir(&path) {
            Ok(d) => state::read(&d),
            Err(e) => StateRead::Invalid { reason: e },
        };
        let dirty = is_dirty(&path)?;
        let parent = derived_parent_for_facts(cfg, &state, head.as_deref());
        let unreflected = match (&parent, &head) {
            (Some(p), Some(h)) if branches.contains(p) => {
                repo.has_unreflected_commits(p, h).unwrap_or(false)
            }
            _ => false,
        };
        let confirmation_key = confirmation_key(&path).ok();
        facts.push(WtFact {
            path,
            head,
            state,
            dirty,
            unreflected,
            confirmation_key,
        });
    }
    let current = facts
        .iter()
        .position(|f| f.path == toplevel)
        .ok_or_else(|| {
            format!(
                "cwd worktree {} not found in `git worktree list`",
                toplevel.display()
            )
        })?;

    let primary = repo
        .common_dir
        .parent()
        .and_then(|p| p.canonicalize().ok())
        .and_then(|p| facts.iter().position(|f| f.path == p));

    let mut shares_base_with_head = BTreeSet::new();
    if let Ok(head_oid) = run_git(cwd, &["rev-parse", "HEAD"]) {
        let head_oid = head_oid.trim().to_string();
        for b in &branches {
            if repo.merge_base_exists(&head_oid, b) {
                shares_base_with_head.insert(b.clone());
            }
        }
    }
    Ok(World {
        facts,
        current,
        primary,
        branches,
        shares_base_with_head,
    })
}

/// Mirror of the judgment core's parent derivation, used only to compute the
/// `unreflected` fact at gather time (`judge::Ctx::parent_of` is the
/// authoritative rule): valid matching record -> recorded parent; missing
/// record on a declared fixed branch -> its unique bare-listing section.
fn derived_parent_for_facts(cfg: &Rules, state: &StateRead, head: Option<&str>) -> Option<String> {
    match (state, head) {
        (StateRead::Valid(s), Some(h)) if s.branch == h => Some(s.parent.clone()),
        (StateRead::Missing, Some(h)) if cfg.section(SectionKind::Branch, h).is_some() => {
            cfg.bare_parent_sections(h).first().map(|s| s.name.clone())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules;
    use crate::state::Kind;
    use crate::testutil::Fixture;

    fn cfg(text: &str) -> Rules {
        let l = rules::load_str(text, ".git/wtree/rules");
        assert!(l.errors.is_empty(), "rules errors: {:?}", l.errors);
        l.rules
    }

    #[test]
    fn discover_and_private_dirs() {
        let fx = Fixture::new();
        let repo = Repo::discover(&fx.repo).unwrap();
        assert_eq!(
            repo.common_dir,
            fx.repo.join(".git").canonicalize().unwrap()
        );
        // same common dir when discovered from a secondary worktree
        let wt = fx.add_worktree("feature/a", "main");
        let repo2 = Repo::discover(&wt).unwrap();
        assert_eq!(repo2.common_dir, repo.common_dir);
        // private git dir of a secondary worktree lives under .git/worktrees/
        let private = private_git_dir(&wt).unwrap();
        assert!(
            private.starts_with(repo.common_dir.join("worktrees")),
            "{private:?}"
        );
        // and the primary's private dir is the common dir itself
        assert_eq!(
            private_git_dir(&fx.repo).unwrap().canonicalize().unwrap(),
            repo.common_dir
        );
    }

    #[test]
    fn worktree_enumeration_and_detached_head() {
        let fx = Fixture::new();
        let wt = fx.add_worktree("feature/a", "main");
        let det = fx.add_worktree_detached("det", "main");
        let repo = Repo::discover(&fx.repo).unwrap();
        let list = repo.list_worktrees().unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].head_branch.as_deref(), Some("main"));
        let by_path = |p: &Path| {
            list.iter()
                .find(|w| w.path.canonicalize().unwrap() == p.canonicalize().unwrap())
                .unwrap()
        };
        assert_eq!(by_path(&wt).head_branch.as_deref(), Some("feature/a"));
        assert_eq!(by_path(&det).head_branch, None);
        // head_branch query agrees
        assert_eq!(head_branch(&wt).unwrap().as_deref(), Some("feature/a"));
        assert_eq!(head_branch(&det).unwrap(), None);
    }

    #[test]
    fn branch_and_merge_base_queries() {
        let fx = Fixture::new();
        fx.add_worktree("feature/a", "main");
        let repo = Repo::discover(&fx.repo).unwrap();
        assert!(repo.branch_exists("main"));
        assert!(repo.branch_exists("feature/a"));
        assert!(!repo.branch_exists("nope"));
        assert!(repo.merge_base_exists("main", "feature/a"));
        assert_eq!(
            repo.branches().unwrap().into_iter().collect::<Vec<_>>(),
            vec!["feature/a".to_string(), "main".to_string()]
        );
    }

    #[test]
    fn dirty_and_unreflected_facts() {
        let fx = Fixture::new();
        let wt = fx.add_worktree("feature/a", "main");
        let repo = Repo::discover(&fx.repo).unwrap();
        assert!(!is_dirty(&wt).unwrap());
        assert!(!repo.has_unreflected_commits("main", "feature/a").unwrap());
        fx.make_dirty(&wt);
        assert!(is_dirty(&wt).unwrap());
        fx.commit(&wt, "work");
        assert!(!is_dirty(&wt).unwrap());
        assert!(repo.has_unreflected_commits("main", "feature/a").unwrap());
    }

    #[test]
    fn reflection_is_squash_aware() {
        let fx = Fixture::new();
        let wt = fx.add_worktree("feature/a", "main");
        let repo = Repo::discover(&fx.repo).unwrap();
        fx.commit(&wt, "work");
        assert!(repo.has_unreflected_commits("main", "feature/a").unwrap());
        // squash merge: the content lands in main under a different commit, so
        // the branch's own commits are in no ancestry of main
        fx.git(&fx.repo, &["merge", "--squash", "feature/a"]);
        fx.git(&fx.repo, &["commit", "-q", "-m", "squashed"]);
        assert!(!is_ancestor(&repo.common_dir, "feature/a", "main"));
        assert!(!repo.has_unreflected_commits("main", "feature/a").unwrap());
        // and it keeps holding after main advances with unrelated changes
        std::fs::write(fx.repo.join("other.txt"), "x\n").unwrap();
        fx.git(&fx.repo, &["add", "-A"]);
        fx.git(&fx.repo, &["commit", "-q", "-m", "unrelated"]);
        assert!(!repo.has_unreflected_commits("main", "feature/a").unwrap());
        // new work on the branch is unreflected again
        fx.commit(&wt, "more");
        assert!(repo.has_unreflected_commits("main", "feature/a").unwrap());
    }

    #[test]
    fn confirmation_key_changes_with_worktree_state() {
        let fx = Fixture::new();
        let wt = fx.add_worktree("feature/a", "main");
        let k1 = confirmation_key(&wt).unwrap();
        assert_eq!(k1.len(), 5);
        fx.make_dirty(&wt); // untracked file
        let k2 = confirmation_key(&wt).unwrap();
        assert_ne!(k1, k2);
        fx.commit(&wt, "work"); // HEAD moves
        let k3 = confirmation_key(&wt).unwrap();
        assert_ne!(k2, k3);
        // stable when nothing changes
        assert_eq!(k3, confirmation_key(&wt).unwrap());
    }

    #[test]
    fn gather_builds_world() {
        let fx = Fixture::new();
        let wt = fx.add_worktree("feature/a", "main");
        fx.write_state(&wt, "feature/a", "group:g", "main");
        let c = cfg("[main]\nchildren = group:g\n\n[group:g]\n");
        let w = gather(&wt, &c).unwrap();
        assert_eq!(w.facts.len(), 2);
        let cur = w.current();
        assert_eq!(cur.head.as_deref(), Some("feature/a"));
        match &cur.state {
            StateRead::Valid(s) => {
                assert_eq!(s.kind, Kind::Group("g".into()));
                assert_eq!(s.parent, "main");
            }
            other => panic!("expected valid state, got {other:?}"),
        }
        assert!(!cur.dirty && !cur.unreflected);
        assert!(cur.confirmation_key.is_some());
        assert!(w.branches.contains("main"));
        assert!(w.shares_base_with_head.contains("main"));
        // gathered from the primary worktree, current points at it
        let w2 = gather(&fx.repo, &c).unwrap();
        assert_eq!(w2.current().head.as_deref(), Some("main"));
    }
}
