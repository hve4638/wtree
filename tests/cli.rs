//! Integration tests: run the real `wtree` binary against git fixtures.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use wtree::state::{Kind, StateRead};
use wtree::testutil::Fixture;
use wtree::{repo, state};

fn run_wt(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wtree"))
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("failed to spawn wtree")
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[track_caller]
fn assert_ok(o: &Output) {
    assert!(
        o.status.success(),
        "expected success\nstdout:\n{}\nstderr:\n{}",
        out(o),
        err(o)
    );
}

#[track_caller]
fn assert_fail(o: &Output) {
    assert!(
        !o.status.success(),
        "expected failure\nstdout:\n{}\nstderr:\n{}",
        out(o),
        err(o)
    );
}

fn write_config(fx: &Fixture, text: &str) {
    let dir = fx.repo.join(".git/wtree");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("config"), text).unwrap();
}

/// Default placement base used by `wtree new`: `<tmp>/repo.worktrees`.
fn default_dest(fx: &Fixture, branch: &str) -> PathBuf {
    fx.tmp
        .0
        .join("repo.worktrees")
        .join(branch.replace('/', "-"))
}

const GROUP_CFG: &str =
    "[main]\nchildren = group:feat\n\n[group:feat]\nname-allow = feature/*\n";

// -------------------------------------------------------------------- init ----

#[test]
fn init_creates_template_and_refuses_rerun() {
    let fx = Fixture::new();
    let o = run_wt(&fx.repo, &["init"]);
    assert_ok(&o);
    let cfg_path = fx.repo.join(".git/wtree/config");
    let text = fs::read_to_string(&cfg_path).unwrap();
    assert!(text.contains("[main]"), "{text}");
    assert!(text.contains("destroyable = false"), "{text}");
    assert!(text.contains("# children = group:work"), "{text}");
    assert!(text.contains("# name-allow = feat/*, fix/*"), "{text}");
    assert!(fx.repo.join(".git/wtree/hooks").is_dir());
    // the template must load clean
    let check = run_wt(&fx.repo, &["check", cfg_path.to_str().unwrap()]);
    assert_ok(&check);
    // re-run refused, config untouched
    let o2 = run_wt(&fx.repo, &["init"]);
    assert_fail(&o2);
    assert!(err(&o2).contains("already exists"), "{}", err(&o2));
    assert_eq!(fs::read_to_string(&cfg_path).unwrap(), text);
}

/// The two knobs `init` cannot prefill with anything useful still have to be
/// discoverable, so it leaves a commented file at each. Neither may take effect
/// on its own: the settings file must load as defaults, and the hook sample
/// must not be found by `new` (hence git's `.sample` spelling — a file named
/// `post-create` would warn about not being executable on every run).
#[test]
fn init_seeds_a_commented_settings_file_and_an_inert_hook_sample() {
    let fx = Fixture::new();
    assert_ok(&run_wt(&fx.repo, &["init"]));

    let sett = fs::read_to_string(fx.repo.join(".git/wtree/settings")).unwrap();
    assert!(sett.contains("# worktree-dir"), "the knob is shown, not set:\n{sett}");
    assert!(
        !sett.lines().any(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty()),
        "every line must be a comment:\n{sett}"
    );

    let sample = fx.repo.join(".git/wtree/hooks/post-create.sample");
    assert!(sample.is_file(), "hook sample written");
    assert!(!fx.repo.join(".git/wtree/hooks/post-create").exists(), "not enabled");
    assert_ne!(
        fs::metadata(&sample).unwrap().permissions().mode() & 0o111,
        0,
        "already executable, so enabling it is a rename"
    );

    // The defaults still apply and nothing warns about the sample.
    write_config(&fx, "[main]\nchildren = group:feat\n\n[group:feat]\nname-allow = feature/*\n");
    let o = run_wt(&fx.repo, &["new", "feature/a"]);
    assert_ok(&o);
    assert!(!err(&o).contains("hook"), "the sample must not be found:\n{}", err(&o));
    assert!(default_dest(&fx, "feature/a").is_dir(), "default placement unchanged");
}

/// `init` is guarded on the config file alone, so anything else it writes has
/// to give way to a file that was already there.
#[test]
fn init_keeps_a_settings_file_that_was_written_by_hand() {
    let fx = Fixture::new();
    let sett = fx.repo.join(".git/wtree/settings");
    fs::create_dir_all(sett.parent().unwrap()).unwrap();
    fs::write(&sett, "worktree-dir = ../mine\n").unwrap();

    assert_ok(&run_wt(&fx.repo, &["init"]));
    assert_eq!(fs::read_to_string(&sett).unwrap(), "worktree-dir = ../mine\n");
}

#[test]
fn init_root_detection_order() {
    // origin/HEAD symref wins
    let fx = Fixture::new();
    fx.git(
        &fx.repo,
        &["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/trunk"],
    );
    assert_ok(&run_wt(&fx.repo, &["init"]));
    let text = fs::read_to_string(fx.repo.join(".git/wtree/config")).unwrap();
    assert!(text.contains("[trunk]"), "{text}");

    // no origin/HEAD: master existence
    let fx = Fixture::new();
    fx.git(&fx.repo, &["branch", "-m", "master"]);
    assert_ok(&run_wt(&fx.repo, &["init"]));
    let text = fs::read_to_string(fx.repo.join(".git/wtree/config")).unwrap();
    assert!(text.contains("[master]"), "{text}");

    // no origin/HEAD, no main/master: current branch
    let fx = Fixture::new();
    fx.git(&fx.repo, &["branch", "-m", "work"]);
    assert_ok(&run_wt(&fx.repo, &["init"]));
    let text = fs::read_to_string(fx.repo.join(".git/wtree/config")).unwrap();
    assert!(text.contains("[work]"), "{text}");
}

#[test]
fn init_refuses_bare_repo() {
    let fx = Fixture::new();
    let bare = fx.tmp.0.join("bare.git");
    fs::create_dir_all(&bare).unwrap();
    fx.git(&bare, &["init", "-q", "--bare"]);
    let o = run_wt(&bare, &["init"]);
    assert_fail(&o);
    assert!(err(&o).contains("bare"), "{}", err(&o));
}

// --------------------------------------------------------------------- new ----

#[test]
fn new_group_member_records_state_at_default_placement() {
    let fx = Fixture::new();
    write_config(&fx, GROUP_CFG);
    let o = run_wt(&fx.repo, &["new", "feature/a"]);
    assert_ok(&o);
    let dest = default_dest(&fx, "feature/a");
    assert!(dest.is_dir(), "worktree missing at {}", dest.display());
    let stdout = out(&o);
    assert!(stdout.contains("group:feat"), "{stdout}");
    assert!(stdout.contains(&format!("cd {}", dest.display())), "{stdout}");
    // state record: (branch, kind, parent) as judged
    let private = repo::private_git_dir(&dest).unwrap();
    match state::read(&private) {
        StateRead::Valid(s) => {
            assert_eq!(s.branch, "feature/a");
            assert_eq!(s.kind, Kind::Group("feat".into()));
            assert_eq!(s.parent, "main");
        }
        other => panic!("expected a valid state record, got {other:?}"),
    }
}

#[test]
fn new_fixed_branch_leaves_no_state() {
    let fx = Fixture::new();
    write_config(&fx, "[main]\nchildren = dev\n\n[dev]\n");
    let o = run_wt(&fx.repo, &["new", "dev"]);
    assert_ok(&o);
    assert!(out(&o).contains("(fixed)"), "{}", out(&o));
    let dest = default_dest(&fx, "dev");
    assert_eq!(repo::head_branch(&dest).unwrap().as_deref(), Some("dev"));
    let private = repo::private_git_dir(&dest).unwrap();
    assert_eq!(state::read(&private), StateRead::Missing);
}

#[test]
fn new_refusal_prints_judge_reasons_and_creates_nothing() {
    let fx = Fixture::new();
    write_config(&fx, GROUP_CFG);
    let o = run_wt(&fx.repo, &["new", "junk/x"]);
    assert_fail(&o);
    let stderr = err(&o);
    assert!(stderr.contains("wtree new: refused"), "{stderr}");
    assert!(stderr.contains("does not match name-allow"), "{stderr}");
    assert!(stderr.contains("rule: name-allow"), "{stderr}");
    // neither a worktree nor a branch was created
    assert!(!default_dest(&fx, "junk/x").exists());
    let refs = fx.git(&fx.repo, &["for-each-ref", "--format=%(refname:short)", "refs/heads"]);
    assert_eq!(refs.trim(), "main");
}

#[test]
fn new_placement_settings_override() {
    // absolute worktree-dir
    let fx = Fixture::new();
    write_config(&fx, GROUP_CFG);
    let abs = fx.tmp.0.join("abs-wts");
    fs::write(
        fx.repo.join(".git/wtree/settings"),
        format!("worktree-dir = {}\n", abs.display()),
    )
    .unwrap();
    assert_ok(&run_wt(&fx.repo, &["new", "feature/a"]));
    assert!(abs.join("feature-a").is_dir());

    // relative worktree-dir resolves against the primary worktree root
    let fx = Fixture::new();
    write_config(&fx, GROUP_CFG);
    fs::write(fx.repo.join(".git/wtree/settings"), "worktree-dir = wts\n").unwrap();
    assert_ok(&run_wt(&fx.repo, &["new", "feature/b"]));
    assert!(fx.repo.join("wts/feature-b").is_dir());

    // a settings typo aborts instead of silently using the default
    let fx = Fixture::new();
    write_config(&fx, GROUP_CFG);
    fs::write(fx.repo.join(".git/wtree/settings"), "worktreedir = x\n").unwrap();
    let o = run_wt(&fx.repo, &["new", "feature/c"]);
    assert_fail(&o);
    assert!(err(&o).contains("unknown key 'worktreedir'"), "{}", err(&o));
}

