//! Verb implementations — the side-effectful layer.
//!
//! Every policy decision is delegated to `judge::plan_*` over a gathered
//! `World`; nothing here re-judges. This module only locates the `.git/wtree/`
//! files, executes approved plans (git commands, state records, hooks) and
//! renders output.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use console::style;

use crate::judge::{self, Affordance, Ctx, DestroyPlan, Identity, MergePlan, NewPlan};
use crate::prompt;
use crate::repo::{self, Repo};
use crate::rules::{self, CopyPattern, MergeMode, Rules, SectionKind};
use crate::settings::{self, SETTINGS_LABEL, Settings};
use crate::state::{self, Kind, State};

/// Label used when citing rules in refusals and errors. Spelled as the
/// path a user would `cd` to, so a cited rule can be opened without guessing.
pub const RULES_LABEL: &str = ".git/wtree/rules";

/// `Err` is printed to stderr by main, exit 1.
pub type CmdResult = Result<(), String>;

pub const NEW_USAGE: &str = "usage: wtree new <name> [--group G] [--dir <name>]";
pub const OPEN_USAGE: &str = "usage: wtree open <branch>";
pub const INIT_USAGE: &str = "usage: wtree init [--new | --load [path]] [--force]";
pub const SAVE_USAGE: &str = "usage: wtree save [path] [--force]";

fn wt_dir(common: &Path) -> PathBuf {
    common.join("wtree")
}

fn rules_path(common: &Path) -> PathBuf {
    wt_dir(common).join("rules")
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

/// Root of the worktree the command was run from, which is the primary root
/// only when that is where you are standing. Resolved through git so a
/// subdirectory works the same as the top.
fn worktree_root(cwd: &Path) -> Result<PathBuf, String> {
    let out = repo::run_git(cwd, &["rev-parse", "--show-toplevel"])?;
    let p = PathBuf::from(out.trim());
    p.canonicalize()
        .map_err(|e| format!("cannot resolve {}: {e}", p.display()))
}

/// Load `.git/wtree/rules`: missing file points at `wtree init`; warnings go to
/// stderr; any error aborts.
fn load_rules(repo: &Repo) -> Result<Rules, String> {
    let path = rules_path(&repo.common_dir);
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        // Where the file goes is `init`'s business to say and the rule
        // citations' to cite; a reader who has none yet is told what is
        // missing, the way git names a repository rather than a path.
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err("no policy rules in this repository — run `wtree init` first".to_string());
        }
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let loaded = rules::load_str(&text, RULES_LABEL);
    for w in &loaded.warnings {
        eprintln!("warning: {w}");
    }
    if !loaded.errors.is_empty() {
        for e in &loaded.errors {
            eprintln!("error: {e}");
        }
        return Err(format!(
            "{RULES_LABEL}: {} error(s) — fix the policy rules first",
            loaded.errors.len()
        ));
    }
    Ok(loaded.rules)
}

// ------------------------------------------------------------------ init ----

/// Where `wtree init` takes the rules from. `Ask` is the bare invocation: it
/// resolves to one of the other two by prompting, and refuses outright when
/// there is no terminal to prompt on.
pub enum InitMode {
    New,
    /// `None` means "work it out from the worktree roots", which succeeds only
    /// when the answer is unambiguous.
    Load(Option<PathBuf>),
    Ask,
}

/// A `.wtree/` holding something `init` could load, and the worktree it came
/// from. The label is what the menu and the refusals call it.
struct Candidate {
    path: PathBuf,
    label: &'static str,
}

const HERE: &str = "this worktree";
const MAIN: &str = "main worktree";

/// The two files `.wtree/` may carry. Hooks are deliberately not among them:
/// they are executable scripts, and picking them up from a checkout would make
/// `init` run someone else's code.
const SEED: [&str; 2] = ["rules", "settings"];

pub fn init(cwd: &Path, mode: InitMode, force: bool) -> CmdResult {
    if repo::run_git(cwd, &["rev-parse", "--is-bare-repository"])?.trim() == "true" {
        return Err("error: bare repositories are not supported".into());
    }
    let repo = Repo::discover(cwd)?;
    let wt = wt_dir(&repo.common_dir);
    let occupied: Vec<&str> = SEED.into_iter().filter(|n| wt.join(n).exists()).collect();

    let (source, force) = match mode {
        InitMode::New => (None, force),
        InitMode::Load(Some(p)) => (Some(cwd.join(p)), force),
        InitMode::Load(None) => (Some(resolve_load(cwd, &repo)?), force),
        // Answering the overwrite prompt with yes *is* the force; there is no
        // second flag to reach for once a person has said so to their face.
        InitMode::Ask => match ask(cwd, &repo, &occupied)? {
            Some(choice) => (choice, true),
            None => return Ok(()),
        },
    };

    if !occupied.is_empty() && !force {
        return Err(format!(
            "error: {} already has {} — pass --force to replace it \
             (the current one is kept in {})",
            wt.display(),
            occupied.join(" and "),
            wt.join(".backup").display()
        ));
    }

    // Read and validate before touching anything. A source with errors must
    // leave the repo exactly as it was, which also means it must not consume
    // the --force that would have backed the old rules up.
    let seed = match &source {
        Some(dir) => read_seed(dir)?,
        None => Seed::default(),
    };
    // Only needed for the template, and it can fail on a detached HEAD — no
    // reason to let that block a load.
    let root = match seed.rules {
        Some(_) => None,
        None => Some(detect_root(cwd, &repo)?),
    };

    // Rules are always written, from the seed or the template. Settings only
    // when the seed carries one: a `.wtree/` holding rules alone says nothing
    // about where *this* machine puts its worktrees, and replacing a local
    // `worktree-dir` with a blank template is not what "load the rules" means.
    let replacing: Vec<&str> = occupied
        .iter()
        .copied()
        .filter(|n| *n == "rules" || seed.settings.is_some())
        .collect();
    let backup = if replacing.is_empty() {
        None
    } else {
        Some(back_up(&wt, &replacing)?)
    };

    let hooks = hooks_dir(&repo.common_dir);
    fs::create_dir_all(&hooks).map_err(|e| format!("cannot create {}: {e}", hooks.display()))?;
    // `.sample`, as git spells its own hook templates: a file named
    // `post-create` would be found on every `new` and warned about for not
    // being executable. Executable already, so enabling it is a rename.
    let sample = hooks.join("post-create.sample");
    write_if_absent(&sample, HOOK_SAMPLE, 0o755)?;

    let rules_file = rules_path(&repo.common_dir);
    let sett = settings_path(&repo.common_dir);
    let rules_body = match &seed.rules {
        Some(t) => t.clone(),
        None => template(
            root.as_deref()
                .expect("root is detected when there is no seed"),
        ),
    };
    write_new(&rules_file, &rules_body, 0o644)?;
    let kept_settings = match &seed.settings {
        Some(t) => {
            write_new(&sett, t, 0o644)?;
            false
        }
        // Fills the gap only. An existing settings is left exactly as it is.
        None => !write_if_absent(&sett, SETTINGS_TEMPLATE, 0o644)?,
    };

    match &source {
        Some(dir) => println!("loaded {} from {}", seed.describe(), dir.display()),
        None => println!("initialized policy rules at {}", rules_file.display()),
    }
    if let Some(r) = &root {
        println!("root branch: '{r}' (destroyable = false)");
    }
    println!("  {}", rules_file.display());
    println!(
        "  {}{}",
        sett.display(),
        if kept_settings {
            "  (left as it was)"
        } else {
            ""
        }
    );
    println!("  {}  (rename to post-create to enable)", sample.display());
    if let Some(b) = backup {
        println!(
            "previous {} kept in {}",
            replacing.join(" and "),
            b.display()
        );
    }
    Ok(())
}

/// Text of the files a `.wtree/` offers. Either may be absent; the one that is
/// missing falls back to the template rather than blocking the load.
#[derive(Default)]
struct Seed {
    rules: Option<String>,
    settings: Option<String>,
}

