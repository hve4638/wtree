//! Verb implementations — the side-effectful layer.
//!
//! Every policy decision is delegated to `judge::plan_*` over a gathered
//! `World`; nothing here re-judges. This module only locates the `.git/wtree/`
//! files, executes approved plans (git commands, state records, hooks) and
//! renders output.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, IsTerminal};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::config::{self, Config, CopyPattern, MergeMode, SectionKind};
use crate::judge::{self, Affordance, Ctx, DestroyPlan, Identity, MergePlan, NewPlan};
use crate::repo::{self, Repo};
use crate::settings::{self, Settings};
use crate::state::{self, Kind, State};

/// Label used when citing config rules in refusals and errors. Spelled as the
/// path a user would `cd` to, so a cited rule can be opened without guessing.
pub const CONFIG_LABEL: &str = ".git/wtree/config";

/// `Err` is printed to stderr by main, exit 1.
pub type CmdResult = Result<(), String>;

pub const NEW_USAGE: &str = "usage: wtree new <name> [--group G]";
pub const OPEN_USAGE: &str = "usage: wtree open <branch>";

fn wt_dir(common: &Path) -> PathBuf {
    common.join("wtree")
}

fn config_path(common: &Path) -> PathBuf {
    wt_dir(common).join("config")
}

fn settings_path(common: &Path) -> PathBuf {
    wt_dir(common).join("settings")
}

fn hooks_dir(common: &Path) -> PathBuf {
    wt_dir(common).join("hooks")
}

/// Primary worktree root: `<root>/.git` -> `<root>`.
fn primary_root(repo: &Repo) -> Result<PathBuf, String> {
    repo.common_dir
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            format!(
                "cannot derive the repo root from {}",
                repo.common_dir.display()
            )
        })
}

/// Load `.git/wtree/config`: missing file points at `wtree init`; warnings go to
/// stderr; any error aborts.
fn load_policy(repo: &Repo) -> Result<Config, String> {
    let path = config_path(&repo.common_dir);
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(format!(
                "no policy config at {} — run `wtree init` first",
                path.display()
            ));
        }
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let loaded = config::load_str(&text, CONFIG_LABEL);
    for w in &loaded.warnings {
        eprintln!("warning: {w}");
    }
    if !loaded.errors.is_empty() {
        for e in &loaded.errors {
            eprintln!("error: {e}");
        }
        return Err(format!(
            "{CONFIG_LABEL}: {} error(s) — fix the policy config first",
            loaded.errors.len()
        ));
    }
    Ok(loaded.config)
}

// ------------------------------------------------------------------ init ----

pub fn init(cwd: &Path) -> CmdResult {
    if repo::run_git(cwd, &["rev-parse", "--is-bare-repository"])?.trim() == "true" {
        return Err("wtree init: bare repositories are not supported".into());
    }
    let repo = Repo::discover(cwd)?;
    let cfg_path = config_path(&repo.common_dir);
    if cfg_path.exists() {
        return Err(format!(
            "wtree init: {} already exists — edit it instead of re-initializing",
            cfg_path.display()
        ));
    }
    let root = detect_root(cwd, &repo)?;
    let hooks = hooks_dir(&repo.common_dir);
    fs::create_dir_all(&hooks).map_err(|e| format!("cannot create {}: {e}", hooks.display()))?;
    // `.sample`, as git spells its own hook templates: a file named
    // `post-create` would be found on every `new` and warned about for not
    // being executable. Executable already, so enabling it is a rename.
    let sample = hooks.join("post-create.sample");
    write_if_absent(&sample, HOOK_SAMPLE, 0o755)?;
    let sett = settings_path(&repo.common_dir);
    write_if_absent(&sett, SETTINGS_TEMPLATE, 0o644)?;
    fs::write(&cfg_path, template(&root))
        .map_err(|e| format!("cannot write {}: {e}", cfg_path.display()))?;
    println!("initialized policy config at {}", cfg_path.display());
    println!("root branch: '{root}' (destroyable = false)");
    println!("declare children/groups there. also written:");
    println!("  {}  (machine-local, all commented out)", sett.display());
    println!("  {}  (rename to post-create to enable)", sample.display());
    Ok(())
}

/// Only fills a gap. `init` is guarded on the config file alone, so a settings
/// file or hook written by hand before it ran has to survive it.
fn write_if_absent(path: &Path, body: &str, mode: u32) -> Result<(), String> {
    if path.exists() {
        return Ok(());
    }
    fs::write(path, body).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|e| format!("cannot set mode on {}: {e}", path.display()))
}

const SETTINGS_TEMPLATE: &str = "\
# Machine-local settings. Unlike the policy config next to it, this file is not
# meant to be shared — it holds paths that are true only on this machine.

# Where `wtree new` places worktrees. A relative path resolves against the primary
# worktree root, not the cwd, so every worktree derives the same location.
# Unset = <repo parent>/<repo name>.worktrees
# worktree-dir = ../wts
";

const HOOK_SAMPLE: &str = "\
#!/usr/bin/env sh
# Runs after `wtree new` has created a worktree, with that worktree as the cwd.
# Rename this file to `post-create` to enable it.
#
#   WT_PATH         absolute path of the new worktree
#   WT_BRANCH       branch that was created
#   WT_PARENT       branch it was created from
#   WT_REPO         primary worktree root
#   WT_INTERACTIVE  1 when stdout is a terminal, else 0
#
# A non-zero exit is reported as a warning and the worktree is kept either way,
# so `set -e` is how a failure gets noticed.
#
# Files the parent already has belong in the policy config's `copy` key; a hook
# is for what has to be generated here.

set -eu

# cat > .cargo/config.toml <<CFG
# [build]
# target-dir = \"$WT_REPO/../.wtree-target\"
# CFG
";

/// DESIGN "root seeding": origin/HEAD symref -> main/master existence ->
/// current branch. Used only to prefill the init template.
fn detect_root(cwd: &Path, repo: &Repo) -> Result<String, String> {
    if let Ok(out) = repo::run_git(cwd, &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"]) {
        if let Some(b) = out.trim().strip_prefix("refs/remotes/origin/") {
            if !b.is_empty() {
                return Ok(b.to_string());
            }
        }
    }
    for cand in ["main", "master"] {
        if repo.branch_exists(cand) {
            return Ok(cand.to_string());
        }
    }
    match repo::head_branch(cwd)? {
        Some(b) => Ok(b),
        None => Err(
            "wtree init: cannot detect a root branch (no origin/HEAD, no main/master, HEAD detached)"
                .into(),
        ),
    }
}

fn template(root: &str) -> String {
    format!(
        "# wtree policy config — clone-local (never committed); share it by copying.\n\
         # [X] declares a fixed branch; [group:X] a set of fungible work\n\
         # branches. `children` = what may be created here, and what merges back here.\n\
         \n\
         [{root}]\n\
         destroyable = false\n\
         # children = group:work\n\
         \n\
         # [group:work]\n\
         # ephemeral = true\n\
         # A menu, not a policy — cut the prefixes this repo will not use.\n\
         # name-allow = feat/*, fix/*, refactor/*, perf/*, docs/*, test/*, chore/*\n"
    )
}

// ------------------------------------------------------------------- new ----