fn install_hook(fx: &Fixture, body: &str) {
    let hooks = fx.repo.join(".git/wtree/hooks");
    fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("post-create");
    fs::write(&hook, body).unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn new_runs_post_create_hook_with_wt_env() {
    let fx = Fixture::new();
    write_config(&fx, GROUP_CFG);
    install_hook(
        &fx,
        "#!/bin/sh\nprintf '%s|%s|%s|%s|%s' \"$WT_BRANCH\" \"$WT_PARENT\" \"$WT_REPO\" \"$WT_INTERACTIVE\" \"$(pwd)\" > \"$WT_PATH/hook-ran\"\n",
    );
    assert_ok(&run_wt(&fx.repo, &["new", "feature/a"]));
    let dest = default_dest(&fx, "feature/a");
    let marker = fs::read_to_string(dest.join("hook-ran")).unwrap();
    let parts: Vec<&str> = marker.split('|').collect();
    assert_eq!(parts[0], "feature/a");
    assert_eq!(parts[1], "main");
    assert_eq!(
        Path::new(parts[2]).canonicalize().unwrap(),
        fx.repo.canonicalize().unwrap(),
        "WT_REPO must be the primary worktree root"
    );
    assert_eq!(parts[3], "0", "captured output is non-interactive");
    assert_eq!(
        Path::new(parts[4]).canonicalize().unwrap(),
        dest.canonicalize().unwrap(),
        "hook must run with cwd = the new worktree"
    );
}

#[test]
fn new_hook_failure_warns_but_keeps_worktree() {
    let fx = Fixture::new();
    write_config(&fx, GROUP_CFG);
    install_hook(&fx, "#!/bin/sh\nexit 3\n");
    let o = run_wt(&fx.repo, &["new", "feature/a"]);
    assert_ok(&o); // hook failure is not a verb failure
    assert!(err(&o).contains("post-create hook failed (exit 3)"), "{}", err(&o));
    let dest = default_dest(&fx, "feature/a");
    assert!(dest.is_dir());
    assert!(matches!(
        state::read(&repo::private_git_dir(&dest).unwrap()),
        StateRead::Valid(_)
    ));
}

// ------------------------------------------------------------------- copy ----

/// A fresh worktree has only what the branch tracks, so `.env` and friends have
/// to be carried over or nothing runs there.
#[test]
fn new_carries_the_files_the_policy_lists_from_the_parent_worktree() {
    let fx = Fixture::new();
    write_config(
        &fx,
        "[main]\nchildren = group:feat\n\n[group:feat]\nname-allow = feature/*\ncopy = .env, .env.*, .vscode/\n",
    );
    fs::write(fx.repo.join(".env"), "SECRET=1\n").unwrap();
    fs::write(fx.repo.join(".env.local"), "LOCAL=2\n").unwrap();
    fs::create_dir(fx.repo.join(".vscode")).unwrap();
    fs::write(fx.repo.join(".vscode/settings.json"), "{}\n").unwrap();
    fs::write(fx.repo.join("untouched"), "no rule names me\n").unwrap();

    let o = run_wt(&fx.repo, &["new", "feature/a"]);
    assert_ok(&o);
    assert!(out(&o).contains("copied .env, .env.local, .vscode from 'main'"), "{}", out(&o));

    let dest = default_dest(&fx, "feature/a");
    assert_eq!(fs::read_to_string(dest.join(".env")).unwrap(), "SECRET=1\n");
    assert_eq!(fs::read_to_string(dest.join(".env.local")).unwrap(), "LOCAL=2\n");
    assert_eq!(fs::read_to_string(dest.join(".vscode/settings.json")).unwrap(), "{}\n");
    assert!(!dest.join("untouched").exists(), "only listed patterns cross");
}

/// The trailing slash is what makes a directory deliberate. Without it the rule
/// looks like it applies, so the near miss is named rather than passed over.
#[test]
fn a_directory_crosses_only_when_the_pattern_ends_in_a_slash() {
    let fx = Fixture::new();
    write_config(
        &fx,
        "[main]\nchildren = group:feat\n\n[group:feat]\nname-allow = feature/*\ncopy = node_modules\n",
    );
    fs::create_dir(fx.repo.join("node_modules")).unwrap();
    fs::write(fx.repo.join("node_modules/index.js"), "x\n").unwrap();

    let o = run_wt(&fx.repo, &["new", "feature/a"]);
    assert_ok(&o);
    assert!(
        out(&o).contains("skipped 'node_modules': a directory needs a trailing '/'"),
        "{}",
        out(&o)
    );
    assert!(!default_dest(&fx, "feature/a").join("node_modules").exists());
}

/// The entry a pattern names is recreated when it is a link, not followed —
/// otherwise the trailing-slash rule leaks: a bare pattern would drag in the
/// whole tree behind a symlinked `node_modules`.
#[test]
fn a_symlinked_entry_crosses_as_a_link_and_is_not_followed() {
    let fx = Fixture::new();
    write_config(
        &fx,
        "[main]\nchildren = group:feat\n\n[group:feat]\nname-allow = feature/*\ncopy = node_modules\n",
    );
    fs::create_dir(fx.repo.join("real_modules")).unwrap();
    fs::write(fx.repo.join("real_modules/index.js"), "x\n").unwrap();
    std::os::unix::fs::symlink("real_modules", fx.repo.join("node_modules")).unwrap();

    let o = run_wt(&fx.repo, &["new", "feature/a"]);
    assert_ok(&o);
    assert!(out(&o).contains("copied node_modules"), "{}", out(&o));
    let link = default_dest(&fx, "feature/a").join("node_modules");
    let ft = fs::symlink_metadata(&link).unwrap().file_type();
    assert!(ft.is_symlink(), "the link was dereferenced into a copied tree");
    assert_eq!(fs::read_link(&link).unwrap(), Path::new("real_modules"));
}

/// Copying over a tracked file would leave the worktree dirty before the user
/// has touched anything, so what the branch already carries wins.
#[test]
fn copy_never_overwrites_what_the_branch_already_tracks() {
    let fx = Fixture::new();
    write_config(
        &fx,
        "[main]\nchildren = group:feat\n\n[group:feat]\nname-allow = feature/*\ncopy = tracked.txt\n",
    );
    fs::write(fx.repo.join("tracked.txt"), "committed\n").unwrap();
    fx.git(&fx.repo, &["add", "-A"]);
    fx.git(&fx.repo, &["commit", "-q", "-m", "track it"]);
    fs::write(fx.repo.join("tracked.txt"), "local edit\n").unwrap();

    let o = run_wt(&fx.repo, &["new", "feature/a"]);
    assert_ok(&o);
    assert!(out(&o).contains("skipped 'tracked.txt': already in the worktree"), "{}", out(&o));
    let dest = default_dest(&fx, "feature/a");
    assert_eq!(fs::read_to_string(dest.join("tracked.txt")).unwrap(), "committed\n");
    assert_eq!(fx.git(&dest, &["status", "--porcelain"]).trim(), "");
}

/// `open` reads the parent from the config, so the source can be a worktree
/// that is not currently checked out. The worktree is still created — it is
/// usable without the files, and undoing it would be the larger surprise.
#[test]
fn open_says_so_when_the_parent_has_no_worktree_to_copy_from() {
    let fx = Fixture::new();
    write_config(
        &fx,
        "[main]\nchildren = dev\n\n[dev]\nchildren = staging\n\n[staging]\ncopy = .env\n",
    );
    fs::write(fx.repo.join(".env"), "SECRET=1\n").unwrap();
    fx.git(&fx.repo, &["branch", "staging", "main"]);

    let o = run_wt(&fx.repo, &["open", "staging"]);
    assert_ok(&o);
    assert!(out(&o).contains("copied nothing: parent 'dev' has no worktree"), "{}", out(&o));
    assert!(default_dest(&fx, "staging").exists(), "the worktree is created regardless");
}

/// A pattern with a separator can never match — entries are matched by name at
/// the worktree root — so it is a policy that silently does nothing.
#[test]
fn a_copy_pattern_with_a_path_separator_is_a_load_error() {
    let fx = Fixture::new();
    write_config(&fx, "[main]\nchildren = group:feat\n\n[group:feat]\ncopy = config/*.json\n");
    let o = run_wt(&fx.repo, &["list"]);
    assert_fail(&o);
    assert!(
        err(&o).contains("invalid copy pattern 'config/*.json' in [group:feat]"),
        "{}",
        err(&o)
    );
    assert!(err(&o).contains(":5"), "the offending line is cited:\n{}", err(&o));
}

// --------------------------------------------------------------- list/info ----

#[test]
fn list_shows_identities_unknowns_and_bare_branches() {
    let fx = Fixture::new();
    write_config(
        &fx,
        "[main]\nchildren = dev, group:feat\n\n[dev]\n\n[group:feat]\nname-allow = feature/*\n",
    );
    assert_ok(&run_wt(&fx.repo, &["new", "feature/a"]));
    fx.git(&fx.repo, &["branch", "dev", "main"]); // declared fixed, no worktree
    fx.add_worktree("junk", "main"); // raw worktree, unmanaged
    let o = run_wt(&fx.repo, &["list"]);
    assert_ok(&o);
    let stdout = out(&o);
    assert!(stdout.contains("worktrees:"), "{stdout}");
    assert!(stdout.contains("* repo  main  fixed  root"), "{stdout}");
    assert!(stdout.contains("feature/a  group:feat  parent: main"), "{stdout}");
    assert!(stdout.contains("junk  UNKNOWN"), "{stdout}");
    assert!(stdout.contains("wtree adopt"), "{stdout}");
    assert!(stdout.contains("branches without worktrees:"), "{stdout}");
    assert!(stdout.contains("dev  fixed  parent: main"), "{stdout}");
}

/// The count is what tells a worktree it is due for `sync`; nothing else on
/// screen says so. A root branch has no parent to fall behind, and a worktree
/// level with its parent stays quiet.
#[test]
fn list_counts_the_commits_a_worktree_is_behind_its_parent() {
    let fx = Fixture::new();
    write_config(
        &fx,
        "[main]\nchildren = group:feat\n\n[group:feat]\nname-allow = feature/*\n",
    );
    assert_ok(&run_wt(&fx.repo, &["new", "feature/a"]));

    let level = out(&run_wt(&fx.repo, &["list"]));
    assert!(!level.contains("[behind"), "just created, nothing behind:\n{level}");

    commit_other(&fx, &fx.repo, "one.txt", "one");
    commit_other(&fx, &fx.repo, "two.txt", "two");

    let stdout = out(&run_wt(&fx.repo, &["list"]));
    assert!(
        stdout.contains("feature/a  group:feat  parent: main [behind 2]"),
        "{stdout}"
    );
    assert!(
        stdout.lines().any(|l| l.contains("* repo  main") && !l.contains("[behind")),
        "main is the root — it has no parent to fall behind:\n{stdout}"
    );
}

#[test]
fn info_managed_shows_rules_and_previews() {
    let fx = Fixture::new();
    write_config(
        &fx,
        "[main]\nchildren = group:feat\nmerge-mode = squash\n\n[group:feat]\nname-allow = feature/*\n",
    );
    assert_ok(&run_wt(&fx.repo, &["new", "feature/a"]));
    let dest = default_dest(&fx, "feature/a");
    let o = run_wt(&dest, &["info"]);
    assert_ok(&o);
    let stdout = out(&o);
    assert!(stdout.contains("identity: group:feat"), "{stdout}");
    assert!(stdout.contains("parent: main (recorded)"), "{stdout}");
    assert!(stdout.contains("merge to 'main': squash (flag optional"), "{stdout}");
    assert!(stdout.contains("merge: 'feature/a' -> 'main' (--squash)"), "{stdout}");
    assert!(stdout.contains("sync: merge 'main' into 'feature/a'"), "{stdout}");
    assert!(stdout.contains("destroy: would remove 'feature/a'"), "{stdout}");
    assert!(
        stdout.contains("children: none declared — nothing may be created here"),
        "{stdout}"
    );
}

#[test]
fn info_unknown_shows_reasons_and_adopt_hint() {
    let fx = Fixture::new();
    write_config(&fx, GROUP_CFG);
    let junk = fx.add_worktree("junk", "main");
    let o = run_wt(&junk, &["info"]);
    assert_ok(&o);
    let stdout = out(&o);
    assert!(stdout.contains("identity: unknown"), "{stdout}");
    assert!(stdout.contains("not a declared [branch]"), "{stdout}");
    assert!(stdout.contains("wtree adopt"), "{stdout}");
    assert!(
        stdout.contains("allowed verbs here: open, close, list, info, init, adopt"),
        "{stdout}"
    );
}

// ------------------------------------------------------------------- merge ----

/// Managed group-member worktree, as `wtree new` would leave it.
fn member(fx: &Fixture, branch: &str, group: &str, parent: &str) -> PathBuf {
    let p = fx.add_worktree(branch, parent);
    fx.write_state(&p, branch, &format!("group:{group}"), parent);
    p
}

fn rev(fx: &Fixture, r: &str) -> String {
    fx.git(&fx.repo, &["rev-parse", r]).trim().to_string()
}

/// Commit a file other than the fixture's f.txt — moves the branch without
/// conflicting with f.txt edits made elsewhere.
fn commit_other(fx: &Fixture, dir: &Path, name: &str, msg: &str) {
    fs::write(dir.join(name), format!("{msg}\n")).unwrap();
    fx.git(dir, &["add", "-A"]);
    fx.git(dir, &["commit", "-q", "-m", msg]);
}

fn merge_cfg(modes: &str) -> String {
    format!(
        "[main]\nchildren = group:feat\nmerge-mode = {modes}\n\n[group:feat]\nname-allow = feature/*\n"
    )
}

#[test]
fn merge_squash_lands_one_commit_and_converges() {
    let fx = Fixture::new();
    write_config(&fx, &merge_cfg("squash"));
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "one");
    fx.commit(&wt, "two");
    let before = rev(&fx, "main");
    // single allowed mode: the flag may be omitted
    let o = run_wt(&wt, &["merge", "-m", "feat: a"]);
    assert_ok(&o);
    assert!(out(&o).contains("merged 'feature/a' onto 'main'"), "{}", out(&o));
    let count = fx.git(&fx.repo, &["rev-list", "--count", &format!("{before}..main")]);
    assert_eq!(count.trim(), "1", "squash lands exactly one commit");
    assert_eq!(fx.git(&fx.repo, &["log", "-1", "--format=%s", "main"]).trim(), "feat: a");
    // convergence: the branch sits exactly on the target
    assert_eq!(rev(&fx, "main"), rev(&fx, "feature/a"));
    // the target's checked-out worktree received the files
    assert!(fs::read_to_string(fx.repo.join("f.txt")).unwrap().contains("two"));
}