impl Seed {
    fn describe(&self) -> String {
        match (&self.rules, &self.settings) {
            (Some(_), Some(_)) => "rules and settings".into(),
            (Some(_), None) => "rules".into(),
            (None, Some(_)) => "settings".into(),
            (None, None) => "nothing".into(),
        }
    }
}

/// Reads a `.wtree/` and validates every file in it. Nothing is written until
/// this returns `Ok`, so a source with errors leaves the repo untouched.
fn read_seed(dir: &Path) -> Result<Seed, String> {
    let rules_src = dir.join("rules");
    let settings_src = dir.join("settings");
    let seed = Seed {
        rules: read_opt(&rules_src)?,
        settings: read_opt(&settings_src)?,
    };
    if seed.rules.is_none() && seed.settings.is_none() {
        return Err(format!(
            "error: {} has no rules or settings to load",
            dir.display()
        ));
    }
    if let Some(text) = &seed.rules {
        let label = rules_src.display().to_string();
        let loaded = rules::load_str(text, &label);
        for w in &loaded.warnings {
            eprintln!("warning: {w}");
        }
        if !loaded.errors.is_empty() {
            for e in &loaded.errors {
                eprintln!("error: {e}");
            }
            return Err(format!(
                "{label}: {} error(s) — nothing was written",
                loaded.errors.len()
            ));
        }
    }
    if let Some(text) = &seed.settings {
        let label = settings_src.display().to_string();
        warn_if_unportable(&settings::parse(text, &label)?, &label);
    }
    Ok(seed)
}

/// A committed `worktree-dir` only survives the trip to another machine when
/// it is relative. Worth a word, not a refusal — a solo repo can hold whatever
/// its author wants.
fn warn_if_unportable(sett: &Settings, label: &str) {
    if let Some(p) = &sett.worktree_dir {
        if p.is_absolute() {
            eprintln!(
                "warning: worktree-dir is absolute ({}) and will not resolve on another machine ({label})",
                p.display()
            );
        }
    }
}

fn read_opt(path: &Path) -> Result<Option<String>, String> {
    match fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// `.wtree/` under each worktree root, keeping only what `init` could actually
/// load. Pure so the hiding and collapsing rules can be tested without a repo.
fn candidates(here: &Path, main: &Path, usable: &dyn Fn(&Path) -> bool) -> Vec<Candidate> {
    let mut out = Vec::new();
    let mine = here.join(".wtree");
    if usable(&mine) {
        out.push(Candidate {
            path: mine,
            label: HERE,
        });
    }
    // In the main worktree the two roots are the same directory; offering it
    // twice would be a choice between identical paths.
    if main != here {
        let theirs = main.join(".wtree");
        if usable(&theirs) {
            out.push(Candidate {
                path: theirs,
                label: MAIN,
            });
        }
    }
    out
}

fn wtree_candidates(cwd: &Path, repo: &Repo) -> Result<Vec<Candidate>, String> {
    let here = worktree_root(cwd)?;
    let main = primary_root(repo)?;
    Ok(candidates(&here, &main, &|p| {
        SEED.iter().any(|n| p.join(n).is_file())
    }))
}

/// `--load` with no path. Only the worktree you are standing in counts as the
/// obvious answer; anything else is a guess, and a guess here silently installs
/// the wrong policy. Refusals carry the commands that would have worked.
fn resolve_load(cwd: &Path, repo: &Repo) -> Result<PathBuf, String> {
    let cands = wtree_candidates(cwd, repo)?;
    if let [only] = cands.as_slice() {
        if only.label == HERE {
            return Ok(only.path.clone());
        }
    }
    let mut msg = match cands.len() {
        0 => "error: init --load: no .wtree/ in this worktree or the main worktree".to_string(),
        1 => "error: init --load: no .wtree/ in this worktree".to_string(),
        _ => "error: init --load: ambiguous — .wtree/ exists in both worktrees".to_string(),
    };
    for c in &cands {
        msg.push_str(&format!(
            "\n  wtree init --load {}   ({})",
            c.path.display(),
            c.label
        ));
    }
    Err(msg)
}

/// Menu lines for the source prompt, in the order they are offered: the
/// worktree you are standing in, then the main one, then the template.
fn menu(cands: &[Candidate]) -> Vec<String> {
    let mut items: Vec<String> = cands
        .iter()
        .map(|c| format!("{:<14} {}", c.label, c.path.display()))
        .collect();
    items.push("create new rules".to_string());
    items
}

/// Bare `wtree init`. `Ok(None)` means the user backed out; the inner option is
/// the chosen source, `None` being "write the template". Only reached with a
/// terminal on both ends — the caller refuses the mode outright without one.
fn ask(cwd: &Path, repo: &Repo, occupied: &[&str]) -> Result<Option<Option<PathBuf>>, String> {
    if !occupied.is_empty()
        && !prompt::confirm(&format!(
            "{} already has {} — overwrite? (the current one is kept in .backup/)",
            wt_dir(&repo.common_dir).display(),
            occupied.join(" and ")
        ))?
    {
        return Ok(None);
    }
    let cands = wtree_candidates(cwd, repo)?;
    // Nothing to choose between: the template is the only possible answer, and
    // a one-item menu is a question with no question in it.
    if cands.is_empty() {
        return Ok(Some(None));
    }
    match prompt::select("where should the rules come from?", &menu(&cands))? {
        None => Ok(None),
        Some(i) => Ok(Some(cands.get(i).map(|c| c.path.clone()))),
    }
}

/// `20260809T031500Z` — no colons, so it is a usable directory name anywhere.
fn utc_stamp() -> String {
    let t = time::OffsetDateTime::now_utc();
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

/// Moves aside what `init` is about to replace. Only the seed files: `hooks/`
/// is never rewritten, and `.backup/` has to stay out of its own backups or
/// each one would swallow every earlier one.
fn back_up(wt: &Path, occupied: &[&str]) -> Result<PathBuf, String> {
    let root = wt.join(".backup");
    fs::create_dir_all(&root).map_err(|e| format!("cannot create {}: {e}", root.display()))?;
    let dir = root.join(utc_stamp());
    // Not create_dir_all: a second --force inside the same second would reuse
    // the directory and overwrite the backup already sitting in it. Refusing
    // costs a retry a second later; the alternative loses the earlier backup.
    fs::create_dir(&dir).map_err(|e| match e.kind() {
        io::ErrorKind::AlreadyExists => format!(
            "error: {} was backed up less than a second ago — nothing was written, try again",
            wt.display()
        ),
        _ => format!("cannot create {}: {e}", dir.display()),
    })?;
    for name in occupied {
        fs::rename(wt.join(name), dir.join(name))
            .map_err(|e| format!("cannot move {} into {}: {e}", name, dir.display()))?;
    }
    Ok(dir)
}

/// Replaces whatever is there. Callers reach this only after the guard has
/// passed and any previous file has been moved into a backup.
fn write_new(path: &Path, body: &str, mode: u32) -> Result<(), String> {
    fs::write(path, body).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|e| format!("cannot set mode on {}: {e}", path.display()))
}

/// Only fills a gap; `false` when something was already there. Used for the
/// hook sample, which survives every `init` because a hook someone wrote is not
/// this command's to replace, and for the settings template when a load has no
/// settings of its own to put in its place.
fn write_if_absent(path: &Path, body: &str, mode: u32) -> Result<bool, String> {
    let write_error = |e| format!("cannot write {}: {e}", path.display());
    // `create_new` keeps the check and the write in one step: a second `init`
    // racing the first must not overwrite what this is meant to preserve.
    let mut f = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
        Err(e) => return Err(write_error(e)),
    };
    f.write_all(body.as_bytes()).map_err(write_error)?;
    f.set_permissions(fs::Permissions::from_mode(mode))
        .map_err(|e| format!("cannot set mode on {}: {e}", path.display()))?;
    Ok(true)
}