pub fn new(cwd: &Path, name: &str, group: Option<&str>) -> CmdResult {
    let repo = Repo::discover(cwd)?;
    let cfg = load_policy(&repo)?;
    let world = repo::gather(cwd, &cfg)?;
    let ctx = Ctx { world: &world, cfg: &cfg, label: CONFIG_LABEL };
    let plan = ctx.plan_new(name, group).map_err(|r| r.to_string())?;
    // The section whose `copy` list applies is the one the branch lands in. A
    // free branch has none, so it carries nothing (fail closed).
    let (parent, kind, copy_sec) = match plan {
        NewPlan::Fixed { parent, .. } => {
            (parent, None, Some((SectionKind::Branch, name.to_string())))
        }
        NewPlan::GroupMember { parent, group, .. } => (
            parent,
            Some(Kind::Group(group.clone())),
            Some((SectionKind::Group, group)),
        ),
        NewPlan::Free { parent, .. } => (parent, Some(Kind::Free), None),
    };
    let patterns = copy_sec
        .map(|(k, n)| cfg.copy_list(k, &n))
        .unwrap_or_default();

    let sett = settings::load(&settings_path(&repo.common_dir))?;
    let root = primary_root(&repo)?;
    let dest = worktree_dest(&root, &sett, name)?;
    if dest.exists() {
        return Err(format!(
            "wtree new: destination {} already exists",
            dest.display()
        ));
    }
    if let Some(base) = dest.parent() {
        fs::create_dir_all(base).map_err(|e| format!("cannot create {}: {e}", base.display()))?;
    }
    let dest_str = dest
        .to_str()
        .ok_or_else(|| "destination path is not valid UTF-8".to_string())?;

    // New branch forked at the parent worktree's current HEAD (= cwd's HEAD).
    repo::run_git(cwd, &["worktree", "add", "-q", "-b", name, dest_str, "HEAD"])?;

    let identity = match &kind {
        None => "fixed".to_string(),
        Some(k) => k.to_string(),
    };
    if let Some(kind) = kind {
        let record = State { branch: name.to_string(), kind, parent: parent.clone() };
        let written = repo::private_git_dir(&dest)
            .and_then(|d| state::write(&d, &record).map_err(|e| e.to_string()));
        if let Err(e) = written {
            // Roll back so no unmanaged residue is left behind.
            let mut msg = format!(
                "wtree new: failed to record state: {e}\nthe worktree and branch were rolled back"
            );
            if let Err(e2) = repo::run_git(cwd, &["worktree", "remove", "--force", dest_str]) {
                msg.push_str(&format!("\nwarning: rollback (worktree remove) failed: {e2}"));
            }
            if let Err(e2) = repo::run_git(cwd, &["branch", "-D", name]) {
                msg.push_str(&format!("\nwarning: rollback (branch -D) failed: {e2}"));
            }
            return Err(msg);
        }
    }

    run_post_create(&repo.common_dir, &dest, name, &parent, &root);

    println!("created '{name}' ({identity}) from '{parent}'");
    for line in copy_from_parent(&world, &dest, &parent, &patterns) {
        println!("{line}");
    }
    println!("cd {}", dest.display());
    Ok(())
}

// ------------------------------------------------------------- copy ----
//
// `git worktree add` checks out only what the branch tracks, so a fresh
// worktree has no `.env` and cannot be run until one is put there. The `copy`
// list names what crosses from the parent's worktree. Nothing here judges —
// `Config::copy_list` is the policy and this only carries it out.

/// Copies the parent's matching entries into `dest`, returning the lines to
/// print. A worktree is usable without them, so every failure here reports and
/// continues rather than undoing a worktree that was created successfully.
fn copy_from_parent(
    world: &repo::World,
    dest: &Path,
    parent: &str,
    patterns: &[CopyPattern],
) -> Vec<String> {
    if patterns.is_empty() {
        return Vec::new();
    }
    let Some(src) = world
        .facts
        .iter()
        .find(|f| f.head.as_deref() == Some(parent))
        .map(|f| f.path.clone())
    else {
        return vec![format!("copied nothing: parent '{parent}' has no worktree")];
    };
    let entries = match fs::read_dir(&src) {
        Ok(e) => e,
        Err(e) => return vec![format!("warning: cannot read {}: {e}", src.display())],
    };

    let (mut taken, mut notes) = (Vec::new(), Vec::new());
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.path().is_dir();
        if !patterns.iter().any(|p| p.matches(&name, is_dir)) {
            // A directory named by a pattern that lacks the trailing slash is
            // the one near-miss worth naming: the rule looks like it applies.
            if is_dir && patterns.iter().any(|p| !p.dir_only && config::glob_match(&p.glob, &name))
            {
                notes.push(format!("skipped '{name}': a directory needs a trailing '/'"));
            }
            continue;
        }
        let to = dest.join(&name);
        if to.exists() {
            // The branch tracks it; overwriting would leave the new worktree
            // dirty before the user has touched anything.
            notes.push(format!("skipped '{name}': already in the worktree"));
            continue;
        }
        match copy_entry(&entry.path(), &to, is_dir) {
            Ok(()) => taken.push(name),
            Err(e) => notes.push(format!("warning: cannot copy '{name}': {e}")),
        }
    }

    let mut out = Vec::new();
    if !taken.is_empty() {
        taken.sort();
        out.push(format!("copied {} from '{parent}'", taken.join(", ")));
    }
    out.extend(notes);
    out
}

fn copy_entry(from: &Path, to: &Path, is_dir: bool) -> io::Result<()> {
    if is_dir {
        copy_tree(from, to)
    } else {
        fs::copy(from, to).map(|_| ())
    }
}

/// Symlinks are recreated, not followed. A copied dependency tree is the case
/// this matters for: pnpm's `node_modules` links packages to each other, and
/// dereferencing those both explodes the copy and dies on the cycles that
/// circular dependencies produce.
fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(&src)?, &dst)?;
        } else if ft.is_dir() {
            copy_tree(&src, &dst)?;
        } else {
            fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ open ----
//
// Give an existing branch a worktree — the inverse of close, and what `new`
// cannot do because it always forks a new branch. Nothing is recorded: a fixed
// branch's identity is its config declaration, and any other branch stays
// unknown until the user adopts it from the worktree this creates.

pub fn open(cwd: &Path, branch: &str) -> CmdResult {
    let repo = Repo::discover(cwd)?;
    let cfg = load_policy(&repo)?;
    let world = repo::gather(cwd, &cfg)?;
    let ctx = Ctx { world: &world, cfg: &cfg, label: CONFIG_LABEL };
    let plan = ctx.plan_open(branch).map_err(|r| r.to_string())?;

    // Only a declared branch has a parent to copy from: anything else stays
    // unknown until it is adopted there, and an unknown branch has no section
    // whose `copy` list could be read.
    let carry = plan
        .fixed
        .then(|| ctx.parent_of(&Identity::Fixed { branch: plan.branch.clone() }))
        .flatten()
        .map(|(p, _)| (p, cfg.copy_list(SectionKind::Branch, &plan.branch)));

    let sett = settings::load(&settings_path(&repo.common_dir))?;
    let root = primary_root(&repo)?;
    let dest = worktree_dest(&root, &sett, &plan.branch)?;
    if dest.exists() {
        return Err(format!(
            "wtree open: destination {} already exists",
            dest.display()
        ));
    }
    if let Some(base) = dest.parent() {
        fs::create_dir_all(base).map_err(|e| format!("cannot create {}: {e}", base.display()))?;
    }
    let dest_str = dest
        .to_str()
        .ok_or_else(|| "destination path is not valid UTF-8".to_string())?;
    repo::run_git(&root, &["worktree", "add", "-q", dest_str, &plan.branch])?;

    let identity = if plan.fixed { "fixed" } else { "unknown" };
    println!("opened '{}' ({identity})", plan.branch);
    if let Some((parent, patterns)) = carry {
        for line in copy_from_parent(&world, &dest, &parent, &patterns) {
            println!("{line}");
        }
    }
    println!("cd {}", dest.display());
    if !plan.fixed {
        println!(
            "'{}' is not managed by wtree — to bring it in, run there: wtree adopt (--group G | --free) --parent P",
            plan.branch
        );
    }
    Ok(())
}

/// Placement: `<settings worktree-dir>/<sanitized branch>` when set (relative
/// paths resolve against the primary worktree root), else
/// `<repo parent>/<repo name>.worktrees/<sanitized branch>`.
fn worktree_dest(root: &Path, sett: &Settings, branch: &str) -> Result<PathBuf, String> {
    let base = match &sett.worktree_dir {
        Some(p) if p.is_absolute() => p.clone(),
        Some(p) => root.join(p),
        None => {
            let name = root
                .file_name()
                .ok_or_else(|| format!("cannot derive a repo name from {}", root.display()))?;
            let parent = root
                .parent()
                .ok_or_else(|| format!("repo root {} has no parent directory", root.display()))?;
            parent.join(format!("{}.worktrees", name.to_string_lossy()))
        }
    };
    Ok(normalize(&base.join(branch.replace('/', "-"))))
}

/// Fold away `.` and `..` so a relative `worktree-dir` does not surface as
/// `<root>/../<dir>` in the paths we print and hand to git. Purely textual —
/// the destination does not exist yet, so `canonicalize` is not an option, and
/// a shell's `cd` resolves `..` the same lexical way.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            // Pop only a real directory name; a `..` that would escape the
            // root, or follow another `..`, has to stay.
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                _ => out.push(c),
            },
            _ => out.push(c),
        }
    }
    out
}