#[test]
fn merge_rebase_replays_each_commit_onto_moved_target() {
    let fx = Fixture::new();
    write_config(&fx, &merge_cfg("rebase"));
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "one");
    fx.commit(&wt, "two");
    let before = rev(&fx, "main");
    commit_other(&fx, &fx.repo, "other.txt", "main moved"); // non-conflicting
    let o = run_wt(&wt, &["merge"]);
    assert_ok(&o);
    assert!(out(&o).contains("2 commits"), "{}", out(&o));
    let count = fx.git(&fx.repo, &["rev-list", "--count", &format!("{before}..main")]);
    assert_eq!(count.trim(), "3", "2 replayed + the target's own commit");
    let subjects = fx.git(&fx.repo, &["log", "-3", "--format=%s", "main"]);
    assert_eq!(subjects.trim(), "two\none\nmain moved");
    assert_eq!(rev(&fx, "main"), rev(&fx, "feature/a"));
}

#[test]
fn merge_no_ff_creates_merge_commit_without_target_checkout() {
    let fx = Fixture::new();
    write_config(&fx, &merge_cfg("no-ff"));
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "one");
    let main_before = rev(&fx, "main");
    let feat_before = rev(&fx, "feature/a");
    let o = run_wt(&wt, &["merge", "-m", "merge feature/a"]);
    assert_ok(&o);
    assert!(out(&o).contains("merge commit"), "{}", out(&o));
    // target tip is a merge commit: first parent old target, second the branch
    assert_eq!(rev(&fx, "main^1"), main_before);
    assert_eq!(rev(&fx, "main^2"), feat_before);
    assert_eq!(
        fx.git(&fx.repo, &["log", "-1", "--format=%s", "main"]).trim(),
        "merge feature/a"
    );
    assert_eq!(rev(&fx, "main"), rev(&fx, "feature/a")); // convergence
    // the primary worktree (main checked out) moved with the ff
    assert!(fs::read_to_string(fx.repo.join("f.txt")).unwrap().contains("one"));
}

#[test]
fn merge_ff_moves_target_and_refuses_without_fallback() {
    let fx = Fixture::new();
    write_config(&fx, &merge_cfg("ff"));
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "one");
    let feat_tip = rev(&fx, "feature/a");
    let o = run_wt(&wt, &["merge"]);
    assert_ok(&o);
    assert!(out(&o).contains("fast-forwarded 'main' to 'feature/a'"), "{}", out(&o));
    assert_eq!(rev(&fx, "main"), feat_tip); // ff: no new commit objects

    // fork again, let the target move: refused with the sync hint, no fallback
    let wt2 = member(&fx, "feature/b", "feat", "main");
    fx.commit(&wt2, "mine");
    commit_other(&fx, &fx.repo, "other.txt", "main moved");
    let main_before = rev(&fx, "main");
    let feat_b = rev(&fx, "feature/b");
    let o2 = run_wt(&wt2, &["merge"]);
    assert_fail(&o2);
    let stderr = err(&o2);
    assert!(stderr.contains("cannot fast-forward"), "{stderr}");
    assert!(stderr.contains("wtree sync"), "{stderr}");
    assert!(stderr.contains("nothing was changed"), "{stderr}");
    assert_eq!(rev(&fx, "main"), main_before);
    assert_eq!(rev(&fx, "feature/b"), feat_b);
}

#[test]
fn merge_conflict_precheck_refuses_before_touching_anything() {
    let fx = Fixture::new();
    write_config(&fx, &merge_cfg("squash"));
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "branch side");
    fx.commit(&fx.repo, "main side"); // same spot in f.txt -> conflict
    let main_before = rev(&fx, "main");
    let feat_before = rev(&fx, "feature/a");
    let o = run_wt(&wt, &["merge", "-m", "boom"]);
    assert_fail(&o);
    let stderr = err(&o);
    assert!(stderr.contains("would conflict in: f.txt"), "{stderr}");
    assert!(stderr.contains("git merge main"), "{stderr}");
    assert!(stderr.contains("nothing was changed"), "{stderr}");
    assert_eq!(rev(&fx, "main"), main_before);
    assert_eq!(rev(&fx, "feature/a"), feat_before);
    assert_eq!(fx.git(&wt, &["status", "--porcelain"]).trim(), "");
}

#[test]
fn merge_nothing_to_merge_is_refused() {
    let fx = Fixture::new();
    write_config(&fx, &merge_cfg("squash"));
    let wt = member(&fx, "feature/a", "feat", "main"); // no commits of its own
    let o = run_wt(&wt, &["merge", "-m", "empty"]);
    assert_fail(&o);
    assert!(err(&o).contains("nothing to merge"), "{}", err(&o));
}

#[test]
fn merge_stashes_and_restores_uncommitted_work() {
    let fx = Fixture::new();
    write_config(&fx, &merge_cfg("squash"));
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "one");
    // dirty on top: a tracked edit and an untracked file
    let f = wt.join("f.txt");
    let committed = fs::read_to_string(&f).unwrap();
    fs::write(&f, format!("{committed}WIP\n")).unwrap();
    fs::write(wt.join("scratch.txt"), "notes\n").unwrap();
    let o = run_wt(&wt, &["merge", "-m", "feat: a"]);
    assert_ok(&o);
    // only committed work landed
    assert!(!fs::read_to_string(fx.repo.join("f.txt")).unwrap().contains("WIP"));
    // and the uncommitted work came back, not left in the stash
    assert!(fs::read_to_string(&f).unwrap().contains("WIP"));
    assert_eq!(fs::read_to_string(wt.join("scratch.txt")).unwrap(), "notes\n");
    assert_eq!(fx.git(&wt, &["stash", "list"]).trim(), "");
}