const SETTINGS_TEMPLATE: &str = "\
# Machine-local settings. Unlike the policy rules next to it, this file is not
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
# Files the parent already has belong in the rules' `copy` key; a hook
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
    if let Ok(out) = repo::run_git(
        cwd,
        &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
    ) {
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
            "error: cannot detect a root branch (no origin/HEAD, no main/master, HEAD detached)"
                .into(),
        ),
    }
}

fn template(root: &str) -> String {
    format!(
        "# wtree policy rules — clone-local (never committed); share it by copying.\n\
         # [X] declares a fixed branch; [group:X] a set of fungible work\n\
         # branches. `children` = what may be created here, and what merges back here.\n\
         \n\
         [{root}]\n\
         destroyable = false\n\
         # children = group:work\n\
         # description = release line\n\
         \n\
         # [group:work]\n\
         # ephemeral = true\n\
         # A menu, not a policy — cut the prefixes this repo will not use.\n\
         # name-allow = feat/*, fix/*, refactor/*, perf/*, docs/*, test/*, chore/*\n"
    )
}

// ------------------------------------------------------------------ save ----

/// Copies the clone-local rules out to a `.wtree/` that can be committed, so a
/// teammate's `wtree init --load` starts from the same policy. The reverse of
/// a load, and the only way the two files leave `.git/`.
pub fn save(cwd: &Path, dest: Option<&Path>, force: bool) -> CmdResult {
    let repo = Repo::discover(cwd)?;
    let rules_body = read_opt(&rules_path(&repo.common_dir))?
        .ok_or_else(|| format!("error: no {RULES_LABEL} to save — run `wtree init` first"))?;
    // Rules that do not parse must not be the ones a teammate inherits.
    let loaded = rules::load_str(&rules_body, RULES_LABEL);
    for w in &loaded.warnings {
        eprintln!("warning: {w}");
    }
    if !loaded.errors.is_empty() {
        for e in &loaded.errors {
            eprintln!("error: {e}");
        }
        return Err(format!(
            "{RULES_LABEL}: {} error(s) — nothing was written",
            loaded.errors.len()
        ));
    }
    let settings_file = settings_path(&repo.common_dir);
    let settings_body = read_opt(&settings_file)?;
    if let Some(text) = &settings_body {
        warn_if_unportable(&settings::parse(text, SETTINGS_LABEL)?, SETTINGS_LABEL);
    }

    // Written where you are standing, not at the primary root: the point is to
    // commit it, and the commit belongs to the branch you are on.
    let dir = match dest {
        Some(p) => cwd.join(p),
        None => worktree_root(cwd)?.join(".wtree"),
    };
    let taken: Vec<&str> = SEED.into_iter().filter(|n| dir.join(n).exists()).collect();
    if !taken.is_empty() && !force {
        return Err(format!(
            "error: {} already has {} — pass --force to replace it \
             (git holds the previous version, so nothing is backed up)",
            dir.display(),
            taken.join(" and ")
        ));
    }
    fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    write_new(&dir.join("rules"), &rules_body, 0o644)?;
    match &settings_body {
        Some(text) => write_new(&dir.join("settings"), text, 0o644)?,
        // There is no settings to copy, so a settings from an earlier save has
        // to go: left behind it would keep being loaded alongside rules it no
        // longer belongs to.
        None if taken.contains(&"settings") => {
            let stale = dir.join("settings");
            fs::remove_file(&stale)
                .map_err(|e| format!("cannot remove {}: {e}", stale.display()))?;
        }
        None => {}
    }
    println!("saved to {}", dir.display());
    println!("  rules");
    match &settings_body {
        Some(_) => println!("  settings"),
        None if taken.contains(&"settings") => println!("  settings removed (this clone has none)"),
        None => {}
    }
    println!("commit it to share the policy; `wtree init --load` reads it back");
    Ok(())
}

// ------------------------------------------------------------------- new ----