/// `<common>/wtree/hooks/post-create`, run with cwd = the new worktree and the
/// WT_* env contract of the legacy mkwt hook. Hook failure is a warning; the
/// worktree is kept.
fn run_post_create(common: &Path, wt: &Path, branch: &str, parent: &str, repo_root: &Path) {
    let hook = hooks_dir(common).join("post-create");
    let Ok(meta) = fs::metadata(&hook) else {
        return; // no hook installed
    };
    if meta.permissions().mode() & 0o111 == 0 {
        eprintln!(
            "warning: {} exists but is not executable — skipped",
            hook.display()
        );
        return;
    }
    let interactive = if io::stdout().is_terminal() { "1" } else { "0" };
    match Command::new(&hook)
        .current_dir(wt)
        .env("WT_PATH", wt)
        .env("WT_BRANCH", branch)
        .env("WT_PARENT", parent)
        .env("WT_REPO", repo_root)
        .env("WT_INTERACTIVE", interactive)
        .status()
    {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!(
            "warning: post-create hook failed (exit {}); the worktree was still created",
            s.code().unwrap_or(-1)
        ),
        Err(e) => eprintln!(
            "warning: cannot run post-create hook: {e}; the worktree was still created"
        ),
    }
}

// ------------------------------------------------------------------ list ----

/// Commits the parent has that `branch` does not — the signal that `sync` is
/// due. Display only: judgment needs the `unreflected` boolean, not a count, so
/// this stays out of `repo::gather` where every verb would pay for it.
/// `None` when the count cannot be taken, which is the case for a fixed branch
/// whose config-derived parent has not been created yet.
fn behind_count(dir: &Path, branch: &str, parent: &str) -> Option<u32> {
    let range = format!("{branch}..{parent}");
    repo::run_git(dir, &["rev-list", "--count", &range])
        .ok()?
        .trim()
        .parse()
        .ok()
}

pub fn list(cwd: &Path) -> CmdResult {
    let repo = Repo::discover(cwd)?;
    let cfg = load_policy(&repo)?;
    let world = repo::gather(cwd, &cfg)?;
    let ctx = Ctx { world: &world, cfg: &cfg, label: CONFIG_LABEL };

    let mut seen: BTreeSet<String> = BTreeSet::new();
    println!("worktrees:");
    for (i, wt) in world.facts.iter().enumerate() {
        let marker = if i == world.current { '*' } else { ' ' };
        let name = wt
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| wt.path.display().to_string());
        let head = wt.head.clone().unwrap_or_else(|| "(detached)".into());
        if let Some(h) = &wt.head {
            seen.insert(h.clone());
        }
        let mut flags = String::new();
        if wt.dirty {
            flags.push_str(" [dirty]");
        }
        if wt.unreflected {
            flags.push_str(" [unreflected]");
        }
        let id = ctx.identity_of(wt);
        match &id {
            Identity::Unknown { reasons } => {
                println!("{marker} {name}  {head}  UNKNOWN{flags}");
                for r in reasons {
                    println!("      ! {r}");
                }
            }
            _ => {
                let ident = describe_identity(&ctx, &id);
                let (parent, behind) = match ctx.parent_of(&id) {
                    Some((p, _)) => {
                        let n = wt
                            .head
                            .as_deref()
                            .and_then(|h| behind_count(&repo.common_dir, h, &p))
                            .filter(|n| *n > 0)
                            .map(|n| format!(" [behind {n}]"))
                            .unwrap_or_default();
                        (format!("parent: {p}"), n)
                    }
                    None => ("root".to_string(), String::new()),
                };
                println!("{marker} {name}  {head}  {ident}  {parent}{flags}{behind}");
            }
        }
    }

    let rest: Vec<&String> = world.branches.iter().filter(|b| !seen.contains(*b)).collect();
    if !rest.is_empty() {
        println!("branches without worktrees:");
        for b in rest {
            let id = ctx.branch_identity(b);
            match &id {
                Identity::Unknown { .. } => {
                    println!("  {b}  UNKNOWN");
                    println!(
                        "      ! no worktree and no [branch] declaration — to manage it, create a worktree for it, then run: wtree adopt"
                    );
                }
                _ => {
                    let ident = describe_identity(&ctx, &id);
                    let parent = match ctx.parent_of(&id) {
                        Some((p, _)) => format!("parent: {p}"),
                        None => "root".to_string(),
                    };
                    println!("  {b}  {ident}  {parent}");
                }
            }
        }
    }
    Ok(())
}

fn describe_identity(ctx: &Ctx, id: &Identity) -> String {
    match id {
        Identity::Fixed { .. } => "fixed".into(),
        Identity::Free { .. } => "free".into(),
        Identity::GroupMember { group, .. } => {
            if ctx.cfg.ephemeral(group) {
                format!("group:{group} (ephemeral)")
            } else {
                format!("group:{group}")
            }
        }
        Identity::Unknown { .. } => "UNKNOWN".into(),
    }
}

// ------------------------------------------------------------------ info ----