#[test]
fn merge_moves_uncheckedout_target_by_ref_update() {
    let fx = Fixture::new();
    write_config(
        &fx,
        "[main]\nchildren = dev\n\n[dev]\nchildren = group:feat\nmerge-mode = squash\n\n[group:feat]\nname-allow = feature/*\n",
    );
    fx.git(&fx.repo, &["branch", "dev", "main"]); // dev exists, checked out nowhere
    let wt = member(&fx, "feature/a", "feat", "dev");
    fx.commit(&wt, "one");
    let main_before = rev(&fx, "main");
    let o = run_wt(&wt, &["merge", "-m", "feat: a"]);
    assert_ok(&o);
    assert_eq!(rev(&fx, "dev"), rev(&fx, "feature/a"));
    assert_eq!(rev(&fx, "main"), main_before); // main untouched
    assert!(!fs::read_to_string(fx.repo.join("f.txt")).unwrap().contains("one"));
}

#[test]
fn merge_rolls_back_branch_and_stash_when_target_ff_fails() {
    let fx = Fixture::new();
    write_config(&fx, &merge_cfg("squash"));
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "one");
    // work in progress in the target's worktree touching the same file: git
    // itself refuses the fast-forward there
    fs::write(fx.repo.join("f.txt"), "local main edit\n").unwrap();
    // and uncommitted work here, to see the stash come back on the bail path
    fs::write(wt.join("scratch.txt"), "notes\n").unwrap();
    let before = rev(&fx, "feature/a");
    let o = run_wt(&wt, &["merge", "-m", "feat: a"]);
    assert_fail(&o);
    let stderr = err(&o);
    assert!(stderr.contains("could not fast-forward 'main'"), "{stderr}");
    assert!(stderr.contains("nothing merged"), "{stderr}");
    // branch restored to its original commit, stash restored
    assert_eq!(rev(&fx, "feature/a"), before);
    assert_eq!(fs::read_to_string(wt.join("scratch.txt")).unwrap(), "notes\n");
    assert_eq!(fx.git(&wt, &["stash", "list"]).trim(), "");
}

#[test]
fn merge_flag_and_message_rules() {
    let fx = Fixture::new();
    // two allowed modes: the flag is mandatory
    write_config(&fx, &merge_cfg("squash, rebase"));
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "one");
    let o = run_wt(&wt, &["merge", "-m", "x"]);
    assert_fail(&o);
    assert!(err(&o).contains("multiple merge modes"), "{}", err(&o));
    // a mode outside the allowed set is refused by the judge
    let o = run_wt(&wt, &["merge", "--ff"]);
    assert_fail(&o);
    assert!(err(&o).contains("accepts squash, rebase merges only"), "{}", err(&o));
    // --rebase with -m: nothing to name
    let o = run_wt(&wt, &["merge", "--rebase", "-m", "x"]);
    assert_fail(&o);
    assert!(err(&o).contains("-m has nothing to name"), "{}", err(&o));
    // --squash without -m
    let o = run_wt(&wt, &["merge", "--squash"]);
    assert_fail(&o);
    assert!(err(&o).contains("-m <message> is required"), "{}", err(&o));
    // an explicit flag picks the mode
    let o = run_wt(&wt, &["merge", "--rebase"]);
    assert_ok(&o);
    assert_eq!(rev(&fx, "main"), rev(&fx, "feature/a"));
}

// -------------------------------------------------------------------- sync ----

#[test]
fn sync_merges_parent_and_is_idempotent() {
    let fx = Fixture::new();
    write_config(&fx, GROUP_CFG);
    let wt = member(&fx, "feature/a", "feat", "main");
    // own work + parent work in different files: a true merge, no conflict
    commit_other(&fx, &wt, "mine.txt", "mine");
    fx.commit(&fx.repo, "parent work");
    // uncommitted work survives the sync
    fs::write(wt.join("scratch.txt"), "notes\n").unwrap();
    let o = run_wt(&wt, &["sync"]);
    assert_ok(&o);
    assert!(out(&o).contains("synced 'feature/a' with 'main'"), "{}", out(&o));
    // parent contained; own commit kept; uncommitted work restored
    fx.git(&wt, &["merge-base", "--is-ancestor", "main", "feature/a"]);
    assert!(fs::read_to_string(wt.join("f.txt")).unwrap().contains("parent work"));
    assert!(wt.join("mine.txt").exists());
    assert_eq!(fs::read_to_string(wt.join("scratch.txt")).unwrap(), "notes\n");
    assert_eq!(fx.git(&wt, &["stash", "list"]).trim(), "");
    // a second sync has nothing to do
    let o2 = run_wt(&wt, &["sync"]);
    assert_ok(&o2);
    assert!(out(&o2).contains("already up to date"), "{}", out(&o2));
}

#[test]
fn sync_conflict_refused_with_guidance() {
    let fx = Fixture::new();
    write_config(&fx, GROUP_CFG);
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "branch side");
    fx.commit(&fx.repo, "main side"); // same spot in f.txt -> conflict
    let before = rev(&fx, "feature/a");
    let o = run_wt(&wt, &["sync"]);
    assert_fail(&o);
    let stderr = err(&o);
    assert!(stderr.contains("would conflict in: f.txt"), "{stderr}");
    assert!(stderr.contains("git merge main"), "{stderr}");
    assert!(stderr.contains("nothing was changed"), "{stderr}");
    assert_eq!(rev(&fx, "feature/a"), before);
    assert_eq!(fx.git(&wt, &["status", "--porcelain"]).trim(), "");
}

// ------------------------------------------------------------------- adopt ----

/// main accepts a group, a free branch and one fixed child; dev accepts a
/// second group but no free branches; `other` is declared but listed nowhere.
const ADOPT_CFG: &str = "[main]\n\
                         children = dev, group:feat, *\n\
                         merge-mode = squash\n\
                         \n\
                         [dev]\n\
                         children = group:feat2\n\
                         \n\
                         [group:feat]\n\
                         name-allow = feature/*\n\
                         \n\
                         [group:feat2]\n\
                         name-allow = feature/*\n\
                         \n\
                         [group:other]\n\
                         name-allow = feature/*\n";

#[track_caller]
fn state_of(wt: &Path) -> state::State {
    match state::read(&repo::private_git_dir(wt).unwrap()) {
        StateRead::Valid(s) => s,
        other => panic!("expected a valid state record, got {other:?}"),
    }
}

fn state_read(wt: &Path) -> StateRead {
    state::read(&repo::private_git_dir(wt).unwrap())
}

#[test]
fn adopt_records_a_raw_worktree() {
    let fx = Fixture::new();
    write_config(&fx, ADOPT_CFG);
    let wt = fx.add_worktree("feature/a", "main"); // made with raw git: no record
    assert_eq!(state_read(&wt), StateRead::Missing);
    let o = run_wt(&wt, &["adopt", "--group", "feat", "--parent", "main"]);
    assert_ok(&o);
    let stdout = out(&o);
    assert!(
        stdout.contains("adopted 'feature/a' (group:feat) with parent 'main'"),
        "{stdout}"
    );
    assert!(!stdout.contains("replacing"), "nothing to replace: {stdout}");
    let s = state_of(&wt);
    assert_eq!(s.branch, "feature/a");
    assert_eq!(s.kind, Kind::Group("feat".into()));
    assert_eq!(s.parent, "main");
    // managed from here on
    let info = run_wt(&wt, &["info"]);
    assert_ok(&info);
    assert!(out(&info).contains("identity: group:feat"), "{}", out(&info));
}

#[test]
fn adopt_free_needs_a_star_in_the_parents_children() {
    let fx = Fixture::new();
    write_config(&fx, ADOPT_CFG);
    fx.git(&fx.repo, &["branch", "dev", "main"]);
    let wt = fx.add_worktree("junk", "main");
    // main lists '*'
    let o = run_wt(&wt, &["adopt", "--free", "--parent", "main"]);
    assert_ok(&o);
    assert_eq!(state_of(&wt).kind, Kind::Free);
    // dev does not, and the refusal leaves the existing record alone
    let o = run_wt(&wt, &["adopt", "--free", "--parent", "dev"]);
    assert_fail(&o);
    assert!(err(&o).contains("contains no '*'"), "{}", err(&o));
    assert_eq!(state_of(&wt).parent, "main");
}

#[test]
fn readopt_corrects_group_and_parent_after_showing_the_old_record() {
    let fx = Fixture::new();
    write_config(&fx, ADOPT_CFG);
    fx.git(&fx.repo, &["branch", "dev", "main"]);
    let wt = member(&fx, "feature/a", "feat", "main");
    let o = run_wt(&wt, &["adopt", "--group", "feat2", "--parent", "dev"]);
    assert_ok(&o);
    let stdout = out(&o);
    assert!(
        stdout.contains("replacing the existing record: branch=feature/a, kind=group:feat, parent=main"),
        "{stdout}"
    );
    assert!(
        stdout.contains("adopted 'feature/a' (group:feat2) with parent 'dev'"),
        "{stdout}"
    );
    let s = state_of(&wt);
    assert_eq!(s.kind, Kind::Group("feat2".into()));
    assert_eq!(s.parent, "dev");
}

#[test]
fn adopt_recovers_a_record_head_mismatch() {
    let fx = Fixture::new();
    write_config(&fx, ADOPT_CFG);
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.git(&wt, &["switch", "-q", "-c", "oops"]); // raw switch: the record now lies
    let refused = run_wt(&wt, &["sync"]);
    assert_fail(&refused);
    assert!(
        err(&refused).contains("recorded branch 'feature/a' != HEAD 'oops'"),
        "{}",
        err(&refused)
    );
    let o = run_wt(&wt, &["adopt", "--free", "--parent", "main"]);
    assert_ok(&o);
    assert!(
        out(&o).contains("replacing the existing record: branch=feature/a"),
        "{}",
        out(&o)
    );
    let s = state_of(&wt);
    assert_eq!(s.branch, "oops");
    assert_eq!(s.kind, Kind::Free);
    assert_eq!(s.parent, "main");
    // the verbs that were locked out work again
    assert_ok(&run_wt(&wt, &["sync"]));
}