pub fn new(cwd: &Path, name: &str, group: Option<&str>, dir: Option<&str>) -> CmdResult {
    let repo = Repo::discover(cwd)?;
    let cfg = load_rules(&repo)?;
    let world = repo::gather(cwd, &cfg)?;
    let ctx = Ctx {
        world: &world,
        cfg: &cfg,
        label: RULES_LABEL,
    };
    let plan = ctx.plan_new(name, group).map_err(|r| r.to_string())?;
    let sett = settings::load(&settings_path(&repo.common_dir))?;
    let root = primary_root(&repo)?;
    let dest = match dir {
        Some(d) => worktree_dest_in(&root, &sett, d)?,
        None => worktree_dest(&root, &sett, name)?,
    };
    if dest.exists() {
        let mut msg = occupied(&ctx, &dest);
        if occupant(&ctx, &dest).is_some() {
            msg.push_str(&format!(
                "\n  or put it elsewhere: wtree new {name} --dir <name>"
            ));
        }
        return Err(msg);
    }

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

    if let Some(base) = dest.parent() {
        fs::create_dir_all(base).map_err(|e| format!("cannot create {}: {e}", base.display()))?;
    }
    let dest_str = dest
        .to_str()
        .ok_or_else(|| "destination path is not valid UTF-8".to_string())?;

    // New branch forked at the parent worktree's current HEAD (= cwd's HEAD).
    repo::run_git(
        cwd,
        &["worktree", "add", "-q", "-b", name, dest_str, "HEAD"],
    )?;

    let identity = match &kind {
        None => "fixed".to_string(),
        Some(k) => k.to_string(),
    };
    if let Some(kind) = kind {
        let record = State {
            branch: name.to_string(),
            kind,
            parent: parent.clone(),
        };
        let written = repo::private_git_dir(&dest)
            .and_then(|d| state::write(&d, &record).map_err(|e| e.to_string()));
        if let Err(e) = written {
            // Roll back so no unmanaged residue is left behind.
            let mut msg = format!(
                "error: failed to record state: {e}\nthe worktree and branch were rolled back"
            );
            if let Err(e2) = repo::run_git(cwd, &["worktree", "remove", "--force", dest_str]) {
                msg.push_str(&format!(
                    "\nwarning: rollback (worktree remove) failed: {e2}"
                ));
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
// `Rules::copy_list` is the policy and this only carries it out.

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
        // A symlink is not a directory here even when it points at one: it is
        // recreated as a link, so it drags in no tree.
        let ft = entry.file_type().ok();
        let is_dir = ft.is_some_and(|t| t.is_dir());
        if !patterns.iter().any(|p| p.matches(&name, is_dir)) {
            // Both near misses: the trailing slash is there and shouldn't be, or
            // it isn't and should be. Either way the rule looks like it applies.
            let named = |dir_only: bool| {
                patterns
                    .iter()
                    .any(|p| p.dir_only == dir_only && rules::glob_match(&p.glob, &name))
            };
            if is_dir && named(false) {
                notes.push(format!(
                    "skipped '{name}': a directory needs a trailing '/'"
                ));
            } else if ft.is_some_and(|t| t.is_symlink()) && named(true) {
                notes.push(format!(
                    "skipped '{name}': a symlink crosses as a link — drop the '/'"
                ));
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
        match copy_entry(&entry.path(), &to) {
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

/// Symlinks are recreated, not followed — the entry named by a pattern as much
/// as anything under it. A copied dependency tree is the case this matters for:
/// pnpm's `node_modules` links packages to each other, and dereferencing those
/// both explodes the copy and dies on the cycles circular dependencies produce.
fn copy_entry(from: &Path, to: &Path) -> io::Result<()> {
    let ft = fs::symlink_metadata(from)?.file_type();
    if ft.is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(from)?, to)
    } else if ft.is_dir() {
        copy_tree(from, to)
    } else {
        fs::copy(from, to).map(|_| ())
    }
}

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        copy_entry(&entry.path(), &to.join(entry.file_name()))?;
    }
    Ok(())
}

// ------------------------------------------------------------------ open ----
//
// Give an existing branch a worktree — the inverse of close, and what `new`
// cannot do because it always forks a new branch. Nothing is recorded: a fixed
// branch's identity is its rules declaration, and any other branch stays
// unknown until the user adopts it from the worktree this creates.

pub fn open(cwd: &Path, branch: &str) -> CmdResult {
    let repo = Repo::discover(cwd)?;
    let cfg = load_rules(&repo)?;
    let world = repo::gather(cwd, &cfg)?;
    let ctx = Ctx {
        world: &world,
        cfg: &cfg,
        label: RULES_LABEL,
    };
    let plan = ctx.plan_open(branch).map_err(|r| r.to_string())?;

    // Only a declared branch has a parent to copy from: anything else stays
    // unknown until it is adopted there, and an unknown branch has no section
    // whose `copy` list could be read.
    let carry = plan
        .fixed
        .then(|| {
            ctx.parent_of(&Identity::Fixed {
                branch: plan.branch.clone(),
            })
        })
        .flatten()
        .map(|(p, _)| (p, cfg.copy_list(SectionKind::Branch, &plan.branch)));

    let sett = settings::load(&settings_path(&repo.common_dir))?;
    let root = primary_root(&repo)?;
    let dest = worktree_dest(&root, &sett, &plan.branch)?;
    if dest.exists() {
        return Err(occupied(&ctx, &dest));
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
/// What is sitting at a destination, named the way `list` names it. `None` when
/// git knows nothing about the path: a plain directory is merely in the way,
/// and there is nothing to say about it beyond that it is there.
fn occupant(ctx: &Ctx, dest: &Path) -> Option<String> {
    let wt = ctx.world.facts.iter().find(|f| f.path == dest)?;
    let what = match (&wt.head, &wt.head_short) {
        (Some(h), _) => format!("branch '{h}'"),
        (None, Some(o)) => format!("a detached worktree ({o})"),
        (None, None) => "a detached worktree".to_string(),
    };
    Some(match ctx.identity_of(wt) {
        // Being outside the policy is worth saying — it is why this one is not
        // in the tree the user just looked at — but it changes nothing here.
        Identity::Unknown { .. } => format!("{what} (unmanaged)"),
        _ => what,
    })
}

/// `<verb>: destination ... already exists`, with the occupant named when there
/// is one. Two branch names can fold onto one directory (`fix/xyz` and
/// `fix-xyz` both want `fix-xyz`), and the bare path said nothing about which
/// of the three possible reasons had been hit.
fn occupied(ctx: &Ctx, dest: &Path) -> String {
    let mut msg = format!("error: destination {} already exists", dest.display());
    if let Some(o) = occupant(ctx, dest) {
        msg.push_str(&format!("\n  conflict: {o} occupies it"));
    }
    msg
}

fn worktree_dest(root: &Path, sett: &Settings, branch: &str) -> Result<PathBuf, String> {
    worktree_dest_in(root, sett, &branch.replace('/', "-"))
}

/// `leaf` is a directory name, never a path: the policy owns where worktrees
/// live and `--dir` only relabels one of them.
fn worktree_dest_in(root: &Path, sett: &Settings, leaf: &str) -> Result<PathBuf, String> {
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
    Ok(normalize(&base.join(leaf)))
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
        Err(e) => {
            eprintln!("warning: cannot run post-create hook: {e}; the worktree was still created")
        }
    }
}

// ------------------------------------------------------------------ list ----

/// Commits each of `branch` and `parent` holds that the other does not — the
/// signal that `merge` or `sync` is due. One `--left-right` walk answers both
/// directions, so a row costs one process rather than two.
///
/// Display only, and deliberately a different question from the one
/// `Repo::has_unreflected_commits` answers: this counts commits by ancestry,
/// that judges whether the content is already in the parent. A branch can be
/// several commits ahead with nothing left to lose. `destroy` is what needs
/// the second answer, and it gathers the fact for itself.
///
/// `None` when the walk cannot be taken, which is the case for a fixed branch
/// whose rules-derived parent has not been created yet.
fn divergence(dir: &Path, branch: &str, parent: &str) -> Option<(u32, u32)> {
    let range = format!("{parent}...{branch}");
    let out = repo::run_git(dir, &["rev-list", "--left-right", "--count", &range]).ok()?;
    let mut counts = out.split_whitespace();
    // `--left-right` orders the pair by the range: left of `...` first.
    let behind = counts.next()?.parse().ok()?;
    let ahead = counts.next()?.parse().ok()?;
    Some((ahead, behind))
}

/// `  ↑3 ↓1` for a branch three commits past its parent and one behind it.
/// Empty when the two are level, so a settled tree carries no noise.
fn divergence_text(g: &Glyphs, d: Option<(u32, u32)>) -> String {
    let Some((ahead, behind)) = d else {
        return String::new();
    };
    let mut parts = Vec::new();
    if ahead > 0 {
        parts.push(style(format!("{}{ahead}", g.ahead)).green().to_string());
    }
    if behind > 0 {
        parts.push(style(format!("{}{behind}", g.behind)).yellow().to_string());
    }
    if parts.is_empty() {
        String::new()
    } else {
        // No leading gap: this is a column now, and the caller spaces it.
        parts.join(" ")
    }
}

/// Tree glyphs: box-drawing on a TTY, ASCII through a pipe so redirected output
/// stays greppable. `mid`/`last` must share a display width with `bar`/`gap`,
/// or every level of descent shifts the subtree by the difference.
struct Glyphs {
    mid: &'static str,
    last: &'static str,
    bar: &'static str,
    gap: &'static str,
    /// Marker for a branch that has no worktree.
    bare: &'static str,
    /// Commits this branch holds that its parent does not, and the reverse.
    ahead: &'static str,
    behind: &'static str,
}

const TREE_PRETTY: Glyphs = Glyphs {
    mid: "├─",
    last: "└─",
    bar: "│ ",
    gap: "  ",
    bare: "·",
    ahead: "↑",
    behind: "↓",
};
const TREE_PLAIN: Glyphs = Glyphs {
    mid: "|-",
    last: "`-",
    bar: "| ",
    gap: "  ",
    bare: ".",
    ahead: "^",
    behind: "v",
};

/// Terminal columns `s` occupies. Branch names and worktree directory names can
/// hold CJK, which is two columns wide; sizing the branch column by `chars()`
/// would leave everything right of such a name a column short per wide char.
///
/// Rows carry colour by the time they are measured, so this has to skip the
/// escape sequences too — counting them as the seven-odd columns their bytes
/// look like shrinks every row and cuts the divergence counts off the end.
fn display_width(s: &str) -> usize {
    console::measure_text_width(s)
}

/// Columns the terminal on stdout has, `None` when stdout is not one or the
/// kernel will not say. A row longer than this wraps, and a wrapped row puts
/// its tail under the next row's gutter — the tree stops being one.
fn terminal_width() -> Option<usize> {
    if !io::stdout().is_terminal() {
        return None;
    }
    // SAFETY: `ws` is a fully-owned `winsize` for the kernel to fill in, and
    // `TIOCGWINSZ` writes nothing else. A non-zero return leaves it untouched,
    // which is why the result is checked before the field is read.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
    (rc == 0 && ws.ws_col > 0).then_some(ws.ws_col as usize)
}

/// Cut `s` to `width` columns, spending the last one on `…` so the cut is
/// visible. Returns `s` whole when it already fits.
fn truncate_to(s: &str, width: usize) -> String {
    // `console::truncate_str` spends the tail even when there is no room for
    // it, handing back a 1-wide `…` for a 0-wide budget.
    if width == 0 {
        return String::new();
    }
    console::truncate_str(s, width, "…").into_owned()
}

/// Floor for the branch column, so a narrow terminal does not shave names down
/// to nothing in the name of leaving room for what follows them.
const MIN_NAME_COL: usize = 24;

/// One branch of the tree. Parentage is resolved by name in a second pass, so
/// the node itself only carries what gets printed.
struct Node {
    branch: String,
    /// Declared fixed branches sort above the group/free churn beneath them.
    fixed: bool,
    /// The three states `git branch` marks, spelled its way: `*` the worktree
    /// one stands in, `+` one checked out elsewhere, `Glyphs::bare` for a branch
    /// no worktree claims. git leaves that last one blank; a tree row cannot,
    /// or the slot reads as something cut off rather than something absent.
    mark: &'static str,
    /// What is printed right of the branch name, kept apart so each column can
    /// be sized across every row. Run together, a row missing its directory
    /// slides its divergence under the directories above it and the eye reads
    /// the wrong column.
    identity: String,
    /// Empty for a branch no worktree has checked out.
    dir: String,
    /// Already carries its own colour, so measure it rather than count bytes.
    div: String,
    flags: String,
    kids: Vec<usize>,
}

/// A worktree or branch with no identity, and so no parent to hang it from.
struct Stray {
    /// Which pass turned it up. Both kinds are unmanaged, but only one is a
    /// checkout sitting in a directory, and the summary says which you have.
    worktree: bool,
    /// The whole row. No `UNKNOWN` column and no reason beneath it: the
    /// heading above says both. Why a particular one could not be judged is
    /// `wtree info`'s answer, standing in the worktree it is asking about.
    line: String,
}

fn plural(k: usize, one: &'static str, many: &'static str) -> &'static str {
    if k == 1 { one } else { many }
}

/// `found 2 unmanaged: 1 worktree, 1 branch`. The breakdown earns its colon
/// only when both kinds are present; with one kind the noun rides along with
/// the count instead of being repeated after it.
fn unmanaged_summary(strays: &[Stray]) -> String {
    let n = strays.len();
    let wt = strays.iter().filter(|s| s.worktree).count();
    let br = n - wt;
    if wt > 0 && br > 0 {
        format!(
            "found {n} unmanaged: {wt} {}, {br} {}",
            plural(wt, "worktree", "worktrees"),
            plural(br, "branch", "branches")
        )
    } else if wt > 0 {
        format!("found {wt} unmanaged {}", plural(wt, "worktree", "worktrees"))
    } else {
        format!("found {br} unmanaged {}", plural(br, "branch", "branches"))
    }
}

/// How much of the unmanaged block `list` prints. Knowing how many there are
/// is a glance; knowing which ones is a second question. The heading over
/// them carries the step either way.
#[derive(Clone, Copy)]
pub enum UnmanagedView {
    Count,
    Entries,
}

pub fn list(cwd: &Path, view: UnmanagedView) -> CmdResult {
    let repo = Repo::discover(cwd)?;
    let cfg = load_rules(&repo)?;
    // Light: nothing here judges work loss, and the reflected-in-parent probe
    // is the most expensive fact gather takes. `↑`/`↓` answer the neighbouring
    // question — how far apart — from a single ancestry walk per row.
    let world = repo::gather_light(cwd, &cfg)?;
    let ctx = Ctx {
        world: &world,
        cfg: &cfg,
        label: RULES_LABEL,
    };
    let g = if io::stdout().is_terminal() { &TREE_PRETTY } else { &TREE_PLAIN };

    let mut nodes: Vec<Node> = Vec::new();
    let mut parent_of: Vec<Option<String>> = Vec::new();
    let mut by_branch: BTreeMap<String, usize> = BTreeMap::new();
    let mut strays: Vec<Stray> = Vec::new();
    // Every branch a worktree has checked out, judged or not. A worktree that
    // came out unmanaged still owns its branch, so the branch pass must not
    // list it a second time.
    let mut claimed: BTreeSet<String> = BTreeSet::new();

    // Worktrees first: a worktree's record outranks a declaration (the
    // grandfather rule `branch_identity` encodes), so the branches they claim
    // are the ones the branch pass below skips.
    for (i, wt) in world.facts.iter().enumerate() {
        if let Some(h) = &wt.head {
            claimed.insert(h.clone());
        }
        let dir = wt
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| wt.path.display().to_string());
        let mut flags = String::new();
        if wt.dirty {
            // Yellow like the `behind` arrow: both say the worktree is carrying
            // something its parent has not seen.
            flags.push_str(&format!(" {}", style("[dirty]").yellow()));
        }
        let id = ctx.identity_of(wt);
        if let Identity::Unknown { .. } = &id {
            // The path in full, as `git worktree list` gives it: an unmanaged
            // checkout sits wherever it was made, so the name alone will not
            // get anyone there. A detached one is called by its commit, having
            // no branch to be called by.
            // The path leads there and the name says which one it is, so the
            // name keeps the full brightness and the path steps back a shade.
            let what = match (&wt.head, &wt.head_short) {
                (Some(h), _) => h.clone(),
                (None, Some(o)) => format!("{o}{}", style(" (detached)").dim()),
                (None, None) => style("(detached)").dim().to_string(),
            };
            strays.push(Stray {
                worktree: true,
                line: format!("{}  {what}{flags}", style(wt.path.display()).dim()),
            });
            continue;
        }
        let branch = id.branch().expect("a judged identity names its branch").to_string();
        let (parent, div) = match ctx.parent_of(&id) {
            Some((p, _)) => {
                let d = divergence_text(g, divergence(&repo.common_dir, &branch, &p));
                (Some(p), d)
            }
            None => (None, String::new()),
        };
        by_branch.insert(branch.clone(), nodes.len());
        parent_of.push(parent);
        nodes.push(Node {
            branch,
            fixed: matches!(id, Identity::Fixed { .. }),
            mark: if i == world.current { "*" } else { "+" },
            identity: describe_identity(&id),
            dir: dir.clone(),
            div: div.clone(),
            flags: flags.trim_start().to_string(),
            kids: Vec::new(),
        });
    }

    for b in &world.branches {
        if claimed.contains(b) {
            continue;
        }
        let id = ctx.branch_identity(b);
        if let Identity::Unknown { .. } = &id {
            strays.push(Stray {
                worktree: false,
                line: b.clone(),
            });
            continue;
        }
        let parent = ctx.parent_of(&id).map(|(p, _)| p);
        // A branch with no worktree still has a parent to be measured against,
        // and being the one nobody has opened is exactly when how far it has
        // drifted is worth knowing.
        let div = parent
            .as_deref()
            .map(|p| divergence_text(g, divergence(&repo.common_dir, b, p)))
            .unwrap_or_default();
        by_branch.insert(b.clone(), nodes.len());
        parent_of.push(parent);
        nodes.push(Node {
            branch: b.clone(),
            fixed: matches!(id, Identity::Fixed { .. }),
            mark: g.bare,
            identity: describe_identity(&id),
            dir: String::new(),
            div: div.clone(),
            flags: String::new(),
            kids: Vec::new(),
        });
    }

    // A node whose recorded parent is absent — or is itself — becomes a root
    // rather than vanishing: the tree shows every branch it was given.
    let mut roots: Vec<usize> = Vec::new();
    for (i, parent) in parent_of.iter().enumerate() {
        match parent.as_ref().and_then(|p| by_branch.get(p)) {
            Some(&p) if p != i => nodes[p].kids.push(i),
            _ => roots.push(i),
        }
    }
    let key = |n: &Node| (!n.fixed, n.branch.clone());
    roots.sort_by_key(|&i| key(&nodes[i]));
    for i in 0..nodes.len() {
        let mut kids = std::mem::take(&mut nodes[i].kids);
        kids.sort_by_key(|&j| key(&nodes[j]));
        nodes[i].kids = kids;
    }

    // Flatten depth-first into (gutter, node) so the branch column can be
    // measured in one pass and padded in the next — the gutter's width varies
    // by depth, so the two cannot be done together.
    let mut rows: Vec<(String, usize)> = Vec::new();
    let mut seen = vec![false; nodes.len()];
    let mut stack: Vec<(usize, String, String)> = roots
        .iter()
        .rev()
        .map(|&r| (r, String::new(), String::new()))
        .collect();
    while let Some((i, gutter, prefix)) = stack.pop() {
        if seen[i] {
            continue;
        }
        seen[i] = true;
        rows.push((gutter, i));
        for (n, &k) in nodes[i].kids.iter().enumerate().rev() {
            let last = n == nodes[i].kids.len() - 1;
            stack.push((
                k,
                format!("{prefix}{}", if last { g.last } else { g.mid }),
                format!("{prefix}{}", if last { g.gap } else { g.bar }),
            ));
        }
    }
    // Recorded parents that form a cycle leave their nodes unreachable from any
    // root. Print them flat rather than drop them.
    for (i, visited) in seen.iter().enumerate() {
        if !visited {
            rows.push((String::new(), i));
        }
    }

    let head_of = |(gutter, i): &(String, usize)| {
        let n: &Node = &nodes[*i];
        // `git branch` already colours these three states, and its readers are
        // ours: the branch one stands in green, one checked out in another
        // worktree cyan, one no worktree claims left in the default colour.
        let name = match n.mark {
            "*" => style(&n.branch).green().to_string(),
            "+" => style(&n.branch).cyan().to_string(),
            _ => n.branch.clone(),
        };
        format!("{gutter}{} {name}", n.mark)
    };
    let natural = rows.iter().map(|r| display_width(&head_of(r))).max().unwrap_or(0);
    // Piped output is never cut: a reader that is not a terminal is grepping or
    // diffing, and half a branch name serves neither. On a terminal, one long
    // name would otherwise push every row's tail past the right edge, so the
    // names give way first and only past half the screen.
    let term = terminal_width();
    let name_w = match term {
        Some(t) => natural.min((t / 2).max(MIN_NAME_COL)),
        None => natural,
    };
    // The tail columns are sized the same way, so a row with no directory
    // leaves the gap open instead of pulling its divergence left.
    let col_w = |f: fn(&Node) -> &String| -> usize {
        rows.iter()
            .map(|r| display_width(f(&nodes[r.1])))
            .max()
            .unwrap_or(0)
    };
    let id_w = col_w(|n| &n.identity);
    let dir_w = col_w(|n| &n.dir);
    let div_w = col_w(|n| &n.div);
    let pad_to = |s: &str, w: usize| " ".repeat(w.saturating_sub(display_width(s)));
    // `--unmanaged` is its own screen, not an addition to this one. Left out,
    // the block below is free of the tree's column budget — an absolute path
    // has room to be printed whole.
    for r in rows.iter().take(match view {
        UnmanagedView::Count => rows.len(),
        UnmanagedView::Entries => 0,
    }) {
        let n = &nodes[r.1];
        let head = truncate_to(&head_of(r), name_w);
        let line = format!(
            "{head}{}  {}{}  {}{}  {}{}  {}",
            pad_to(&head, name_w),
            style(&n.identity).dim(),
            pad_to(&n.identity, id_w),
            style(&n.dir).dim(),
            pad_to(&n.dir, dir_w),
            n.div,
            pad_to(&n.div, div_w),
            n.flags,
        );
        let line = line.trim_end();
        match term {
            Some(t) => println!("{}", truncate_to(line, t)),
            None => println!("{line}"),
        }
    }

    if !strays.is_empty() {
        // What the policy does not cover is the one thing on screen that is not
        // wtree's to arrange, so it recedes rather than competing with the tree.
        if matches!(view, UnmanagedView::Count) {
            println!(
                "\n{}",
                style(format!(
                    "{}. See 'wtree list --unmanaged'.",
                    unmanaged_summary(&strays)
                ))
                .dim()
            );
        }
        // Listed apart by kind: a checkout left on disk and a branch nobody
        // opened want different things done to them, and a heading says which
        // without the reader pairing every row against its recovery line.
        // The recovery rides on the heading: every row under one takes the same
        // step, and repeating it per row said the same thing five times over.
        // Which rows that step actually works on is the gate's call, not this
        // listing's — `adopt` refuses a detached HEAD with its own reason.
        let mut first = matches!(view, UnmanagedView::Entries);
        for (kind, label, how) in [
            (true, "worktree", judge::ADOPT_HINT),
            (
                false,
                "branch",
                // Two commands with prose after the second: quoted, so `there`
                // reads as the place to stand and not as an argument.
                "recover with: 'wtree open <branch>', then 'wtree adopt' there",
            ),
        ] {
            if matches!(view, UnmanagedView::Count) {
                break;
            }
            let group: Vec<&Stray> = strays.iter().filter(|s| s.worktree == kind).collect();
            if group.is_empty() {
                continue;
            }
            if !first {
                println!();
            }
            first = false;
            // Asked for by name, this screen is nobody's appendix: what recedes
            // here is the hint, read once, and not the rows that were the point
            // of typing it.
            //
            // The heading takes magenta rather than weight. Bold made it
            // brighter than the rows it introduces, and against a plain branch
            // name — the whole of a branch row — weight alone barely reads.
            // Magenta is the one colour no row uses: green, cyan and yellow all
            // mean something about a branch, and scaffolding should not borrow
            // a word from the data.
            println!(
                "{}  {}",
                style(format!("[unmanaged {label}]")).magenta(),
                style(format!("({how})")).dim()
            );
            for s in group {
                println!("{}", s.line);
            }
        }
    }
    Ok(())
}

/// What the policy says a branch is. Not what the policy will *do* with it:
/// `ephemeral` is a property of the group, identical on every row under it, and
/// only `destroy` ever consults it. `wtree info` answers that question.
fn describe_identity(id: &Identity) -> String {
    match id {
        Identity::Fixed { .. } => "fixed".into(),
        Identity::Free { .. } => "free".into(),
        Identity::GroupMember { group, .. } => format!("group:{group}"),
        Identity::Unknown { .. } => "UNKNOWN".into(),
    }
}

// ------------------------------------------------------------------ info ----

pub fn info(cwd: &Path) -> CmdResult {
    let repo = Repo::discover(cwd)?;
    let cfg = load_rules(&repo)?;
    let world = repo::gather(cwd, &cfg)?;
    let ctx = Ctx {
        world: &world,
        cfg: &cfg,
        label: RULES_LABEL,
    };
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
            "save", "adopt",
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
            println!("identity: fixed — declared [{branch}] ({RULES_LABEL}:{line})");
        }
        Identity::GroupMember { group, .. } => {
            println!("identity: group:{group} — state record in this worktree's private git dir")
        }
        Identity::Free { .. } => {
            println!("identity: free — state record in this worktree's private git dir")
        }
        Identity::Unknown { .. } => unreachable!("handled above"),
    }
    if let Some(d) = description_of(&ctx, &id) {
        println!("description: {d}");
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
        Ok(p) => println!(
            "  merge: '{}' -> '{}' (--{})",
            p.source,
            p.target,
            p.mode.as_str()
        ),
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

/// The contextual menu — `wtree` with no arguments. Lists the verbs that would
/// get past policy here and nothing else, so the screen is a truthful answer to
/// "what can I do from this worktree?".
///
/// `help` and `help --all` still reach this and the manual, undocumented: the
/// word is what a lost user types first, and turning that into an error to
/// teach them a shorter spelling is a poor trade. Nothing points at them, so
/// there is one name on screen for each of the two screens.
///
/// Only the shape of each invocation appears. The data a verb operates on (the
/// branches `open` would take, the names `new` accepts) belongs to that verb's
/// own no-argument screen, which keeps this one from moving every time a
/// branch comes or goes. `--key` and `--force` are likewise absent: they are
/// guards that appear when something is off, and the refusal introduces them
/// with the situation that called for them.
pub fn help(cwd: &Path) -> CmdResult {
    let repo = Repo::discover(cwd)?;
    if !rules_path(&repo.common_dir).exists() {
        println!("this repo has no wtree policy yet\n");
        println!("  init                      ask where {RULES_LABEL} should come from");
        println!("  init --new                write a starter one");
        println!("  init --load [path]        take one from a .wtree/");
        return Ok(());
    }
    let cfg = load_rules(&repo)?;
    let world = repo::gather(cwd, &cfg)?;
    let ctx = Ctx {
        world: &world,
        cfg: &cfg,
        label: RULES_LABEL,
    };

    let head = world
        .current()
        .head
        .clone()
        .unwrap_or_else(|| "(detached)".into());
    let id = ctx.current_identity();
    println!("{head} ({})", identity_word(&id));
    // Only the current branch's: one per screen, or the line that applies
    // gets buried under the groups `new` could create here.
    if let Some(d) = description_of(&ctx, &id) {
        println!("  {d}");
    }
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
    rows.push((
        "info".to_string(),
        "rules and previews for this worktree".to_string(),
    ));
    rows.push((
        "save".to_string(),
        "copy the rules out to a .wtree/".to_string(),
    ));

    let width = rows.iter().map(|(u, _)| u.len()).max().unwrap_or(0);
    for (usage, note) in &rows {
        println!("  {usage:width$}  {note}");
    }
    println!("\nwtree -h for every verb, whether or not it applies here");
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

/// The `description` of the section declaring `id`. Free and unknown branches
/// are declared nowhere, so they never have one.
fn description_of<'a>(ctx: &Ctx<'a>, id: &Identity) -> Option<&'a str> {
    let (kind, name) = match id {
        Identity::Fixed { branch } => (SectionKind::Branch, branch),
        Identity::GroupMember { group, .. } => (SectionKind::Group, group),
        Identity::Free { .. } | Identity::Unknown { .. } => return None,
    };
    ctx.cfg.get(kind, name, "description")
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
        let note = if c.fixed {
            ""
        } else {
            "   (unmanaged until adopted)"
        };
        eprintln!("  {}{note}", c.branch);
    }
}