pub fn info(cwd: &Path) -> CmdResult {
    let repo = Repo::discover(cwd)?;
    let cfg = load_policy(&repo)?;
    let world = repo::gather(cwd, &cfg)?;
    let ctx = Ctx { world: &world, cfg: &cfg, label: CONFIG_LABEL };
    let cur = world.current();

    println!("worktree: {}", cur.path.display());
    println!(
        "HEAD: {}",
        cur.head.clone().unwrap_or_else(|| "(detached)".into())
    );

    let id = ctx.current_identity();
    if let Identity::Unknown { reasons } = &id {
        println!("identity: unknown — unmanaged, fail closed");
        for r in reasons {
            println!("  - {r}");
        }
        let allowed: Vec<&str> = [
            "new", "open", "merge", "sync", "land", "destroy", "close", "list", "info", "init",
            "adopt",
        ]
        .into_iter()
        .filter(|v| judge::verb_allowed_when_unknown(v))
        .collect();
        println!("allowed verbs here: {}", allowed.join(", "));
        return Ok(());
    }

    match &id {
        Identity::Fixed { branch } => {
            let line = cfg
                .section(SectionKind::Branch, branch)
                .map(|s| s.line)
                .unwrap_or(0);
            println!("identity: fixed — declared [{branch}] ({CONFIG_LABEL}:{line})");
        }
        Identity::GroupMember { group, .. } => println!(
            "identity: group:{group} — state record in this worktree's private git dir"
        ),
        Identity::Free { .. } => {
            println!("identity: free — state record in this worktree's private git dir")
        }
        Identity::Unknown { .. } => unreachable!("handled above"),
    }
    match ctx.parent_of(&id) {
        Some((p, how)) => println!("parent: {p} ({how})"),
        None => println!("parent: none (root branch)"),
    }

    println!("rules:");
    match &id {
        Identity::Free { .. } => {
            println!("  children: none — free branches cannot have children (fail closed)")
        }
        Identity::Fixed { branch } => print_children(&ctx, SectionKind::Branch, branch),
        Identity::GroupMember { group, .. } => print_children(&ctx, SectionKind::Group, group),
        Identity::Unknown { .. } => unreachable!("handled above"),
    }
    if let Some((p, _)) = ctx.parent_of(&id) {
        // An unmanaged parent has no readable rules, and merge refuses rather
        // than read that absence as freedom — so this line must not list the
        // full set either, or it would advertise what the verb will refuse.
        if matches!(ctx.branch_identity(&p), Identity::Unknown { .. }) {
            println!("  merge to '{p}': no rules readable — '{p}' is unmanaged (fail closed)");
        } else {
            let (modes, _cite) = ctx.target_merge_modes(&p);
            let list = modes
                .iter()
                .map(|m| m.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let flag = if modes.len() == 1 {
                format!("(flag optional, --{} implied)", modes[0].as_str())
            } else {
                "(flag required)".to_string()
            };
            println!("  merge to '{p}': {list} {flag}");
        }
    }
    match &id {
        Identity::Fixed { branch } => println!("  destroyable: {}", cfg.destroyable(branch)),
        Identity::GroupMember { group, .. } => println!("  ephemeral: {}", cfg.ephemeral(group)),
        _ => {}
    }

    println!("preview:");
    match ctx.plan_merge(None) {
        Ok(p) => println!("  merge: '{}' -> '{}' (--{})", p.source, p.target, p.mode.as_str()),
        Err(r) => print_refusal_indented(&r),
    }
    match ctx.plan_sync() {
        Ok(p) => println!("  sync: merge '{}' into '{}'", p.parent, p.branch),
        Err(r) => print_refusal_indented(&r),
    }
    match ctx.plan_destroy(false, None) {
        Ok(p) => {
            println!("  destroy: would remove '{}'", p.branch);
            for c in &p.cascade {
                println!("    cascade (ephemeral, leaf first): '{c}'");
            }
        }
        Err(r) => print_refusal_indented(&r),
    }
    Ok(())
}

/// The contextual menu — `wtree` with no arguments, or `wtree help`. Lists the
/// verbs that would get past policy here and nothing else, so the screen is a
/// truthful answer to "what can I do from this worktree?".
///
/// Only the shape of each invocation appears. The data a verb operates on (the
/// branches `open` would take, the names `new` accepts) belongs to that verb's
/// own no-argument screen, which keeps this one from moving every time a
/// branch comes or goes. `--key` and `--force` are likewise absent: they are
/// guards that appear when something is off, and the refusal introduces them
/// with the situation that called for them.
pub fn help(cwd: &Path) -> CmdResult {
    let repo = Repo::discover(cwd)?;
    if !config_path(&repo.common_dir).exists() {
        println!("this repo has no wtree policy yet\n");
        println!("  init                      write a starter {CONFIG_LABEL}");
        return Ok(());
    }
    let cfg = load_policy(&repo)?;
    let world = repo::gather(cwd, &cfg)?;
    let ctx = Ctx { world: &world, cfg: &cfg, label: CONFIG_LABEL };

    let head = world.current().head.clone().unwrap_or_else(|| "(detached)".into());
    println!("{head} ({})", identity_word(&ctx.current_identity()));
    println!();

    let mut rows: Vec<(String, String)> = Vec::new();
    for a in ctx.affordances() {
        rows.push(match a {
            Affordance::New(g) => (
                if g.groups.len() > 1 {
                    "new <name> [--group G]".to_string()
                } else {
                    "new <name>".to_string()
                },
                "create a branch and its worktree".to_string(),
            ),
            Affordance::Open(_) => (
                "open <branch>".to_string(),
                "give an existing branch a worktree".to_string(),
            ),
            Affordance::Merge(g) => (
                format!("merge {}", mode_flags(&g.modes)),
                format!("merge into '{}'", g.target),
            ),
            Affordance::Sync(p) => (
                "sync".to_string(),
                format!("merge '{}' into '{}'", p.parent, p.branch),
            ),
            Affordance::Land(g) => (
                format!("land {}", mode_flags(&g.modes)),
                format!("merge into '{}', then destroy", g.target),
            ),
            Affordance::Close(p) => (
                "close".to_string(),
                match &p.branch {
                    Some(b) => format!("remove this worktree, keep '{b}'"),
                    None => "remove this worktree".to_string(),
                },
            ),
            Affordance::Destroy(g) => (
                "destroy".to_string(),
                format!("delete '{}' and its worktree", g.branch),
            ),
            Affordance::Adopt => (
                "adopt (--group G | --free) --parent P".to_string(),
                "record what this branch is, and whose child".to_string(),
            ),
        });
    }
    rows.push(("list".to_string(), "worktrees in this repo".to_string()));
    rows.push(("info".to_string(), "rules and previews for this worktree".to_string()));

    let width = rows.iter().map(|(u, _)| u.len()).max().unwrap_or(0);
    for (usage, note) in &rows {
        println!("  {usage:width$}  {note}");
    }
    println!("\nwtree help --all for every verb, whether or not it applies here");
    Ok(())
}

/// `--ff` when the target takes one mode, `[--squash|--no-ff]` when it takes
/// several — which is exactly when the flag stops being optional.
fn mode_flags(modes: &[MergeMode]) -> String {
    let flags: Vec<String> = modes.iter().map(|m| format!("--{}", m.as_str())).collect();
    match flags.len() {
        1 => flags[0].clone(),
        _ => format!("[{}]", flags.join("|")),
    }
}

fn identity_word(id: &Identity) -> String {
    match id {
        Identity::Fixed { .. } => "fixed".to_string(),
        Identity::GroupMember { group, .. } => format!("group:{group}"),
        Identity::Free { .. } => "free".to_string(),
        Identity::Unknown { .. } => "unmanaged".to_string(),
    }
}

/// `wtree new` with no name. The naming rules are what the user is missing at
/// that moment, so they are printed here rather than in the menu. Usage error
/// all the same — nothing was created — so this goes to stderr under exit 2.
pub fn usage_new(cwd: &Path) {
    eprintln!("{NEW_USAGE}");
    let Ok(g) = with_ctx(cwd, |ctx| ctx.gate_new()) else {
        return;
    };
    eprintln!();
    for r in &g.groups {
        let mut rule = if r.allow.is_empty() {
            "any name".to_string()
        } else {
            r.allow.join(" | ")
        };
        if !r.deny.is_empty() {
            rule.push_str(&format!("   (except {})", r.deny.join(" | ")));
        }
        eprintln!("  --group {:<12} {rule}", r.group);
    }
    for b in &g.bares {
        eprintln!("  {b:<20} fixed branch");
    }
    if g.star {
        eprintln!("  {:<20} any other name, as a free branch", "*");
    }
}

/// `wtree open` with no branch: the branches it would take. See `usage_new`
/// for why this is stderr under exit 2.
pub fn usage_open(cwd: &Path) {
    eprintln!("{OPEN_USAGE}");
    let Ok(candidates) = with_ctx(cwd, |ctx| Ok(ctx.open_candidates())) else {
        return;
    };
    if candidates.is_empty() {
        eprintln!("\nno branch is waiting for a worktree");
        return;
    }
    eprintln!();
    for c in &candidates {
        let note = if c.fixed { "" } else { "   (unmanaged until adopted)" };
        eprintln!("  {}{note}", c.branch);
    }
}

/// Gather the world and hand a `Ctx` to `f`. Used by the no-argument usage
/// screens, which have nothing to report if the repo or config is unusable —
/// the usage line alone is still the answer.
fn with_ctx<T>(cwd: &Path, f: impl FnOnce(&Ctx) -> judge::Decision<T>) -> Result<T, String> {
    let repo = Repo::discover(cwd)?;
    let cfg = load_policy(&repo)?;
    let world = repo::gather(cwd, &cfg)?;
    let ctx = Ctx { world: &world, cfg: &cfg, label: CONFIG_LABEL };
    f(&ctx).map_err(|r| r.to_string())
}

fn print_children(ctx: &Ctx, kind: SectionKind, name: &str) {
    match ctx.cfg.get(kind, name, "children") {
        Some(v) => {
            let line = ctx.cfg.line_of(kind, name, "children").unwrap_or(0);
            println!(
                "  children: {v}  (from {}, {}:{line})",
                kind.header(name),
                ctx.label
            );
        }
        None => println!("  children: none declared — nothing may be created here (fail closed)"),
    }
}

fn print_refusal_indented(r: &judge::Refusal) {
    for line in r.to_string().lines() {
        println!("  {line}");
    }
}

// ----------------------------------------------------------------- adopt ----
//
// The one recovery path out of an unknown identity: it writes the record that
// `wtree new` would have written at creation time. Validation is `new`'s,
// creation omitted — all of it in plan_adopt, none of it repeated here.

pub fn adopt(cwd: &Path, group: Option<&str>, free: bool, parent: &str) -> CmdResult {
    let repo = Repo::discover(cwd)?;
    let cfg = load_policy(&repo)?;
    let world = repo::gather(cwd, &cfg)?;
    let ctx = Ctx { world: &world, cfg: &cfg, label: CONFIG_LABEL };
    let plan = ctx.plan_adopt(group, free, parent).map_err(|r| r.to_string())?;

    // A record about to be replaced (mismatch recovery, re-adopt) is shown
    // before it is gone — never a silent overwrite.
    if let Some(prev) = &plan.previous {
        println!("replacing the existing record: {prev}");
    }
    let dir = repo::private_git_dir(&world.current().path)?;
    let record = State {
        branch: plan.branch,
        kind: plan.kind,
        parent: plan.parent,
    };
    state::write(&dir, &record).map_err(|e| {
        format!(
            "wtree adopt: cannot write the state record in {}: {e}",
            dir.display()
        )
    })?;
    println!(
        "adopted '{}' ({}) with parent '{}'",
        record.branch, record.kind, record.parent
    );
    Ok(())
}

// ----------------------------------------------------------------- merge ----
//
// Port of wtree.sh cmd_merge, generalized to the four policy modes. Every step
// runs in THIS worktree except the final fast-forward:
//   1. merge the target in memory; stop before touching anything if it
//      conflicts  (skipped for --ff, which can never conflict)
//   2. stash uncommitted work, so only committed work lands and the working
//      state survives the history rewrite
//   3. mode-specific rewrite of this branch (squash / rebase / merge commit)
//   4. fast-forward the target
//   5. restore the stash
//
// The target is never merged into, only fast-forwarded. Its worktree
// therefore cannot end up half-merged and needs no cleanliness check: git
// refuses the fast-forward by itself when it would overwrite work in
// progress there, and leaves unrelated work in progress alone.

pub fn merge(cwd: &Path, mode_flag: Option<MergeMode>, msg: Option<&str>) -> CmdResult {
    let repo = Repo::discover(cwd)?;
    let cfg = load_policy(&repo)?;
    let world = repo::gather(cwd, &cfg)?;
    let ctx = Ctx { world: &world, cfg: &cfg, label: CONFIG_LABEL };
    let plan = ctx.plan_merge(mode_flag).map_err(|r| r.to_string())?;
    let commit_msg = check_message("merge", plan.mode, msg)?;

    // Nothing to land is not a failure of any git step, so it is checked
    // before running any of them. Under `merge` it is a refusal; `land` treats
    // it as "the cleanup is the whole remaining job" instead.
    if !has_changes_to_merge(&world, &plan)? {
        return Err(format!(
            "wtree merge: nothing to merge: '{}' adds no changes relative to '{}'",
            plan.source, plan.target
        ));
    }
    run_merge(&world, &plan, commit_msg, false)
}

/// The message rule depends on the judged mode (the flag may be omitted when
/// the policy allows a single mode), so it is checked after the plan. A
/// superfluous -m is refused rather than ignored: a message that silently goes
/// nowhere leaves the caller believing they named the work.
fn check_message<'a>(
    verb: &str,
    mode: MergeMode,
    msg: Option<&'a str>,
) -> Result<Option<&'a str>, String> {
    match mode {
        MergeMode::Squash | MergeMode::NoFf => match msg {
            Some(m) => Ok(Some(m)),
            None => Err(format!(
                "wtree {verb}: --{} creates a new commit; -m <message> is required",
                mode.as_str()
            )),
        },
        MergeMode::Rebase | MergeMode::Ff => {
            if msg.is_some() {
                return Err(format!(
                    "wtree {verb}: --{} keeps each commit as-is; -m has nothing to name",
                    mode.as_str()
                ));
            }
            Ok(None)
        }
    }
}