#[test]
fn adopt_refusals_write_nothing() {
    let fx = Fixture::new();
    write_config(&fx, ADOPT_CFG);

    // a declared group that is not in the parent's children
    let a = fx.add_worktree("feature/a", "main");
    let o = run_wt(&a, &["adopt", "--group", "other", "--parent", "main"]);
    assert_fail(&o);
    let stderr = err(&o);
    assert!(stderr.contains("wtree adopt: refused"), "{stderr}");
    assert!(
        stderr.contains("--group other: not in children of [main]"),
        "{stderr}"
    );
    assert!(stderr.contains("rule: children"), "{stderr}");
    assert_eq!(state_read(&a), StateRead::Missing);

    // naming constraints are not a back door
    let j = fx.add_worktree("junk", "main");
    let o = run_wt(&j, &["adopt", "--group", "feat", "--parent", "main"]);
    assert_fail(&o);
    assert!(err(&o).contains("does not match name-allow"), "{}", err(&o));
    assert!(err(&o).contains("rule: name-allow"), "{}", err(&o));

    // name reservation, for --group and --free alike
    let dev = fx.add_worktree("dev", "main");
    for flags in [
        vec!["adopt", "--group", "feat", "--parent", "main"],
        vec!["adopt", "--free", "--parent", "main"],
    ] {
        let o = run_wt(&dev, &flags);
        assert_fail(&o);
        assert!(err(&o).contains("name reservation"), "{:?}: {}", flags, err(&o));
    }
    assert_eq!(state_read(&dev), StateRead::Missing);

    // orphan history: no merge-base with the parent
    let orph = fx.add_worktree_detached("orph", "main");
    fx.git(&orph, &["checkout", "-q", "--orphan", "orphan-branch"]);
    fx.commit(&orph, "disconnected root");
    let o = run_wt(&orph, &["adopt", "--free", "--parent", "main"]);
    assert_fail(&o);
    assert!(
        err(&o).contains("no common ancestor (merge-base) with parent 'main'"),
        "{}",
        err(&o)
    );

    // a nonexistent parent
    let o = run_wt(&a, &["adopt", "--group", "feat", "--parent", "ghost"]);
    assert_fail(&o);
    assert!(err(&o).contains("parent branch 'ghost' does not exist"), "{}", err(&o));
}

#[test]
fn adopt_flag_combinations_fail_as_usage_errors() {
    let fx = Fixture::new();
    write_config(&fx, ADOPT_CFG);
    let a = fx.add_worktree("feature/a", "main");
    for (flags, needle) in [
        (
            vec!["adopt", "--group", "feat", "--free", "--parent", "main"],
            "mutually exclusive",
        ),
        (vec!["adopt", "--parent", "main"], "one of --group <X> or --free"),
        (vec!["adopt", "--group", "feat"], "--parent <branch> is required"),
        (vec!["adopt", "--free", "--parent", "main", "x"], "unknown argument 'x'"),
    ] {
        let o = run_wt(&a, &flags);
        assert_eq!(o.status.code(), Some(2), "{flags:?} must be a usage error");
        assert!(err(&o).contains(needle), "{:?}: {}", flags, err(&o));
        assert!(err(&o).contains("usage: wtree adopt"), "{:?}: {}", flags, err(&o));
    }
    assert_eq!(state_read(&a), StateRead::Missing);
}

#[test]
fn merge_refused_while_unmanaged_then_allowed_after_adopt() {
    let fx = Fixture::new();
    write_config(&fx, ADOPT_CFG);
    let wt = fx.add_worktree("feature/a", "main"); // raw worktree, unmanaged
    fx.commit(&wt, "one");
    let o = run_wt(&wt, &["merge", "-m", "feat: a"]);
    assert_fail(&o);
    let stderr = err(&o);
    assert!(stderr.contains("wtree merge: refused"), "{stderr}");
    assert!(stderr.contains("unmanaged (fail closed)"), "{stderr}");
    assert!(stderr.contains("wtree adopt"), "{stderr}");
    let main_before = rev(&fx, "main");

    assert_ok(&run_wt(&wt, &["adopt", "--group", "feat", "--parent", "main"]));
    let o = run_wt(&wt, &["merge", "-m", "feat: a"]);
    assert_ok(&o);
    assert_ne!(rev(&fx, "main"), main_before);
    assert_eq!(rev(&fx, "main"), rev(&fx, "feature/a"));
}

// ----------------------------------------------------------------- destroy ----

fn branches(fx: &Fixture) -> Vec<String> {
    fx.git(&fx.repo, &["for-each-ref", "--format=%(refname:short)", "refs/heads"])
        .lines()
        .map(str::to_string)
        .collect()
}

/// The confirmation key out of a refusal that issued one.
#[track_caller]
fn issued_key(stderr: &str) -> String {
    stderr
        .split("--key ")
        .nth(1)
        .unwrap_or_else(|| panic!("no key issued in:\n{stderr}"))
        .split_whitespace()
        .next()
        .unwrap()
        .to_string()
}

/// main -> group:feat -> group:sub, where sub is NOT ephemeral: a live sub
/// member blocks its parent's destroy outright.
const NESTED_CFG: &str = "[main]\n\
                          children = group:feat\n\
                          merge-mode = squash\n\
                          \n\
                          [group:feat]\n\
                          name-allow = feature/*\n\
                          children = group:sub\n\
                          \n\
                          [group:sub]\n\
                          name-allow = sub/*\n";

/// Same shape, but the descendants are ephemeral and may cascade.
const EPH_CFG: &str = "[main]\n\
                       children = group:mid\n\
                       \n\
                       [group:mid]\n\
                       name-allow = mid/*\n\
                       children = group:eph\n\
                       \n\
                       [group:eph]\n\
                       name-allow = eph/*\n\
                       children = group:eph\n\
                       ephemeral = true\n";

#[test]
fn destroy_clean_leaf_removes_worktree_and_branch() {
    let fx = Fixture::new();
    write_config(&fx, GROUP_CFG);
    let wt = member(&fx, "feature/a", "feat", "main");
    let o = run_wt(&wt, &["destroy"]);
    assert_ok(&o);
    assert!(out(&o).contains("destroyed worktree"), "{}", out(&o));
    assert!(out(&o).contains("Deleted branch feature/a"), "{}", out(&o));
    assert!(!wt.exists(), "worktree directory survived");
    assert_eq!(branches(&fx), vec!["main".to_string()]);
    // git no longer knows the worktree either
    assert!(!fx.git(&fx.repo, &["worktree", "list"]).contains("wtree-feature-a"));
}

#[test]
fn destroy_refuses_undestroyable_branch_even_with_force() {
    let fx = Fixture::new();
    write_config(
        &fx,
        "[main]\nchildren = dev\n\n[dev]\nchildren = group:feat\ndestroyable = false\n\n\
         [group:feat]\nname-allow = feature/*\n",
    );
    // In a linked worktree: the primary one is refused first, on grounds that
    // say nothing about the policy under test.
    assert_ok(&run_wt(&fx.repo, &["new", "dev"]));
    let dev = default_dest(&fx, "dev");
    for flags in [vec!["destroy"], vec!["destroy", "--force"]] {
        let o = run_wt(&dev, &flags);
        assert_fail(&o);
        let stderr = err(&o);
        assert!(stderr.contains("destroyable = false"), "{flags:?}: {stderr}");
        assert!(stderr.contains("--force cannot override"), "{flags:?}: {stderr}");
    }
    assert_eq!(branches(&fx), vec!["dev".to_string(), "main".into()]);
    assert!(dev.join("f.txt").exists());
}

#[test]
fn destroy_refuses_the_primary_worktree() {
    let fx = Fixture::new();
    write_config(&fx, GROUP_CFG);
    let o = run_wt(&fx.repo, &["destroy", "--force"]);
    assert_fail(&o);
    assert!(err(&o).contains("primary worktree"), "{}", err(&o));
    assert_eq!(branches(&fx), vec!["main".to_string()]);
    assert!(fx.repo.join("f.txt").exists());
}

#[test]
fn destroy_refuses_a_live_non_ephemeral_child_even_with_force() {
    let fx = Fixture::new();
    write_config(&fx, NESTED_CFG);
    let a = member(&fx, "feature/a", "feat", "main");
    let s = member(&fx, "sub/x", "sub", "feature/a");
    for flags in [vec!["destroy"], vec!["destroy", "--force"]] {
        let o = run_wt(&a, &flags);
        assert_fail(&o);
        let stderr = err(&o);
        assert!(stderr.contains("'sub/x'"), "{flags:?}: {stderr}");
        assert!(stderr.contains("not ephemeral"), "{flags:?}: {stderr}");
        assert!(stderr.contains("--force cannot override"), "{flags:?}: {stderr}");
    }
    assert!(a.is_dir() && s.is_dir());
    assert_eq!(branches(&fx).len(), 3);
}

#[test]
fn destroy_cascades_ephemeral_children_leaf_first() {
    let fx = Fixture::new();
    write_config(&fx, EPH_CFG);
    let mid = member(&fx, "mid/a", "mid", "main");
    let e1 = member(&fx, "eph/1", "eph", "mid/a");
    let e2 = member(&fx, "eph/2", "eph", "eph/1");
    let o = run_wt(&mid, &["destroy"]);
    assert_ok(&o);
    let stdout = out(&o);
    let at = |s: &str| stdout.find(s).unwrap_or_else(|| panic!("missing {s} in:\n{stdout}"));
    assert!(at("wtree-eph-2") < at("wtree-eph-1"), "leaf first:\n{stdout}");
    assert!(at("wtree-eph-1") < at("wtree-mid-a"), "children before the parent:\n{stdout}");
    assert!(!mid.exists() && !e1.exists() && !e2.exists());
    assert_eq!(branches(&fx), vec!["main".to_string()]);
}

#[test]
fn destroy_refuses_the_whole_cascade_when_one_child_is_dirty() {
    let fx = Fixture::new();
    write_config(&fx, EPH_CFG);
    let mid = member(&fx, "mid/a", "mid", "main");
    let e1 = member(&fx, "eph/1", "eph", "mid/a");
    let e2 = member(&fx, "eph/2", "eph", "eph/1");
    fx.make_dirty(&e2);
    let o = run_wt(&mid, &["destroy"]);
    assert_fail(&o);
    let stderr = err(&o);
    assert!(stderr.contains("'eph/2' — uncommitted changes"), "{stderr}");
    assert!(stderr.contains("--force cannot override"), "{stderr}");
    // nothing at all was removed: a cascade is all or nothing
    assert!(mid.is_dir() && e1.is_dir() && e2.is_dir());
    assert_eq!(branches(&fx).len(), 4);
}