/// Gather the world and hand a `Ctx` to `f`. Used by the no-argument usage
/// screens, which have nothing to report if the repo or rules are unusable —
/// the usage line alone is still the answer.
fn with_ctx<T>(cwd: &Path, f: impl FnOnce(&Ctx) -> judge::Decision<T>) -> Result<T, String> {
    let repo = Repo::discover(cwd)?;
    let cfg = load_rules(&repo)?;
    let world = repo::gather(cwd, &cfg)?;
    let ctx = Ctx {
        world: &world,
        cfg: &cfg,
        label: RULES_LABEL,
    };
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
    let cfg = load_rules(&repo)?;
    let world = repo::gather(cwd, &cfg)?;
    let ctx = Ctx {
        world: &world,
        cfg: &cfg,
        label: RULES_LABEL,
    };
    let plan = ctx
        .plan_adopt(group, free, parent)
        .map_err(|r| r.to_string())?;

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
            "error: cannot write the state record in {}: {e}",
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
    let cfg = load_rules(&repo)?;
    let world = repo::gather(cwd, &cfg)?;
    let ctx = Ctx {
        world: &world,
        cfg: &cfg,
        label: RULES_LABEL,
    };
    let plan = ctx.plan_merge(mode_flag).map_err(|r| r.to_string())?;
    let commit_msg = check_message(plan.mode, msg)?;

    // Nothing to land is not a failure of any git step, so it is checked
    // before running any of them. Under `merge` it is a refusal; `land` treats
    // it as "the cleanup is the whole remaining job" instead.
    if !has_changes_to_merge(&world, &plan)? {
        return Err(format!(
            "error: nothing to merge: '{}' adds no changes relative to '{}'",
            plan.source, plan.target
        ));
    }
    run_merge(&world, &plan, commit_msg, false)
}