/// 3-dot diff: does the source add anything relative to the target?
fn has_changes_to_merge(world: &repo::World, plan: &MergePlan) -> Result<bool, String> {
    let wt = &world.current().path;
    let (code, _, derr) = repo::run_git_code(
        wt,
        &["diff", "--quiet", &format!("{}...{}", plan.target, plan.source), "--"],
    )?;
    match code {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(format!("wtree merge: git diff failed: {}", derr.trim())),
    }
}

/// The execution half of merge, shared with `land`. `in_land` suppresses the
/// "worktree kept" note and the destroy hint (the worktree is about to go) and
/// attributes errors to the verb that was typed.
fn run_merge(
    world: &repo::World,
    plan: &MergePlan,
    commit_msg: Option<&str>,
    in_land: bool,
) -> CmdResult {
    let verb = if in_land { "land" } else { "merge" };
    let (branch, target, mode) = (plan.source.clone(), plan.target.clone(), plan.mode);
    let wt = world.current().path.clone();

    // Where the target is checked out, if anywhere — its files must move with
    // the fast-forward, so that is the worktree the ff runs in.
    let target_wt = world
        .facts
        .iter()
        .find(|f| f.head.as_deref() == Some(target.as_str()))
        .map(|f| f.path.clone());

    // --ff never rewrites this branch and never creates a commit, so there is
    // nothing to precheck, stash or roll back: either the target can simply
    // advance, or the merge is refused — ff-only with no fallback (DESIGN).
    if mode == MergeMode::Ff {
        if !repo::is_ancestor(&wt, &target, &branch) {
            return Err(format!(
                "wtree {verb}: cannot fast-forward: '{target}' has commits that '{branch}' lacks\n  run `wtree sync`, then retry\n  nothing was changed"
            ));
        }
        ff_target(&wt, target_wt.as_deref(), &target, &branch)
            .map_err(|e| format!("wtree {verb}: {e}; nothing was changed"))?;
        println!("fast-forwarded '{target}' to '{branch}' ({})", short_head(&wt));
        print_kept(&wt, in_land);
        return Ok(());
    }

    // The target tip is pinned once: the no-ff commit is built against it, so
    // if the target moves mid-run the final fast-forward fails instead of
    // silently reverting the moved-in-between commits.
    let target_oid = repo::run_git(&wt, &["rev-parse", &format!("refs/heads/{target}")])?
        .trim()
        .to_string();

    // Step 1 — conflict precheck. merge-tree merges in memory, touching no
    // working tree, index, or ref. It reads only the two tips and their merge
    // base, so it predicts the squash/rebase below even before they run —
    // which is what lets it run first, before anything is modified.
    let merged_tree = merge_tree_precheck(
        &wt,
        verb,
        &target_oid,
        &branch,
        &format!("merging onto '{target}'"),
        &format!("reconcile here first:  git merge {target}   (resolve, commit), then re-run"),
    )?;

    // Step 2 — stash, so only committed work lands and the working state
    // survives the rewrite below.
    let stashed = stash_push(&wt, verb, &branch)?;

    // Every failure past this point puts the branch back on its original
    // commit first, so "nothing merged" means what it says (a failed commit
    // would otherwise leave the squash reset applied, a conflicted rebase the
    // squash).
    let orig_head = match repo::run_git(&wt, &["rev-parse", "HEAD"]) {
        Ok(h) => h.trim().to_string(),
        Err(e) => {
            stash_pop(&wt, stashed);
            return Err(format!("wtree {verb}: {e}; nothing was changed"));
        }
    };
    let fail = |m: String| bail(&wt, &orig_head, stashed, format!("wtree {verb}: {m}"));

    // Step 3 — rewrite this branch per mode.
    match mode {
        // wtree.sh squash path: single commit at the merge base, then rebase.
        // The base is computed, never read from a file, so after a previous
        // merge (branch and target on the same commit) it IS that point —
        // already-landed work falls outside the range and cannot be replayed.
        MergeMode::Squash => {
            let m = commit_msg.expect("required above");
            let base = repo::run_git(&wt, &["merge-base", &target, &branch])
                .map_err(|_| {
                    fail(format!(
                        "no merge base between '{branch}' and '{target}'; nothing merged"
                    ))
                })?
                .trim()
                .to_string();
            repo::run_git(&wt, &["reset", "-q", "--soft", &base])
                .map_err(|_| fail(format!("could not squash '{branch}'; nothing merged")))?;
            repo::run_git(&wt, &["commit", "-q", "-m", m]).map_err(|e| {
                fail(format!("commit failed (hook or signing?): {e}; nothing merged"))
            })?;
            rebase_onto(&wt, &target).map_err(&fail)?;
        }
        // wtree.sh --no-squash path: each commit kept, replayed onto the target.
        MergeMode::Rebase => {
            rebase_onto(&wt, &target).map_err(&fail)?;
        }
        // A true merge commit, built without checking the target out:
        // commit-tree over the precheck's merged tree, then this branch
        // fast-forwards onto it (it is a parent, so the ff always applies).
        MergeMode::NoFf => {
            let m = commit_msg.expect("required above");
            let commit = repo::run_git(
                &wt,
                &["commit-tree", &merged_tree, "-p", &target_oid, "-p", &branch, "-m", m],
            )
            .map_err(|e| fail(format!("could not create the merge commit: {e}; nothing merged")))?
            .trim()
            .to_string();
            repo::run_git(&wt, &["merge", "--ff-only", &commit]).map_err(|e| {
                fail(format!(
                    "could not advance '{branch}' to the merge commit: {e}; nothing merged"
                ))
            })?;
        }
        MergeMode::Ff => unreachable!("returned above"),
    }

    // Counted before the target moves: exactly what is about to land (1 after
    // a squash; a rebase may have dropped commits already applied).
    let ncommits = repo::run_git(&wt, &["rev-list", "--count", &format!("{target}..{branch}")])
        .map_err(|e| fail(format!("{e}; nothing merged")))?
        .trim()
        .to_string();

    // Step 4 — fast-forward the target.
    ff_target(&wt, target_wt.as_deref(), &target, &branch)
        .map_err(|e| fail(format!("{e}; nothing merged")))?;
    let tip = short_head(&wt);

    // Step 5 — the worktree stays, so the stash always comes back.
    stash_pop(&wt, stashed);

    // ncommits is 0 when the rebase dropped every commit as already applied —
    // the first point where "the target did not move" is known; "merged … as
    // <sha>" would name the target's pre-existing tip.
    if ncommits == "0" {
        println!("nothing landed on '{target}': every commit was already there");
    } else {
        match mode {
            MergeMode::Squash => println!("merged '{branch}' onto '{target}' as {tip}"),
            MergeMode::Rebase => {
                println!("merged '{branch}' onto '{target}' ({ncommits} commits, tip {tip})")
            }
            MergeMode::NoFf => println!("merged '{branch}' into '{target}' as merge commit {tip}"),
            MergeMode::Ff => unreachable!("returned above"),
        }
    }
    print_kept(&wt, in_land);
    Ok(())
}