#[test]
fn destroy_requires_the_confirmation_key_when_work_would_be_lost() {
    let fx = Fixture::new();
    write_config(&fx, GROUP_CFG);
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "unmerged work");
    fx.make_dirty(&wt);

    let o = run_wt(&wt, &["destroy"]);
    assert_fail(&o);
    let stderr = err(&o);
    assert!(stderr.contains("work-loss risk"), "{stderr}");
    assert!(
        stderr.contains("uncommitted changes, commits not reflected in parent"),
        "{stderr}"
    );
    let key = issued_key(&stderr);

    // --force is not a substitute for the key
    let o = run_wt(&wt, &["destroy", "--force"]);
    assert_fail(&o);
    assert!(err(&o).contains("--force cannot override"), "{}", err(&o));
    // nor is a wrong one
    let o = run_wt(&wt, &["destroy", "--key", "zzzzz"]);
    assert_fail(&o);
    assert!(err(&o).contains("confirmation key required"), "{}", err(&o));
    assert!(wt.is_dir());

    // the issued key goes through, dirty worktree and all
    let o = run_wt(&wt, &["destroy", "--key", &key]);
    assert_ok(&o);
    assert!(!wt.exists());
    assert_eq!(branches(&fx), vec!["main".to_string()]);
}

#[test]
fn destroy_after_a_squash_merge_needs_no_key() {
    let fx = Fixture::new();
    write_config(&fx, GROUP_CFG);
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "one");
    fx.commit(&wt, "two");
    // squashed into main by hand: the content is reflected, the ancestry is not
    fx.git(&fx.repo, &["merge", "--squash", "feature/a"]);
    fx.git(&fx.repo, &["commit", "-q", "-m", "squashed"]);
    commit_other(&fx, &fx.repo, "other.txt", "main moved on");
    assert!(
        !fx.git(&fx.repo, &["branch", "--merged", "main"]).contains("feature/a"),
        "the fixture must be a real squash, not an ancestry merge"
    );
    let o = run_wt(&wt, &["destroy"]);
    assert_ok(&o);
    assert!(!err(&o).contains("confirmation key"), "{}", err(&o));
    assert!(!wt.exists());
    assert_eq!(branches(&fx), vec!["main".to_string()]);
}

// ------------------------------------------------------------- open/close ----
//
// The pair that separates a worktree's life from its branch's: open attaches
// one to a branch that already exists, close takes it away and leaves the
// branch. Both used to require raw git.

/// main -> dev (fixed) -> group:feat, the shape where a fixed branch sits
/// between the root and the work branches.
const MIDDLE_CFG: &str = "[main]\n\
                          children = dev\n\
                          \n\
                          [dev]\n\
                          children = group:feat\n\
                          merge-mode = squash\n\
                          \n\
                          [group:feat]\n\
                          name-allow = feature/*\n";

#[test]
fn open_fixed_branch_creates_a_worktree_without_a_record() {
    let fx = Fixture::new();
    write_config(&fx, MIDDLE_CFG);
    fx.git(&fx.repo, &["branch", "dev", "main"]); // exists, has no worktree
    let o = run_wt(&fx.repo, &["open", "dev"]);
    assert_ok(&o);
    let dest = default_dest(&fx, "dev");
    let stdout = out(&o);
    assert!(stdout.contains("opened 'dev' (fixed)"), "{stdout}");
    assert!(stdout.contains(&format!("cd {}", dest.display())), "{stdout}");
    assert!(!stdout.contains("wtree adopt"), "a declared branch is managed already: {stdout}");
    assert_eq!(repo::head_branch(&dest).unwrap().as_deref(), Some("dev"));
    // [dev] IS the identity, so open writes nothing
    assert_eq!(state_read(&dest), StateRead::Missing);
    let info = out(&run_wt(&dest, &["info"]));
    assert!(info.contains("identity: fixed"), "{info}");
    assert!(info.contains("parent: main"), "{info}");
}

#[test]
fn open_unknown_branch_stays_unknown_until_adopted() {
    let fx = Fixture::new();
    write_config(&fx, ADOPT_CFG);
    fx.git(&fx.repo, &["branch", "feature/a", "main"]);
    let o = run_wt(&fx.repo, &["open", "feature/a"]);
    assert_ok(&o);
    let dest = default_dest(&fx, "feature/a");
    let stdout = out(&o);
    assert!(stdout.contains("opened 'feature/a' (unknown)"), "{stdout}");
    assert!(stdout.contains("wtree adopt"), "{stdout}");
    assert!(dest.is_dir());
    assert_eq!(state_read(&dest), StateRead::Missing);
    assert!(
        out(&run_wt(&dest, &["info"])).contains("identity: unknown"),
        "{}",
        out(&run_wt(&dest, &["info"]))
    );
    // the hint is the whole point: adopt runs in the worktree open just made
    assert_ok(&run_wt(&dest, &["adopt", "--group", "feat", "--parent", "main"]));
    let s = state_of(&dest);
    assert_eq!(s.branch, "feature/a");
    assert_eq!(s.kind, Kind::Group("feat".into()));
    assert!(out(&run_wt(&dest, &["info"])).contains("identity: group:feat"));
}

#[test]
fn open_and_new_point_at_each_other() {
    let fx = Fixture::new();
    write_config(&fx, GROUP_CFG);
    // open has no branch to attach to
    let o = run_wt(&fx.repo, &["open", "feature/ghost"]);
    assert_fail(&o);
    let stderr = err(&o);
    assert!(stderr.contains("wtree open: refused"), "{stderr}");
    assert!(stderr.contains("branch 'feature/ghost' does not exist"), "{stderr}");
    assert!(stderr.contains("wtree new feature/ghost"), "{stderr}");
    assert!(!default_dest(&fx, "feature/ghost").exists());

    let wt = member(&fx, "feature/a", "feat", "main");
    // new has a branch already, and open is what the caller meant
    let o = run_wt(&fx.repo, &["new", "feature/a"]);
    assert_fail(&o);
    let stderr = err(&o);
    assert!(stderr.contains("branch 'feature/a' already exists"), "{stderr}");
    assert!(stderr.contains("wtree open feature/a"), "{stderr}");

    // and that open is refused too, because the branch is checked out already
    let o = run_wt(&fx.repo, &["open", "feature/a"]);
    assert_fail(&o);
    let stderr = err(&o);
    assert!(stderr.contains("already checked out at"), "{stderr}");
    assert!(stderr.contains("wtree-feature-a"), "the path must be named: {stderr}");
    assert!(wt.is_dir());
}

#[test]
fn close_keeps_a_protected_fixed_branch() {
    let fx = Fixture::new();
    write_config(&fx, "[main]\nchildren = dev\n\n[dev]\ndestroyable = false\n");
    assert_ok(&run_wt(&fx.repo, &["new", "dev"]));
    let dev = default_dest(&fx, "dev");
    // destroy is refused unconditionally — close is the verb that exists for it
    let d = run_wt(&dev, &["destroy"]);
    assert_fail(&d);
    assert!(err(&d).contains("destroyable = false"), "{}", err(&d));

    let o = run_wt(&dev, &["close"]);
    assert_ok(&o);
    let stdout = out(&o);
    assert!(stdout.contains("closed worktree"), "{stdout}");
    assert!(stdout.contains("branch 'dev' is kept"), "{stdout}");
    assert!(!stdout.contains("unmanaged now"), "a declared branch stays managed: {stdout}");
    assert!(!dev.exists());
    assert_eq!(branches(&fx), vec!["dev".to_string(), "main".into()]);
    assert!(!fx.git(&fx.repo, &["worktree", "list"]).contains("repo.worktrees"));
}

#[test]
fn close_fixed_parent_still_receives_its_children() {
    let fx = Fixture::new();
    write_config(&fx, MIDDLE_CFG);
    assert_ok(&run_wt(&fx.repo, &["new", "dev"]));
    let dev = default_dest(&fx, "dev");
    assert_ok(&run_wt(&dev, &["new", "feature/a"]));
    let a = default_dest(&fx, "feature/a");

    // a live child does not block: [dev] holds dev in the tree without it
    assert_ok(&run_wt(&dev, &["close"]));
    assert!(!dev.exists());

    fx.commit(&a, "work");
    let o = run_wt(&a, &["merge", "-m", "feat: a"]);
    assert_ok(&o);
    assert!(out(&o).contains("merged 'feature/a' onto 'dev'"), "{}", out(&o));
    assert_eq!(rev(&fx, "dev"), rev(&fx, "feature/a"));
}

#[test]
fn close_refuses_a_group_branch_with_live_children() {
    let fx = Fixture::new();
    write_config(&fx, NESTED_CFG);
    let a = member(&fx, "feature/a", "feat", "main");
    let sub = member(&fx, "sub/x", "sub", "feature/a");
    let o = run_wt(&a, &["close"]);
    assert_fail(&o);
    let stderr = err(&o);
    assert!(stderr.contains("wtree close: refused"), "{stderr}");
    assert!(stderr.contains("orphans its children"), "{stderr}");
    assert!(stderr.contains("'sub/x'"), "{stderr}");
    assert!(a.is_dir() && sub.is_dir());

    // an ephemeral child blocks just the same: close never cascades, so it
    // would be left behind with an unmanaged parent
    let fx = Fixture::new();
    write_config(&fx, EPH_CFG);
    let mid = member(&fx, "mid/a", "mid", "main");
    member(&fx, "eph/1", "eph", "mid/a");
    let o = run_wt(&mid, &["close"]);
    assert_fail(&o);
    assert!(err(&o).contains("'eph/1'"), "{}", err(&o));
    assert!(mid.is_dir());
}

#[test]
fn close_group_branch_drops_its_record() {
    let fx = Fixture::new();
    write_config(&fx, GROUP_CFG);
    let wt = member(&fx, "feature/a", "feat", "main");
    let o = run_wt(&wt, &["close"]);
    assert_ok(&o);
    let stdout = out(&o);
    assert!(stdout.contains("branch 'feature/a' is kept"), "{stdout}");
    assert!(stdout.contains("unmanaged now"), "{stdout}");
    assert!(!wt.exists());
    assert_eq!(branches(&fx), vec!["feature/a".to_string(), "main".into()]);
    // the record lived in the worktree, so the branch reads as unknown now
    let l = run_wt(&fx.repo, &["list"]);
    assert_ok(&l);
    assert!(out(&l).contains("feature/a  UNKNOWN"), "{}", out(&l));
}