/// The message rule depends on the judged mode (the flag may be omitted when
/// the policy allows a single mode), so it is checked after the plan. A
/// superfluous -m is refused rather than ignored: a message that silently goes
/// nowhere leaves the caller believing they named the work.
fn check_message(mode: MergeMode, msg: Option<&str>) -> Result<Option<&str>, String> {
    match mode {
        MergeMode::Squash | MergeMode::NoFf => match msg {
            Some(m) => Ok(Some(m)),
            None => Err(format!(
                "error: --{} creates a new commit; -m <message> is required",
                mode.as_str()
            )),
        },
        MergeMode::Rebase | MergeMode::Ff => {
            if msg.is_some() {
                return Err(format!(
                    "error: --{} keeps each commit as-is; -m has nothing to name",
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
        &[
            "diff",
            "--quiet",
            &format!("{}...{}", plan.target, plan.source),
            "--",
        ],
    )?;
    match code {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(format!("error: git diff failed: {}", derr.trim())),
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
                "error: cannot fast-forward: '{target}' has commits that '{branch}' lacks\n  run `wtree sync`, then retry\n  nothing was changed"
            ));
        }
        ff_target(&wt, target_wt.as_deref(), &target, &branch, verb)
            .map_err(|e| format!("error: {e}; nothing was changed"))?;
        println!(
            "fast-forwarded '{target}' to '{branch}' ({})",
            short_head(&wt)
        );
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
            return Err(format!("error: {e}; nothing was changed"));
        }
    };
    let fail = |m: String| bail(&wt, &orig_head, stashed, format!("error: {m}"));

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
                fail(format!(
                    "commit failed (hook or signing?): {e}; nothing merged"
                ))
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
                &[
                    "commit-tree",
                    &merged_tree,
                    "-p",
                    &target_oid,
                    "-p",
                    &branch,
                    "-m",
                    m,
                ],
            )
            .map_err(|e| {
                fail(format!(
                    "could not create the merge commit: {e}; nothing merged"
                ))
            })?
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
    let ncommits = repo::run_git(
        &wt,
        &["rev-list", "--count", &format!("{target}..{branch}")],
    )
    .map_err(|e| fail(format!("{e}; nothing merged")))?
    .trim()
    .to_string();

    // Step 4 — fast-forward the target.
    ff_target(&wt, target_wt.as_deref(), &target, &branch, verb)
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
        println!(
            "worktree kept @ {}; clean up with: wtree destroy",
            wt.display()
        );
    }
}