/// Under `land` the worktree is about to go, so neither the "kept" note nor
/// the destroy hint that follows it would be true.
fn print_kept(wt: &Path, in_land: bool) {
    if !in_land {
        println!("worktree kept @ {}; clean up with: wtree destroy", wt.display());
    }
}

// ------------------------------------------------------------------ sync ----
//
// Bring the recorded parent into the current branch — a true merge, never a
// squash (a squashed sync would repeat the same conflicts on every following
// sync — DESIGN). Same conflict precheck and stash round-trip as merge.

pub fn sync(cwd: &Path) -> CmdResult {
    let repo = Repo::discover(cwd)?;
    let cfg = load_policy(&repo)?;
    let world = repo::gather(cwd, &cfg)?;
    let ctx = Ctx { world: &world, cfg: &cfg, label: CONFIG_LABEL };
    let plan = ctx.plan_sync().map_err(|r| r.to_string())?;
    let (branch, parent) = (plan.branch, plan.parent);
    let wt = world.current().path.clone();

    // Sync is maintenance, so having nothing to do is success, not refusal.
    if repo::is_ancestor(&wt, &parent, &branch) {
        println!("'{branch}' is already up to date with '{parent}'");
        return Ok(());
    }

    merge_tree_precheck(
        &wt,
        "sync",
        &branch,
        &parent,
        &format!("merging '{parent}' into '{branch}'"),
        &format!("resolve by hand:  git merge {parent}   (resolve, commit) — that completes the sync"),
    )?;

    let stashed = stash_push(&wt, "sync", &branch)?;
    if let Err(e) = repo::run_git(&wt, &["merge", "-q", "--no-edit", &parent]) {
        let _ = repo::run_git(&wt, &["merge", "--abort"]);
        stash_pop(&wt, stashed);
        return Err(format!(
            "wtree sync: merging '{parent}' failed ('{parent}' moved since the precheck?): {e}\n  the merge was aborted; nothing was changed"
        ));
    }
    stash_pop(&wt, stashed);
    println!("synced '{branch}' with '{parent}' (now {})", short_head(&wt));
    Ok(())
}

// --------------------------------------------------------------- destroy ----
//
// Execution half of wtree.sh's cmd_destroy. All four safety layers are decided in
// `plan_destroy`; nothing here re-judges. The plan's cascade is already
// ordered leaf first, so running it in order never removes a parent before its
// ephemeral children.

pub fn destroy(cwd: &Path, force: bool, key: Option<&str>) -> CmdResult {
    let repo = Repo::discover(cwd)?;
    let cfg = load_policy(&repo)?;
    let world = repo::gather(cwd, &cfg)?;
    let ctx = Ctx { world: &world, cfg: &cfg, label: CONFIG_LABEL };
    let plan = ctx.plan_destroy(force, key).map_err(|r| r.to_string())?;
    let targets = resolve_targets(&world, &plan)?;
    execute_destroy(&primary_root(&repo)?, &targets, "destroy", "")
}

/// One branch of a destroy plan, with what removing it needs.
struct Target {
    branch: String,
    path: PathBuf,
    /// `git worktree remove` refuses on modified or untracked files, so a
    /// destroy the judge cleared through the confirmation key needs `--force`
    /// to carry out what the key confirmed.
    dirty: bool,
}