#[test]
fn close_dirty_worktree_needs_the_confirmation_key() {
    let fx = Fixture::new();
    write_config(&fx, GROUP_CFG);
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "unmerged work");
    fx.make_dirty(&wt);

    let o = run_wt(&wt, &["close"]);
    assert_fail(&o);
    let stderr = err(&o);
    assert!(stderr.contains("uncommitted changes go with the worktree"), "{stderr}");
    // the commits are not work loss here: the branch keeps them
    assert!(!stderr.contains("not reflected"), "{stderr}");
    let key = issued_key(&stderr);

    let o = run_wt(&wt, &["close", "--key", "zzzzz"]);
    assert_fail(&o);
    assert!(err(&o).contains("confirmation key required"), "{}", err(&o));
    assert!(wt.is_dir());

    let o = run_wt(&wt, &["close", "--key", &key]);
    assert_ok(&o);
    assert!(!wt.exists());
    // the branch and its commits survived the discarded edits
    assert_eq!(branches(&fx), vec!["feature/a".to_string(), "main".into()]);
    assert_ne!(rev(&fx, "feature/a"), rev(&fx, "main"));
}

#[test]
fn close_refuses_the_primary_worktree() {
    let fx = Fixture::new();
    write_config(&fx, GROUP_CFG);
    let o = run_wt(&fx.repo, &["close"]);
    assert_fail(&o);
    assert!(err(&o).contains("primary worktree"), "{}", err(&o));
    assert!(fx.repo.join("f.txt").exists());
    assert_eq!(branches(&fx), vec!["main".to_string()]);
}

#[test]
fn open_close_round_trip_keeps_a_fixed_branch_working() {
    let fx = Fixture::new();
    write_config(&fx, MIDDLE_CFG);
    assert_ok(&run_wt(&fx.repo, &["new", "dev"]));
    let dev = default_dest(&fx, "dev");
    assert_ok(&run_wt(&dev, &["close"]));
    assert!(!dev.exists());

    let o = run_wt(&fx.repo, &["open", "dev"]);
    assert_ok(&o);
    assert!(out(&o).contains("opened 'dev' (fixed)"), "{}", out(&o));
    assert!(dev.is_dir());
    assert_eq!(state_read(&dev), StateRead::Missing);
    // indistinguishable from the worktree `new` had made
    let info = out(&run_wt(&dev, &["info"]));
    assert!(info.contains("identity: fixed"), "{info}");
    assert!(info.contains("parent: main"), "{info}");
    assert_ok(&run_wt(&dev, &["new", "feature/a"]));
    assert_eq!(state_of(&default_dest(&fx, "feature/a")).parent, "dev");
}

#[test]
fn merge_and_sync_fail_closed_when_the_parent_became_unmanaged() {
    let fx = Fixture::new();
    write_config(
        &fx,
        "[main]\n\
         children = group:mid\n\
         \n\
         [group:mid]\n\
         name-allow = mid/*\n\
         children = group:leaf\n\
         merge-mode = squash\n\
         \n\
         [group:leaf]\n\
         name-allow = leaf/*\n",
    );
    let mid = member(&fx, "mid/a", "mid", "main");
    let leaf = member(&fx, "leaf/x", "leaf", "mid/a");
    fx.commit(&leaf, "work");
    let mid_before = rev(&fx, "mid/a");

    // while the parent is managed, its merge-mode rules the merge
    let o = run_wt(&leaf, &["merge", "--no-ff", "-m", "x"]);
    assert_fail(&o);
    assert!(err(&o).contains("accepts squash merges only"), "{}", err(&o));

    // raw `git worktree remove` — what there was before `close`, and what
    // `close` now refuses precisely because it strands the children
    assert!(err(&run_wt(&mid, &["close"])).contains("orphans its children"));
    fx.git(&fx.repo, &["worktree", "remove", mid.to_str().unwrap()]);

    // the parent's record went with it: its rules cannot be read, so nothing
    // that depends on them proceeds (before this check, --no-ff went through
    // because an unreadable merge-mode read as "no constraint")
    for flags in [
        vec!["merge", "--no-ff", "-m", "x"],
        vec!["merge", "--squash", "-m", "x"],
        vec!["sync"],
    ] {
        let o = run_wt(&leaf, &flags);
        assert_fail(&o);
        let stderr = err(&o);
        assert!(stderr.contains("parent 'mid/a' is unmanaged"), "{flags:?}: {stderr}");
        assert!(stderr.contains("fail closed"), "{flags:?}: {stderr}");
        assert!(stderr.contains("wtree open mid/a"), "{flags:?}: {stderr}");
    }
    assert_eq!(rev(&fx, "mid/a"), mid_before, "nothing may have landed");
    // info does not advertise the modes merge will refuse, either
    let info = out(&run_wt(&leaf, &["info"]));
    assert!(info.contains("merge to 'mid/a': no rules readable"), "{info}");

    // reopening and re-adopting the parent puts its rules back in force
    assert_ok(&run_wt(&fx.repo, &["open", "mid/a"]));
    let reopened = default_dest(&fx, "mid/a");
    assert_ok(&run_wt(&reopened, &["adopt", "--group", "mid", "--parent", "main"]));
    let o = run_wt(&leaf, &["merge", "--no-ff", "-m", "x"]);
    assert_fail(&o);
    assert!(err(&o).contains("accepts squash merges only"), "{}", err(&o));
    assert_ok(&run_wt(&leaf, &["merge", "--squash", "-m", "feat: x"]));
    assert_eq!(rev(&fx, "mid/a"), rev(&fx, "leaf/x"));
}

// -------------------------------------------------------------------- land ----

#[test]
fn land_merges_then_destroys() {
    let fx = Fixture::new();
    write_config(&fx, &merge_cfg("squash"));
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "one");
    fx.commit(&wt, "two");
    let before = rev(&fx, "main");
    let o = run_wt(&wt, &["land", "-m", "feat: a"]);
    assert_ok(&o);
    let stdout = out(&o);
    assert!(stdout.contains("merged 'feature/a' onto 'main'"), "{stdout}");
    assert!(!stdout.contains("worktree kept"), "land removes it: {stdout}");
    assert!(stdout.contains("destroyed worktree"), "{stdout}");
    let count = fx.git(&fx.repo, &["rev-list", "--count", &format!("{before}..main")]);
    assert_eq!(count.trim(), "1");
    assert_eq!(fx.git(&fx.repo, &["log", "-1", "--format=%s", "main"]).trim(), "feat: a");
    assert!(fs::read_to_string(fx.repo.join("f.txt")).unwrap().contains("two"));
    assert!(!wt.exists());
    assert_eq!(branches(&fx), vec!["main".to_string()]);
}

#[test]
fn land_preflight_refuses_before_merging() {
    let fx = Fixture::new();
    write_config(&fx, NESTED_CFG);
    let a = member(&fx, "feature/a", "feat", "main");
    let s = member(&fx, "sub/x", "sub", "feature/a");
    fx.commit(&a, "one");
    let main_before = rev(&fx, "main");
    let feat_before = rev(&fx, "feature/a");
    let o = run_wt(&a, &["land", "-m", "feat: a"]);
    assert_fail(&o);
    let stderr = err(&o);
    // attributed to the verb that was typed, not to the half that judged it
    assert!(stderr.contains("wtree land: refused"), "{stderr}");
    assert!(stderr.contains("'sub/x'"), "{stderr}");
    assert!(stderr.contains("not ephemeral"), "{stderr}");
    // the merge half never started
    assert_eq!(rev(&fx, "main"), main_before);
    assert_eq!(rev(&fx, "feature/a"), feat_before);
    assert!(!out(&o).contains("merged"), "{}", out(&o));
    assert!(a.is_dir() && s.is_dir());
}

#[test]
fn land_refuses_uncommitted_work_before_merging() {
    let fx = Fixture::new();
    write_config(&fx, &merge_cfg("squash"));
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "one");
    fx.make_dirty(&wt);
    let main_before = rev(&fx, "main");
    let o = run_wt(&wt, &["land", "-m", "feat: a"]);
    assert_fail(&o);
    let stderr = err(&o);
    assert!(stderr.contains("uncommitted changes"), "{stderr}");
    assert!(stderr.contains("`wtree merge` and then `wtree destroy`"), "{stderr}");
    assert_eq!(rev(&fx, "main"), main_before);
    assert!(wt.is_dir());
    assert!(wt.join("scratch.txt").exists());
}

#[test]
fn land_with_nothing_to_merge_goes_straight_to_destroy() {
    let fx = Fixture::new();
    write_config(&fx, &merge_cfg("squash"));
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "one");
    // the documented main flow: merge while working, land at the end
    assert_ok(&run_wt(&wt, &["merge", "-m", "feat: a"]));
    let merged = rev(&fx, "main");
    let o = run_wt(&wt, &["land", "-m", "feat: a"]);
    assert_ok(&o);
    assert!(
        out(&o).contains("nothing to merge onto 'main'; going straight to destroy"),
        "{}",
        out(&o)
    );
    assert_eq!(rev(&fx, "main"), merged, "main must not move again");
    assert!(!wt.exists());
    assert_eq!(branches(&fx), vec!["main".to_string()]);
}

// ------------------------------------------------------------ config gating ----

#[test]
fn verbs_require_init_and_valid_config() {
    let fx = Fixture::new();
    // no config yet
    for verb in ["list", "info", "new"] {
        let o = if verb == "new" {
            run_wt(&fx.repo, &["new", "feature/a"])
        } else {
            run_wt(&fx.repo, &[verb])
        };
        assert_fail(&o);
        assert!(err(&o).contains("run `wtree init` first"), "{verb}: {}", err(&o));
    }
    // a config with errors blocks execution, citing label:line
    write_config(&fx, "[main]\nchildren = group:ghost\n");
    let o = run_wt(&fx.repo, &["list"]);
    assert_fail(&o);
    let stderr = err(&o);
    assert!(stderr.contains("undeclared group 'group:ghost'"), "{stderr}");
    assert!(stderr.contains("(.git/wtree/config:2)"), "{stderr}");
}