// ------------------------------------------------------------------ sync ----
//
// Bring the recorded parent into the current branch — a true merge, never a
// squash (a squashed sync would repeat the same conflicts on every following
// sync — DESIGN). Same conflict precheck and stash round-trip as merge.

pub fn sync(cwd: &Path) -> CmdResult {
    let repo = Repo::discover(cwd)?;
    let cfg = load_rules(&repo)?;
    let world = repo::gather(cwd, &cfg)?;
    let ctx = Ctx {
        world: &world,
        cfg: &cfg,
        label: RULES_LABEL,
    };
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
        &branch,
        &parent,
        &format!("merging '{parent}' into '{branch}'"),
        &format!(
            "resolve by hand:  git merge {parent}   (resolve, commit) — that completes the sync"
        ),
    )?;

    let stashed = stash_push(&wt, "sync", &branch)?;
    if let Err(e) = repo::run_git(&wt, &["merge", "-q", "--no-edit", &parent]) {
        let _ = repo::run_git(&wt, &["merge", "--abort"]);
        stash_pop(&wt, stashed);
        return Err(format!(
            "error: merging '{parent}' failed ('{parent}' moved since the precheck?): {e}\n  the merge was aborted; nothing was changed"
        ));
    }
    stash_pop(&wt, stashed);
    println!(
        "synced '{branch}' with '{parent}' (now {})",
        short_head(&wt)
    );
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
    let cfg = load_rules(&repo)?;
    let world = repo::gather(cwd, &cfg)?;
    let ctx = Ctx {
        world: &world,
        cfg: &cfg,
        label: RULES_LABEL,
    };
    let plan = ctx.plan_destroy(force, key).map_err(|r| r.to_string())?;
    let targets = resolve_targets(&world, &plan)?;
    execute_destroy(&primary_root(&repo)?, &targets, "")
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
                    format!("error: '{b}' has no worktree to remove; nothing was changed")
                })
        })
        .collect()
}