/// Every branch in the plan, cascade first, paired with the worktree that
/// holds it. Resolved in full before the first removal, so a plan that cannot
/// be carried out fails before it has changed anything.
fn resolve_targets(world: &repo::World, plan: &DestroyPlan) -> Result<Vec<Target>, String> {
    plan.cascade
        .iter()
        .chain(std::iter::once(&plan.branch))
        .map(|b| {
            world
                .facts
                .iter()
                .find(|f| f.head.as_deref() == Some(b.as_str()))
                .map(|f| Target {
                    branch: b.clone(),
                    path: f.path.clone(),
                    dirty: f.dirty,
                })
                .ok_or_else(|| {
                    format!("wtree: '{b}' has no worktree to remove; nothing was changed")
                })
        })
        .collect()
}

/// Remove each target in order. A failure stops the run and names exactly how
/// far it got — under `land` the removals sit after a merge that already
/// succeeded, and git's own error says nothing about that.
fn execute_destroy(root: &Path, targets: &[Target], verb: &str, note: &str) -> CmdResult {
    for (i, t) in targets.iter().enumerate() {
        let Err(e) = remove_one(root, t) else {
            continue;
        };
        let mut msg = format!("wtree {verb}: {e}{note}");
        let done: Vec<&str> = targets[..i].iter().map(|t| t.branch.as_str()).collect();
        if done.is_empty() {
            msg.push_str("\n  nothing was removed");
        } else {
            msg.push_str(&format!("\n  already removed: {}", done.join(", ")));
        }
        let left: Vec<&str> = targets[i..].iter().map(|t| t.branch.as_str()).collect();
        msg.push_str(&format!("\n  still present: {}", left.join(", ")));
        return Err(msg);
    }
    Ok(())
}

fn remove_one(root: &Path, t: &Target) -> Result<(), String> {
    let path = t
        .path
        .to_str()
        .ok_or_else(|| format!("worktree path {} is not valid UTF-8", t.path.display()))?;
    let mut args = vec!["worktree", "remove"];
    if t.dirty {
        args.push("--force");
    }
    args.push(path);
    repo::run_git(root, &args)
        .map_err(|e| format!("could not remove the worktree of '{}': {e}", t.branch))?;
    // Force-delete is safe here: the judge cleared this branch, either because
    // nothing would be lost or because the confirmation key said to discard it.
    // git's "Deleted branch … (was <sha>)" is worth keeping — that sha is how
    // the branch is recovered.
    let deleted = repo::run_git(root, &["branch", "-D", &t.branch]).map_err(|e| {
        format!(
            "removed the worktree of '{}' but could not delete the branch: {e}",
            t.branch
        )
    })?;
    let _ = repo::run_git(root, &["worktree", "prune"]);
    // wtree.sh: drop the placement folder once its last worktree is gone.
    if let Some(dir) = t.path.parent() {
        let _ = fs::remove_dir(dir);
    }
    println!("destroyed worktree {}", t.path.display());
    println!("  {}", deleted.trim());
    Ok(())
}

// ----------------------------------------------------------------- close ----
//
// Remove this worktree and keep the branch — the one thing destroy cannot do,
// and the reason raw `git worktree remove` was needed until now. `destroyable
// = false` is not consulted: a protected branch is precisely the kind that
// wants its checkout cleared away between spells of work.

pub fn close(cwd: &Path, key: Option<&str>) -> CmdResult {
    let repo = Repo::discover(cwd)?;
    let cfg = load_policy(&repo)?;
    let world = repo::gather(cwd, &cfg)?;
    let ctx = Ctx { world: &world, cfg: &cfg, label: CONFIG_LABEL };
    let plan = ctx.plan_close(key).map_err(|r| r.to_string())?;

    let path = world.current().path.clone();
    let path_str = path
        .to_str()
        .ok_or_else(|| format!("worktree path {} is not valid UTF-8", path.display()))?;
    // Removal runs from the primary worktree: this one is about to go, and git
    // will not remove the worktree it is standing in.
    let root = primary_root(&repo)?;
    let mut args = vec!["worktree", "remove"];
    if plan.dirty {
        // git refuses on modified or untracked files; the confirmation key
        // above is what said to discard them.
        args.push("--force");
    }
    args.push(path_str);
    repo::run_git(&root, &args).map_err(|e| format!("wtree close: {e}\n  nothing was removed"))?;
    let _ = repo::run_git(&root, &["worktree", "prune"]);
    // As in destroy: drop the placement folder once its last worktree is gone.
    if let Some(dir) = path.parent() {
        let _ = fs::remove_dir(dir);
    }

    println!("closed worktree {}", path.display());
    match &plan.branch {
        Some(b) => {
            println!("  branch '{b}' is kept");
            if plan.drops_record {
                println!(
                    "  its state record went with the worktree — '{b}' is unmanaged now; reopen it and adopt to manage it again"
                );
            }
        }
        None => println!("  HEAD was detached; there was no branch to keep"),
    }
    Ok(())
}

// ------------------------------------------------------------------ land ----
//
// merge and destroy in one call, for when the work is finished. Every check
// either half makes runs BEFORE the merge (DESIGN's preflight): a merge that
// succeeds followed by a destroy that refuses is exactly the half-done state
// land exists to avoid.

const LAND_NOTE: &str = "\n  the merge before it already succeeded";

pub fn land(cwd: &Path, mode_flag: Option<MergeMode>, msg: Option<&str>) -> CmdResult {
    let repo = Repo::discover(cwd)?;
    let cfg = load_policy(&repo)?;
    let world = repo::gather(cwd, &cfg)?;
    let ctx = Ctx { world: &world, cfg: &cfg, label: CONFIG_LABEL };

    // ---- preflight: both halves judged while nothing has happened yet ----
    let plan = ctx.plan_merge(mode_flag).map_err(as_land)?;
    let commit_msg = check_message("land", plan.mode, msg)?;
    // Uncommitted work is the one work-loss cause the merge cannot resolve: it
    // is stashed and put back, and the destroy would then have to throw it
    // away. Refused up front, as in wtree.sh, so land does both or neither.
    if world.current().dirty {
        return Err(format!(
            "wtree land: '{}' has uncommitted changes, which land would have to leave behind\n  commit them, or run `wtree merge` and then `wtree destroy`",
            plan.source
        ));
    }
    let preflight = plan_destroy_for_land(&ctx).map_err(as_land)?;
    // Resolved and discarded: it proves every branch the destroy half will
    // touch still has a worktree, while nothing has happened yet.
    let _ = resolve_targets(&world, &preflight)?;

    let merged = has_changes_to_merge(&world, &plan)?;
    if merged {
        run_merge(&world, &plan, commit_msg, true)?;
    } else {
        // Not a failure: there is nothing to publish, so the cleanup is the
        // whole remaining job — and it is the state a successful `merge`
        // leaves behind, which is the documented way to reach `land`.
        println!("nothing to merge onto '{}'; going straight to destroy", plan.target);
    }

    // The merge moved this branch, so the destroy is carried out on freshly
    // gathered facts rather than the preflight's.
    let note = if merged { LAND_NOTE } else { "" };
    let world = repo::gather(cwd, &cfg)?;
    let ctx = Ctx { world: &world, cfg: &cfg, label: CONFIG_LABEL };
    let plan = plan_destroy_for_land(&ctx)
        .map_err(|r| format!("{}{note}", as_land(r).trim_end()))?;
    let targets = resolve_targets(&world, &plan)?;
    execute_destroy(&primary_root(&repo)?, &targets, "land", note)
}

/// A refusal is attributed to the verb that was typed, not to the half that
/// produced it (wtree.sh keeps `prog` at "wtree land" for the same reason): under
/// `land`, merge's and destroy's judgments both speak as `wtree land`.
fn as_land(r: judge::Refusal) -> String {
    judge::Refusal { verb: "land", ..r }.to_string()
}