#[test]
fn a_closed_reader_does_not_panic() {
    // Rust ignores SIGPIPE by default, which turns `wtree ... | head` into a
    // panic (exit 101) instead of the quiet death every unix tool performs.
    // Dropping the pipe before the verb reaches its first print reproduces it:
    // the child spends several git invocations gathering facts first.
    let fx = Fixture::new();
    write_config(&fx, "[main]\nchildren = group:g\n\n[group:g]\nname-allow = feature/*\n");
    let mut child = Command::new(env!("CARGO_BIN_EXE_wtree"))
        .current_dir(&fx.repo)
        .args(["info"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn wtree");
    drop(child.stdout.take());
    let status = child.wait().expect("wait failed");
    assert_ne!(status.code(), Some(101), "printing to a closed pipe panicked");
}

// -------------------------------------------------------------------- help ----

/// Every verb the menu can list, with an invocation that reaches the judgment
/// core. The flags are the permissive ones on purpose, and the probe name
/// matches the `name-allow` these fixtures use — a probe that trips over some
/// unrelated rule would pass the invariant below without testing it.
///
/// `open` is absent: it takes a branch rather than acting on the cwd, so
/// "hidden means refused" is a claim about every branch at once. The screen
/// that enumerates them is checked directly instead.
const ALL_VERBS: &[(&str, &[&str])] = &[
    ("new", &["new", "feature/probe"]),
    ("merge", &["merge", "--ff"]),
    ("sync", &["sync"]),
    ("land", &["land", "--ff"]),
    ("close", &["close"]),
    ("destroy", &["destroy", "--force"]),
    ("adopt", &["adopt", "--free", "--parent", "main"]),
];

/// The one direction that must not happen: a verb the menu hid, succeeding.
///
/// Showing a verb that then refuses is fine — the menu does not judge merge
/// conflicts or fast-forwardability, and the refusal explains itself. Hiding a
/// verb that would have worked is not: nothing else would ever mention it.
#[track_caller]
fn assert_hidden_verbs_refuse(dir: &Path, menu: &str) {
    for (verb, args) in ALL_VERBS {
        if menu.contains(&format!("  {verb} ")) || menu.contains(&format!("  {verb}\n")) {
            continue;
        }
        let o = run_wt(dir, args);
        assert!(
            !o.status.success(),
            "menu hid '{verb}', but `wtree {}` succeeded\nmenu:\n{menu}\nstdout:\n{}",
            args.join(" "),
            out(&o)
        );
    }
    if !menu.contains("  open ") {
        let e = err(&run_wt(dir, &["open"]));
        assert!(
            e.contains("no branch is waiting"),
            "menu hid 'open' while branches were waiting for a worktree:\n{e}"
        );
    }
}

#[test]
fn the_menu_lists_what_policy_allows_and_hides_what_it_refuses() {
    let fx = Fixture::new();
    write_config(
        &fx,
        "[main]\nchildren = group:feat\ndestroyable = false\n\n\
         [group:feat]\nname-allow = feature/*\nmerge-mode = squash\n",
    );
    // Root, protected, primary worktree: almost everything is out of reach.
    let o = run_wt(&fx.repo, &[]);
    assert_ok(&o);
    let menu = out(&o);
    assert!(menu.starts_with("main (fixed)"), "{menu}");
    assert!(menu.contains("new <name>"), "{menu}");
    for hidden in ["merge", "sync", "land", "close", "destroy"] {
        assert!(!menu.contains(&format!("  {hidden}")), "'{hidden}' in:\n{menu}");
    }
    // list/info are unconditional, and the hint names the way to the rest
    assert!(menu.contains("  list "), "{menu}");
    assert!(menu.contains("  info "), "{menu}");
    assert!(menu.contains("wtree help --all"), "{menu}");
    assert_hidden_verbs_refuse(&fx.repo, &menu);

    // A group member: the leaf where the work happens, so nearly everything is.
    assert_ok(&run_wt(&fx.repo, &["new", "feature/a"]));
    let wt = default_dest(&fx, "feature/a");
    let menu = out(&run_wt(&wt, &[]));
    assert!(menu.starts_with("feature/a (group:feat)"), "{menu}");
    for shown in ["merge", "sync", "land", "close", "destroy", "adopt"] {
        assert!(menu.contains(&format!("  {shown}")), "'{shown}' missing:\n{menu}");
    }
    // [group:feat] declares no children, so nothing forks from here
    assert!(!menu.contains("new <name>"), "{menu}");
    assert_hidden_verbs_refuse(&wt, &menu);
}

#[test]
fn the_menu_spells_merge_modes_but_never_key_or_force() {
    let fx = Fixture::new();
    // main takes one mode, so merge names it; the group takes two, so the
    // flag becomes a choice the menu has to show.
    write_config(
        &fx,
        "[main]\nchildren = group:feat\nmerge-mode = ff\n\n\
         [group:feat]\nchildren = group:sub\nname-allow = feature/*\nmerge-mode = squash, no-ff\n\n\
         [group:sub]\nname-allow = sub/*\n",
    );
    assert_ok(&run_wt(&fx.repo, &["new", "feature/a"]));
    let parent = default_dest(&fx, "feature/a");
    assert_ok(&run_wt(&parent, &["new", "sub/x"]));
    let child = default_dest(&fx, "sub/x");

    assert!(out(&run_wt(&parent, &[])).contains("merge --ff"), "one mode is named");
    let menu = out(&run_wt(&child, &[]));
    assert!(menu.contains("merge [--squash|--no-ff]"), "{menu}");
    assert!(menu.contains("land [--squash|--no-ff]"), "{menu}");

    // Dirty: destroy still shows (the key is not a veto), but the menu does not
    // teach --key, and land goes away entirely — it refuses uncommitted work.
    fs::write(child.join("dirty.txt"), "x").unwrap();
    let menu = out(&run_wt(&child, &[]));
    assert!(menu.contains("  destroy"), "{menu}");
    assert!(!menu.contains("land"), "{menu}");
    assert!(!menu.contains("--key"), "{menu}");
    assert!(!menu.contains("--force"), "{menu}");
    // and the refusal is where the key is introduced
    let e = err(&run_wt(&child, &["destroy"]));
    assert!(e.contains("confirmation key required"), "{e}");
}

#[test]
fn an_unmanaged_worktree_offers_only_the_way_back() {
    let fx = Fixture::new();
    write_config(&fx, GROUP_CFG);
    fx.git(&fx.repo, &["branch", "stray", "main"]);
    assert_ok(&run_wt(&fx.repo, &["open", "stray"]));
    let wt = default_dest(&fx, "stray");
    let menu = out(&run_wt(&wt, &[]));
    assert!(menu.starts_with("stray (unmanaged)"), "{menu}");
    assert!(menu.contains("adopt (--group G | --free) --parent P"), "{menu}");
    assert_hidden_verbs_refuse(&wt, &menu);
}

#[test]
fn open_and_new_answer_a_missing_argument_with_what_they_accept() {
    let fx = Fixture::new();
    write_config(
        &fx,
        "[main]\nchildren = group:feat, group:any, dev\n\n\
         [group:feat]\nname-allow = feature/*\nname-deny = feature/tmp-*\n\n[group:any]\n\n[dev]\n",
    );
    fx.git(&fx.repo, &["branch", "loose", "main"]);

    let o = run_wt(&fx.repo, &["new"]);
    assert_eq!(o.status.code(), Some(2), "a missing name is still a usage error");
    assert!(out(&o).is_empty(), "usage errors do not go to stdout");
    let e = err(&o);
    assert!(e.contains("usage: wtree new <name>"), "{e}");
    assert!(e.contains("--group feat") && e.contains("feature/*"), "{e}");
    assert!(e.contains("except feature/tmp-*"), "{e}");
    assert!(e.contains("--group any") && e.contains("any name"), "{e}");
    assert!(e.contains("dev") && e.contains("fixed branch"), "{e}");

    let o = run_wt(&fx.repo, &["open"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(out(&o).is_empty(), "usage errors do not go to stdout");
    let e = err(&o);
    assert!(e.contains("usage: wtree open <branch>"), "{e}");
    // 'loose' has no worktree and no declaration; 'main' has one already
    assert!(e.contains("loose") && e.contains("unmanaged until adopted"), "{e}");
    assert!(!e.contains("\n  main"), "main is checked out here: {e}");

    // Everything opened: the screen says so rather than showing an empty list.
    assert_ok(&run_wt(&fx.repo, &["open", "loose"]));
    let e = err(&run_wt(&fx.repo, &["open"]));
    assert!(e.contains("no branch is waiting for a worktree"), "{e}");
}

#[test]
fn the_manual_needs_neither_a_repo_nor_a_readable_config() {
    let fx = Fixture::new();
    // Broken beyond loading: the moment a user most needs to look a verb up.
    write_config(&fx, "[main]\nchildren = group:ghost\nbogus-key = 1\n");
    let o = run_wt(&fx.repo, &["help", "--all"]);
    assert_ok(&o);
    let stdout = out(&o);
    for verb in ["new", "open", "close", "merge", "sync", "land", "destroy", "adopt", "init"] {
        assert!(stdout.contains(&format!("wtree {verb}")), "'{verb}' missing:\n{stdout}");
    }
    // ... and outside a git repo entirely
    let o = run_wt(&fx.tmp.0, &["help", "--all"]);
    assert_ok(&o);
    assert!(out(&o).contains("wtree merge"));

    // The contextual menu cannot be built from that config, and says so.
    assert_fail(&run_wt(&fx.repo, &[]));
}

#[test]
fn a_non_utf8_argument_is_refused_instead_of_panicking() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let fx = Fixture::new();
    let o = Command::new(env!("CARGO_BIN_EXE_wtree"))
        .current_dir(&fx.repo)
        .arg("open")
        .arg(OsStr::from_bytes(b"feature/\xff"))
        .output()
        .expect("failed to spawn wtree");
    // Invalid encoding is a usage error, not an internal failure.
    assert_eq!(o.status.code(), Some(2), "stderr:\n{}", err(&o));
    assert!(err(&o).contains("must be valid UTF-8"), "{}", err(&o));
}

#[test]
fn an_uninitialized_repo_is_pointed_at_init() {
    let fx = Fixture::new();
    let o = run_wt(&fx.repo, &[]);
    assert_ok(&o);
    let stdout = out(&o);
    assert!(stdout.contains("no wtree policy yet"), "{stdout}");
    assert!(stdout.contains("init"), "{stdout}");
}