/// Remove each target in order. A failure stops the run and names exactly how
/// far it got — under `land` the removals sit after a merge that already
/// succeeded, and git's own error says nothing about that.
fn execute_destroy(root: &Path, targets: &[Target], note: &str) -> CmdResult {
    for (i, t) in targets.iter().enumerate() {
        let Err(e) = remove_one(root, t) else {
            continue;
        };
        let mut msg = format!("error: {e}{note}");
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
    let cfg = load_rules(&repo)?;
    let world = repo::gather(cwd, &cfg)?;
    let ctx = Ctx {
        world: &world,
        cfg: &cfg,
        label: RULES_LABEL,
    };
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
    repo::run_git(&root, &args).map_err(|e| format!("error: {e}\n  nothing was removed"))?;
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
    let cfg = load_rules(&repo)?;
    let world = repo::gather(cwd, &cfg)?;
    let ctx = Ctx {
        world: &world,
        cfg: &cfg,
        label: RULES_LABEL,
    };

    // ---- preflight: both halves judged while nothing has happened yet ----
    let plan = ctx.plan_merge(mode_flag).map_err(as_land)?;
    let commit_msg = check_message(plan.mode, msg)?;
    // Uncommitted work is the one work-loss cause the merge cannot resolve: it
    // is stashed and put back, and the destroy would then have to throw it
    // away. Refused up front, as in wtree.sh, so land does both or neither.
    if world.current().dirty {
        return Err(format!(
            "error: '{}' has uncommitted changes, which land would have to leave behind\n  commit them, or run `wtree merge` and then `wtree destroy`",
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
        println!(
            "nothing to merge onto '{}'; going straight to destroy",
            plan.target
        );
    }

    // The merge moved this branch, so the destroy is carried out on freshly
    // gathered facts rather than the preflight's.
    let note = if merged { LAND_NOTE } else { "" };
    let world = repo::gather(cwd, &cfg)?;
    let ctx = Ctx {
        world: &world,
        cfg: &cfg,
        label: RULES_LABEL,
    };
    let plan =
        plan_destroy_for_land(&ctx).map_err(|r| format!("{}{note}", as_land(r).trim_end()))?;
    let targets = resolve_targets(&world, &plan)?;
    execute_destroy(&primary_root(&repo)?, &targets, note)
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
            let list = if files.is_empty() {
                "?".to_string()
            } else {
                files.join(", ")
            };
            Err(format!(
                "error: {doing} would conflict in: {list}\n  {hint}\n  nothing was changed"
            ))
        }
        _ => Err(format!(
            "error: git merge-tree failed (git >= 2.38 required): {}",
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
    repo::run_git(
        wt,
        &[
            "stash",
            "push",
            "-u",
            "-q",
            "-m",
            // The stash's own name, not a diagnostic: `git stash list` shows
            // this, so it says which verb parked the work.
            &format!("wtree {verb}: {branch}"),
        ],
    )
    .map_err(|e| {
        format!("error: could not stash uncommitted changes: {e}\n  nothing was changed")
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
    eprintln!(
        "  they are kept in the stash:  git -C {} stash list",
        wt.display()
    );
    eprintln!(
        "  recover with:                git -C {} stash pop --index",
        wt.display()
    );
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
    repo::run_git(wt, &["rebase", "-q", target])
        .map(drop)
        .map_err(|_| {
            format!(
                "rebase onto '{target}' conflicted ('{target}' moved since the precheck); re-run"
            )
        })
}

/// Fast-forward the target, never merging into it. Checked out somewhere: the
/// ff runs in that worktree so its files move with the ref. Not checked out:
/// there are no files to move, so moving the ref IS the fast-forward.
fn ff_target(
    wt: &Path,
    target_wt: Option<&Path>,
    target: &str,
    branch: &str,
    verb: &str,
) -> Result<(), String> {
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
                    // The reflog's own label, not a diagnostic: this is the
                    // only mover of a branch nobody is standing on, so the
                    // line it leaves is the whole record of what happened.
                    &format!("wtree {verb}: {branch}"),
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

    /// The cut has to land on a character boundary and inside the budget, and
    /// a wide character that would straddle the last column has to be dropped
    /// rather than pushed over it — one column past the edge wraps the row,
    /// which is the whole failure being avoided.
    #[test]
    fn truncate_to_never_exceeds_the_width() {
        for s in [
            "feat/short",
            "feat/an-extremely-long-branch-name-that-keeps-going",
            "feat/아주-긴-한글-브랜치-이름",
            "├─+ feat/혼합-mixed-이름",
        ] {
            for w in 0..40 {
                let cut = truncate_to(s, w);
                assert!(
                    display_width(&cut) <= w,
                    "{s:?} cut to {w}: {cut:?} is {} wide",
                    display_width(&cut)
                );
            }
        }
    }

    #[test]
    fn truncate_to_leaves_what_already_fits() {
        assert_eq!(truncate_to("feat/a", 6), "feat/a");
        assert_eq!(truncate_to("feat/a", 99), "feat/a");
        assert_eq!(truncate_to("한글", 4), "한글");
        // One column short: the marker replaces the last character it can.
        assert_eq!(truncate_to("feat/a", 5), "feat…");
        // Two columns for the wide character, one for the marker, exactly 3.
        assert_eq!(truncate_to("한글", 3), "한…");
        // Only the marker fits beside a character that needs two columns.
        assert_eq!(truncate_to("한글", 2), "…");
    }

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
            let sett = Settings {
                worktree_dir: Some(PathBuf::from(dir)),
            };
            worktree_dest(root, &sett, "feat/x").unwrap()
        };
        assert_eq!(
            dest("../proj.worktrees"),
            Path::new("/repos/proj.worktrees/feat-x")
        );
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

    /// What the menu offers, and what it refuses to offer. A directory that
    /// cannot be loaded is not a choice, and in the main worktree the two roots
    /// are one place — offering it twice would be a choice between identicals.
    #[test]
    fn candidates_hide_the_useless_and_collapse_the_identical() {
        let here = Path::new("/repo.worktrees/feat-x");
        let main = Path::new("/repo");
        let labels = |c: &[Candidate]| -> Vec<&'static str> { c.iter().map(|c| c.label).collect() };

        let both = candidates(here, main, &|_| true);
        assert_eq!(labels(&both), vec![HERE, MAIN]);
        assert_eq!(both[0].path, Path::new("/repo.worktrees/feat-x/.wtree"));
        assert_eq!(both[1].path, Path::new("/repo/.wtree"));

        let only_main = candidates(here, main, &|p| p.starts_with("/repo/"));
        assert_eq!(labels(&only_main), vec![MAIN]);

        assert!(candidates(here, main, &|_| false).is_empty());

        // standing in the main worktree
        assert_eq!(labels(&candidates(main, main, &|_| true)), vec![HERE]);
    }

    #[test]
    fn the_menu_puts_the_template_last() {
        let cands = candidates(Path::new("/a"), Path::new("/b"), &|_| true);
        let items = menu(&cands);
        assert_eq!(items.len(), 3);
        assert!(items[0].starts_with(HERE), "{items:?}");
        assert!(items[0].ends_with("/a/.wtree"), "{items:?}");
        assert!(items[1].starts_with(MAIN), "{items:?}");
        assert_eq!(items[2], "create new rules");

        // with nothing to load the caller skips the prompt, but the template
        // still has to be the entry a bare list falls back to.
        assert_eq!(menu(&[]), vec!["create new rules".to_string()]);
    }

    /// A directory name, so no colons, and sortable as plain text.
    #[test]
    fn the_backup_stamp_is_a_usable_directory_name() {
        let s = utc_stamp();
        assert_eq!(s.len(), 16, "{s}");
        assert!(s.ends_with('Z') && s[8..9].eq("T"), "{s}");
        assert!(
            s.chars()
                .all(|c| c.is_ascii_digit() || c == 'T' || c == 'Z'),
            "{s}"
        );
    }
}