/// destroy's judgment as `land` needs it.
///
/// land refuses a dirty worktree up front, so the only work-loss cause left
/// for the subject branch is "commits not reflected in the parent" — which is
/// precisely what the merge half resolves; counting it as loss would make land
/// refuse the work it is there to do. Handing `plan_destroy` the current
/// confirmation key clears that one layer and nothing else: the policy layer,
/// the child scan and the relation layer still decide, and each ephemeral
/// child's own dirty/unreflected state still blocks the whole cascade.
fn plan_destroy_for_land(ctx: &Ctx) -> judge::Decision<DestroyPlan> {
    let key = ctx.world.current().confirmation_key.clone();
    ctx.plan_destroy(false, key.as_deref())
}

// ---------------------------------------------------- merge/sync helpers ----

/// `git merge-tree --write-tree <ours> <theirs>` — the wtree.sh conflict
/// precheck: merges in memory, touching nothing. Ok(merged tree oid) when
/// clean. Exit 1 is the authoritative conflict signal; the file list is
/// best-effort parsing of the output.
fn merge_tree_precheck(
    wt: &Path,
    verb: &str,
    ours: &str,
    theirs: &str,
    doing: &str,
    hint: &str,
) -> Result<String, String> {
    let (code, stdout, stderr) =
        repo::run_git_code(wt, &["merge-tree", "--write-tree", ours, theirs])?;
    match code {
        0 => Ok(stdout.lines().next().unwrap_or_default().to_string()),
        1 => {
            let files = conflicted_files(&stdout);
            let list = if files.is_empty() { "?".to_string() } else { files.join(", ") };
            Err(format!(
                "wtree {verb}: {doing} would conflict in: {list}\n  {hint}\n  nothing was changed"
            ))
        }
        _ => Err(format!(
            "wtree {verb}: git merge-tree failed (git >= 2.38 required): {}",
            stderr.trim()
        )),
    }
}

/// Conflicted filenames from `merge-tree --write-tree` conflict output: the
/// "Conflicted file info" lines (2..first blank), tab-separated, deduped.
fn conflicted_files(out: &str) -> Vec<String> {
    let mut files = BTreeSet::new();
    for line in out.lines().skip(1) {
        if line.is_empty() {
            break;
        }
        if let Some((_, f)) = line.split_once('\t') {
            files.insert(f.to_string());
        }
    }
    files.into_iter().collect()
}

/// Stash uncommitted work (wtree.sh step 2). `-u` carries untracked files along;
/// the matching pop uses `--index` to put the staged/unstaged split back.
fn stash_push(wt: &Path, verb: &str, branch: &str) -> Result<bool, String> {
    if !repo::is_dirty(wt)? {
        return Ok(false);
    }
    repo::run_git(wt, &["stash", "push", "-u", "-q", "-m", &format!("wtree {verb}: {branch}")])
        .map_err(|e| {
            format!("wtree {verb}: could not stash uncommitted changes: {e}\n  nothing was changed")
        })?;
    Ok(true)
}

/// Restore the stash. A failed pop keeps the entry; that is reported with
/// recovery guidance but never turns the already-done work into a failure of
/// the verb.
fn stash_pop(wt: &Path, stashed: bool) {
    if !stashed {
        return;
    }
    if repo::run_git(wt, &["stash", "pop", "--index", "-q"]).is_ok() {
        return;
    }
    eprintln!("warning: could not restore your uncommitted changes automatically.");
    eprintln!("  they are kept in the stash:  git -C {} stash list", wt.display());
    eprintln!("  recover with:                git -C {} stash pop --index", wt.display());
}

/// Failure path shared by every post-stash merge step: abort any in-flight
/// rebase, put the branch back on its original commit (--keep refuses instead
/// of discarding if the tree somehow is not clean), restore the stash, and
/// return the message for the caller to fail with.
fn bail(wt: &Path, orig_head: &str, stashed: bool, msg: String) -> String {
    let _ = repo::run_git(wt, &["rebase", "--abort"]);
    let _ = repo::run_git(wt, &["reset", "-q", "--keep", orig_head]);
    stash_pop(wt, stashed);
    msg
}

/// wtree.sh step 4: rebase onto the target so it only ever fast-forwards. The
/// precheck cleared this, so a conflict here means the target moved in
/// between; the bail's abort matters beyond tidiness — a worktree left
/// mid-rebase has a detached HEAD, which locks out every verb until a human
/// finishes it by hand.
fn rebase_onto(wt: &Path, target: &str) -> Result<(), String> {
    repo::run_git(wt, &["rebase", "-q", target]).map(drop).map_err(|_| {
        format!("rebase onto '{target}' conflicted ('{target}' moved since the precheck); re-run")
    })
}

/// Fast-forward the target, never merging into it. Checked out somewhere: the
/// ff runs in that worktree so its files move with the ref. Not checked out:
/// there are no files to move, so moving the ref IS the fast-forward.
fn ff_target(wt: &Path, target_wt: Option<&Path>, target: &str, branch: &str) -> Result<(), String> {
    match target_wt {
        Some(tw) => repo::run_git(tw, &["merge", "--ff-only", branch]).map(drop).map_err(|_| {
            format!(
                "could not fast-forward '{target}' in {} ('{target}' moved, or work in progress there touches the same files)",
                tw.display()
            )
        }),
        None => {
            if !repo::is_ancestor(wt, target, branch) {
                return Err(format!("'{target}' moved and can no longer fast-forward"));
            }
            repo::run_git(
                wt,
                &[
                    "update-ref",
                    "-m",
                    &format!("wtree merge: {branch}"),
                    &format!("refs/heads/{target}"),
                    branch,
                ],
            )
            .map(drop)
            .map_err(|e| format!("could not move '{target}': {e}"))
        }
    }
}

fn short_head(wt: &Path) -> String {
    repo::run_git(wt, &["rev-parse", "--short", "HEAD"])
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "?".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflicted_files_parsed_from_merge_tree_output() {
        let out = "badf00dtree\n\
                   100644 aaaa 1\tsrc/a.txt\n\
                   100644 bbbb 2\tsrc/a.txt\n\
                   100644 cccc 3\tsrc/a.txt\n\
                   100644 dddd 1\tb.txt\n\
                   \n\
                   Auto-merging src/a.txt\n\
                   CONFLICT (content): Merge conflict in src/a.txt\n";
        assert_eq!(
            conflicted_files(out),
            vec!["b.txt".to_string(), "src/a.txt".into()]
        );
        // no conflicted-file section at all
        assert_eq!(conflicted_files("deadbeef\n"), Vec::<String>::new());
    }

    #[test]
    fn relative_worktree_dir_yields_a_clean_absolute_path() {
        let root = Path::new("/repos/proj-main");
        let dest = |dir: &str| {
            let sett = Settings { worktree_dir: Some(PathBuf::from(dir)) };
            worktree_dest(root, &sett, "feat/x").unwrap()
        };
        assert_eq!(dest("../proj.worktrees"), Path::new("/repos/proj.worktrees/feat-x"));
        assert_eq!(dest("./wtree"), Path::new("/repos/proj-main/wtree/feat-x"));
        assert_eq!(dest("/abs/wtree"), Path::new("/abs/wtree/feat-x"));

        // Derived default: no setting at all.
        let sett = Settings { worktree_dir: None };
        assert_eq!(
            worktree_dest(root, &sett, "dev").unwrap(),
            Path::new("/repos/proj-main.worktrees/dev")
        );
    }

    #[test]
    fn normalize_keeps_a_parent_ref_it_cannot_resolve() {
        // Escaping the root, and `..` following `..`, must survive untouched.
        assert_eq!(normalize(Path::new("/../a")), Path::new("/../a"));
        assert_eq!(normalize(Path::new("a/../../b")), Path::new("../b"));
    }
}
