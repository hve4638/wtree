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

fn write_rules(fx: &Fixture, text: &str) {
    let dir = fx.repo.join(".git/wtree");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("rules"), text).unwrap();
}

/// Default placement base used by `wtree new`: `<tmp>/repo.worktrees`.
fn default_dest(fx: &Fixture, branch: &str) -> PathBuf {
    fx.tmp
        .0
        .join("repo.worktrees")
        .join(branch.replace('/', "-"))
}

const GROUP_CFG: &str = "[main]\nchildren = group:feat\n\n[group:feat]\nname-allow = feature/*\n";

// -------------------------------------------------------------------- init ----

#[test]
fn init_creates_template_and_refuses_rerun() {
    let fx = Fixture::new();
    let o = run_wt(&fx.repo, &["init", "--new"]);
    assert_ok(&o);
    let cfg_path = fx.repo.join(".git/wtree/rules");
    let text = fs::read_to_string(&cfg_path).unwrap();
    assert!(text.contains("[main]"), "{text}");
    assert!(text.contains("destroyable = false"), "{text}");
    assert!(text.contains("# children = group:work"), "{text}");
    assert!(text.contains("# name-allow = feat/*, fix/*"), "{text}");
    assert!(fx.repo.join(".git/wtree/hooks").is_dir());
    // the template must load clean
    let check = run_wt(&fx.repo, &["check", cfg_path.to_str().unwrap()]);
    assert_ok(&check);
    // re-run refused, rules untouched
    let o2 = run_wt(&fx.repo, &["init", "--new"]);
    assert_fail(&o2);
    assert!(err(&o2).contains("already has rules"), "{}", err(&o2));
    assert_eq!(fs::read_to_string(&cfg_path).unwrap(), text);
}

/// The two knobs `init` cannot prefill with anything useful still have to be
/// discoverable, so it leaves a commented file at each. Neither may take effect
/// on its own: the settings file must load as defaults, and the hook samples
/// must not be found by `new` (hence git's `.sample` spelling — a file named
/// `post-create` would warn about not being executable on every run).
#[test]
fn init_seeds_a_commented_settings_file_and_inert_hook_samples() {
    let fx = Fixture::new();
    assert_ok(&run_wt(&fx.repo, &["init", "--new"]));

    let sett = fs::read_to_string(fx.repo.join(".git/wtree/settings")).unwrap();
    assert!(
        sett.contains("# worktree-dir"),
        "the knob is shown, not set:\n{sett}"
    );
    assert!(
        !sett
            .lines()
            .any(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty()),
        "every line must be a comment:\n{sett}"
    );

    for hook in [
        "pre-create",
        "post-create",
        "pre-merge",
        "post-merge",
        "pre-destroy",
        "post-destroy",
    ] {
        let sample = fx.repo.join(format!(".git/wtree/hooks/{hook}.sample"));
        assert!(sample.is_file(), "{hook} sample written");
        assert!(
            !fx.repo.join(".git/wtree/hooks").join(hook).exists(),
            "{hook} not enabled"
        );
        assert_ne!(
            fs::metadata(&sample).unwrap().permissions().mode() & 0o111,
            0,
            "already executable, so enabling {hook} is a rename"
        );
    }

    // The defaults still apply and nothing warns about the sample.
    write_rules(
        &fx,
        "[main]\nchildren = group:feat\n\n[group:feat]\nname-allow = feature/*\n",
    );
    let o = run_wt(&fx.repo, &["new", "feature/a"]);
    assert_ok(&o);
    assert!(
        !err(&o).contains("hook"),
        "the sample must not be found:\n{}",
        err(&o)
    );
    assert!(
        default_dest(&fx, "feature/a").is_dir(),
        "default placement unchanged"
    );
}

/// `init` is guarded on either file, not just the rules: a settings written by
/// hand is someone's work, and replacing it silently because there happened to
/// be no rules next to it is the same loss.
#[test]
fn init_refuses_a_settings_file_that_was_written_by_hand() {
    let fx = Fixture::new();
    let sett = fx.repo.join(".git/wtree/settings");
    fs::create_dir_all(sett.parent().unwrap()).unwrap();
    fs::write(&sett, "worktree-dir = ../mine\n").unwrap();

    let o = run_wt(&fx.repo, &["init", "--new"]);
    assert_fail(&o);
    assert!(err(&o).contains("already has settings"), "{}", err(&o));
    assert_eq!(
        fs::read_to_string(&sett).unwrap(),
        "worktree-dir = ../mine\n",
        "a refusal writes nothing"
    );
}

#[test]
fn init_root_detection_order() {
    // origin/HEAD symref wins
    let fx = Fixture::new();
    fx.git(
        &fx.repo,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/trunk",
        ],
    );
    assert_ok(&run_wt(&fx.repo, &["init", "--new"]));
    let text = fs::read_to_string(fx.repo.join(".git/wtree/rules")).unwrap();
    assert!(text.contains("[trunk]"), "{text}");

    // no origin/HEAD: master existence
    let fx = Fixture::new();
    fx.git(&fx.repo, &["branch", "-m", "master"]);
    assert_ok(&run_wt(&fx.repo, &["init", "--new"]));
    let text = fs::read_to_string(fx.repo.join(".git/wtree/rules")).unwrap();
    assert!(text.contains("[master]"), "{text}");

    // no origin/HEAD, no main/master: current branch
    let fx = Fixture::new();
    fx.git(&fx.repo, &["branch", "-m", "work"]);
    assert_ok(&run_wt(&fx.repo, &["init", "--new"]));
    let text = fs::read_to_string(fx.repo.join(".git/wtree/rules")).unwrap();
    assert!(text.contains("[work]"), "{text}");
}

#[test]
fn init_refuses_bare_repo() {
    let fx = Fixture::new();
    let bare = fx.tmp.0.join("bare.git");
    fs::create_dir_all(&bare).unwrap();
    fx.git(&bare, &["init", "-q", "--bare"]);
    let o = run_wt(&bare, &["init", "--new"]);
    assert_fail(&o);
    assert!(err(&o).contains("bare"), "{}", err(&o));
}

// ------------------------------------------------------- init --load / save ----

/// A committed `.wtree/` under `root`.
fn write_seed(root: &Path, rules: Option<&str>, settings: Option<&str>) -> PathBuf {
    let dir = root.join(".wtree");
    fs::create_dir_all(&dir).unwrap();
    if let Some(t) = rules {
        fs::write(dir.join("rules"), t).unwrap();
    }
    if let Some(t) = settings {
        fs::write(dir.join("settings"), t).unwrap();
    }
    dir
}

#[test]
fn load_takes_both_files_from_a_named_directory() {
    let fx = Fixture::new();
    let seed = write_seed(
        &fx.repo,
        Some(GROUP_CFG),
        Some("worktree-dir = ../shared\n"),
    );

    let o = run_wt(&fx.repo, &["init", "--load", seed.to_str().unwrap()]);
    assert_ok(&o);
    assert_eq!(
        fs::read_to_string(fx.repo.join(".git/wtree/rules")).unwrap(),
        GROUP_CFG
    );
    assert_eq!(
        fs::read_to_string(fx.repo.join(".git/wtree/settings")).unwrap(),
        "worktree-dir = ../shared\n"
    );
    // hooks are never part of a load, but the sample still gets seeded
    assert!(
        fx.repo
            .join(".git/wtree/hooks/post-create.sample")
            .is_file()
    );
    assert!(
        !seed.join("hooks").exists(),
        "load reads, it does not write"
    );
}

/// The file that is there is taken; the one that is not falls back to the
/// template, which is what makes a `.wtree/` holding only rules useful.
#[test]
fn load_fills_the_missing_half_from_the_template() {
    let fx = Fixture::new();
    let seed = write_seed(&fx.repo, Some(GROUP_CFG), None);

    assert_ok(&run_wt(
        &fx.repo,
        &["init", "--load", seed.to_str().unwrap()],
    ));
    assert_eq!(
        fs::read_to_string(fx.repo.join(".git/wtree/rules")).unwrap(),
        GROUP_CFG
    );
    let sett = fs::read_to_string(fx.repo.join(".git/wtree/settings")).unwrap();
    assert!(sett.contains("# worktree-dir"), "{sett}");
}

/// Validation runs before anything is written, so a source that does not parse
/// costs nothing: no rules, no settings, no `.git/wtree` at all.
#[test]
fn load_refuses_broken_rules_and_writes_nothing() {
    let fx = Fixture::new();
    let seed = write_seed(&fx.repo, Some("[branch dev]\nnonsense = 1\n"), None);

    let o = run_wt(&fx.repo, &["init", "--load", seed.to_str().unwrap()]);
    assert_fail(&o);
    assert!(err(&o).contains("nothing was written"), "{}", err(&o));
    assert!(err(&o).contains("old section syntax"), "{}", err(&o));
    assert!(
        !fx.repo.join(".git/wtree/rules").exists(),
        "the repo must be untouched"
    );
    assert!(!fx.repo.join(".git/wtree/settings").exists());
}

#[test]
fn load_refuses_a_wtree_dir_with_nothing_in_it() {
    let fx = Fixture::new();
    let seed = write_seed(&fx.repo, None, None);

    let o = run_wt(&fx.repo, &["init", "--load", seed.to_str().unwrap()]);
    assert_fail(&o);
    assert!(
        err(&o).contains("no rules or settings to load"),
        "{}",
        err(&o)
    );
}

/// In the main worktree the two candidate roots are the same directory, so a
/// bare `--load` has exactly one answer and takes it.
#[test]
fn bare_load_uses_the_wtree_of_the_worktree_you_are_in() {
    let fx = Fixture::new();
    write_seed(&fx.repo, Some(GROUP_CFG), None);

    assert_ok(&run_wt(&fx.repo, &["init", "--load"]));
    assert_eq!(
        fs::read_to_string(fx.repo.join(".git/wtree/rules")).unwrap(),
        GROUP_CFG
    );
}

/// Resolved from the worktree root, not the cwd, so how deep you are standing
/// makes no difference.
#[test]
fn bare_load_works_from_a_subdirectory() {
    let fx = Fixture::new();
    write_seed(&fx.repo, Some(GROUP_CFG), None);
    let deep = fx.repo.join("src/inner");
    fs::create_dir_all(&deep).unwrap();

    assert_ok(&run_wt(&deep, &["init", "--load"]));
    assert_eq!(
        fs::read_to_string(fx.repo.join(".git/wtree/rules")).unwrap(),
        GROUP_CFG
    );
}

/// Two candidates cannot be told apart without asking, and `--load` has no way
/// to ask — so it refuses and hands over both commands it could not choose
/// between. Same when the only `.wtree/` belongs to the other worktree:
/// reaching across for it would be a guess about which branch's policy wins.
#[test]
fn bare_load_refuses_every_answer_it_would_have_to_guess() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("feat/x", "main");
    write_seed(&fx.repo, Some(GROUP_CFG), None);

    // main has one, this worktree does not
    let o = run_wt(&wt, &["init", "--load"]);
    assert_fail(&o);
    assert!(
        err(&o).contains("no .wtree/ in this worktree"),
        "{}",
        err(&o)
    );
    assert!(err(&o).contains("main worktree"), "{}", err(&o));
    assert!(
        err(&o).contains(fx.repo.join(".wtree").to_str().unwrap()),
        "the refusal spells the command that works:\n{}",
        err(&o)
    );

    // both have one
    write_seed(&wt, Some(GROUP_CFG), None);
    let o = run_wt(&wt, &["init", "--load"]);
    assert_fail(&o);
    assert!(err(&o).contains("ambiguous"), "{}", err(&o));
    assert!(err(&o).contains("this worktree"), "{}", err(&o));
    assert!(err(&o).contains("main worktree"), "{}", err(&o));

    // neither
    let fx2 = Fixture::new();
    let o = run_wt(&fx2.repo, &["init", "--load"]);
    assert_fail(&o);
    assert!(
        err(&o).contains("no .wtree/ in this worktree or the main worktree"),
        "{}",
        err(&o)
    );
}

fn backups(fx: &Fixture) -> Vec<PathBuf> {
    let mut b: Vec<PathBuf> = fs::read_dir(fx.repo.join(".git/wtree/.backup"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    b.sort();
    b
}

/// `--force` moves what it replaces into `.backup/<UTC>/` rather than dropping
/// it, and the backup itself never becomes part of the next one.
#[test]
fn force_backs_up_what_it_replaces() {
    let fx = Fixture::new();
    assert_ok(&run_wt(&fx.repo, &["init", "--new"]));
    let before = fs::read_to_string(fx.repo.join(".git/wtree/rules")).unwrap();
    let seed = write_seed(
        &fx.repo,
        Some(GROUP_CFG),
        Some("worktree-dir = ../shared\n"),
    );

    let o = run_wt(&fx.repo, &["init", "--load", seed.to_str().unwrap()]);
    assert_fail(&o);
    assert!(err(&o).contains("--force"), "{}", err(&o));

    let o = run_wt(
        &fx.repo,
        &["init", "--load", seed.to_str().unwrap(), "--force"],
    );
    assert_ok(&o);
    assert_eq!(
        fs::read_to_string(fx.repo.join(".git/wtree/rules")).unwrap(),
        GROUP_CFG
    );

    let kept = &backups(&fx)[0];
    assert_eq!(backups(&fx).len(), 1);
    assert_eq!(fs::read_to_string(kept.join("rules")).unwrap(), before);
    assert!(kept.join("settings").is_file(), "both were replaced");
    assert!(!kept.join(".backup").exists(), "a backup never nests");
    assert!(!kept.join("hooks").exists(), "hooks are not backed up");
    assert!(
        fx.repo
            .join(".git/wtree/hooks/post-create.sample")
            .is_file(),
        "and they survive in place"
    );
}

/// `worktree-dir` is this machine's, and a `.wtree/` carrying only rules says
/// nothing about it. Loading the rules must not quietly move where every later
/// `wtree new` puts its worktrees.
#[test]
fn a_load_without_settings_leaves_the_local_settings_alone() {
    let fx = Fixture::new();
    assert_ok(&run_wt(&fx.repo, &["init", "--new"]));
    let sett = fx.repo.join(".git/wtree/settings");
    fs::write(&sett, "worktree-dir = ../mine\n").unwrap();
    let seed = write_seed(&fx.repo, Some(GROUP_CFG), None);

    let o = run_wt(
        &fx.repo,
        &["init", "--load", seed.to_str().unwrap(), "--force"],
    );
    assert_ok(&o);
    assert_eq!(
        fs::read_to_string(&sett).unwrap(),
        "worktree-dir = ../mine\n",
        "the machine-local file is not the seed's to replace"
    );
    assert!(out(&o).contains("left as it was"), "{}", out(&o));

    // and what was never replaced was never backed up either
    let kept = &backups(&fx)[0];
    assert!(kept.join("rules").is_file());
    assert!(!kept.join("settings").exists(), "{:?}", kept);
}

/// Backing up costs nothing when there is nothing there, so `--force` on a
/// fresh repo is not an error — just a flag that had no work to do.
#[test]
fn force_with_nothing_to_replace_is_not_an_error() {
    let fx = Fixture::new();
    let o = run_wt(&fx.repo, &["init", "--new", "--force"]);
    assert_ok(&o);
    assert!(!fx.repo.join(".git/wtree/.backup").exists());
}

/// The interactive path is the only one that can ask, and it needs a terminal
/// to ask on. Tests run on pipes, which is exactly the case that has to fail
/// fast rather than block on a prompt nobody can see.
#[test]
fn init_without_a_terminal_names_the_two_flags() {
    let fx = Fixture::new();
    let o = run_wt(&fx.repo, &["init"]);
    assert_fail(&o);
    assert_eq!(o.status.code(), Some(2));
    assert!(err(&o).contains("no terminal"), "{}", err(&o));
    assert!(err(&o).contains("wtree init --new"), "{}", err(&o));
    assert!(err(&o).contains("wtree init --load"), "{}", err(&o));
    assert!(!fx.repo.join(".git/wtree/rules").exists());
}

#[test]
fn init_flag_combinations() {
    let fx = Fixture::new();
    for (args, needle) in [
        (vec!["init", "--new", "--load"], "mutually exclusive"),
        (vec!["init", "--force"], "--force only applies"),
        (vec!["init", "--nope"], "unknown argument"),
    ] {
        let o = run_wt(&fx.repo, &args);
        assert_fail(&o);
        assert_eq!(o.status.code(), Some(2), "{args:?}");
        assert!(err(&o).contains(needle), "{args:?}: {}", err(&o));
    }
}

#[test]
fn save_round_trips_through_a_committed_wtree_dir() {
    let fx = Fixture::new();
    assert_ok(&run_wt(&fx.repo, &["init", "--new"]));
    write_rules(&fx, GROUP_CFG);

    let o = run_wt(&fx.repo, &["save"]);
    assert_ok(&o);
    let seed = fx.repo.join(".wtree");
    assert_eq!(fs::read_to_string(seed.join("rules")).unwrap(), GROUP_CFG);
    assert!(seed.join("settings").is_file());
    assert!(!seed.join("hooks").exists(), "hooks stay behind");

    // a second save refuses; --force replaces without keeping a backup
    let o = run_wt(&fx.repo, &["save"]);
    assert_fail(&o);
    assert!(err(&o).contains("--force"), "{}", err(&o));
    write_rules(&fx, "[main]\ndestroyable = false\n");
    assert_ok(&run_wt(&fx.repo, &["save", "--force"]));
    assert_eq!(
        fs::read_to_string(seed.join("rules")).unwrap(),
        "[main]\ndestroyable = false\n"
    );
    assert!(
        !seed.join(".backup").exists(),
        "git already holds the old one"
    );

    // and what it wrote loads back
    let fx2 = Fixture::new();
    let carried = write_seed(
        &fx2.repo,
        Some(&fs::read_to_string(seed.join("rules")).unwrap()),
        None,
    );
    assert_ok(&run_wt(
        &fx2.repo,
        &["init", "--load", carried.to_str().unwrap()],
    ));
}

#[test]
fn save_writes_where_you_are_standing_or_where_you_say() {
    let fx = Fixture::new();
    assert_ok(&run_wt(&fx.repo, &["init", "--new"]));
    write_rules(&fx, GROUP_CFG);
    let wt = fx.add_worktree("feat/x", "main");

    // the linked worktree gets its own copy, so the commit lands on its branch
    assert_ok(&run_wt(&wt, &["save"]));
    assert!(wt.join(".wtree/rules").is_file());
    assert!(!fx.repo.join(".wtree").exists());

    // an explicit path wins, resolved against the cwd
    assert_ok(&run_wt(&fx.repo, &["save", ".wtree.strategy-b"]));
    assert!(fx.repo.join(".wtree.strategy-b/rules").is_file());
}

/// A settings left over from an earlier save would keep travelling with rules
/// it no longer belongs to, so `--force` clears it rather than stepping over it.
#[test]
fn save_force_clears_a_settings_this_clone_no_longer_has() {
    let fx = Fixture::new();
    assert_ok(&run_wt(&fx.repo, &["init", "--new"]));
    write_rules(&fx, GROUP_CFG);
    assert_ok(&run_wt(&fx.repo, &["save"]));
    assert!(fx.repo.join(".wtree/settings").is_file());

    fs::remove_file(fx.repo.join(".git/wtree/settings")).unwrap();
    let o = run_wt(&fx.repo, &["save", "--force"]);
    assert_ok(&o);
    assert!(
        !fx.repo.join(".wtree/settings").exists(),
        "the stale copy is gone"
    );
    assert!(out(&o).contains("settings removed"), "{}", out(&o));
}

#[test]
fn save_refuses_rules_that_do_not_parse() {
    let fx = Fixture::new();
    write_rules(&fx, "[branch dev]\n");
    let o = run_wt(&fx.repo, &["save"]);
    assert_fail(&o);
    assert!(err(&o).contains("nothing was written"), "{}", err(&o));
    assert!(!fx.repo.join(".wtree").exists());
}

#[test]
fn save_before_init_says_so() {
    let fx = Fixture::new();
    let o = run_wt(&fx.repo, &["save"]);
    assert_fail(&o);
    assert!(err(&o).contains("wtree init"), "{}", err(&o));
}

// --------------------------------------------------------------------- new ----

#[test]
fn new_group_member_records_state_at_default_placement() {
    let fx = Fixture::new();
    write_rules(&fx, GROUP_CFG);
    let o = run_wt(&fx.repo, &["new", "feature/a"]);
    assert_ok(&o);
    let dest = default_dest(&fx, "feature/a");
    assert!(dest.is_dir(), "worktree missing at {}", dest.display());
    let stdout = out(&o);
    assert!(stdout.contains("group:feat"), "{stdout}");
    assert!(
        stdout.contains(&format!("cd {}", dest.display())),
        "{stdout}"
    );
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
    write_rules(&fx, "[main]\nchildren = dev\n\n[dev]\n");
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
    write_rules(&fx, GROUP_CFG);
    let o = run_wt(&fx.repo, &["new", "junk/x"]);
    assert_fail(&o);
    let stderr = err(&o);
    assert!(stderr.contains("refusal: new "), "{stderr}");
    assert!(stderr.contains("does not match name-allow"), "{stderr}");
    assert!(stderr.contains("rule: name-allow"), "{stderr}");
    // neither a worktree nor a branch was created
    assert!(!default_dest(&fx, "junk/x").exists());
    let refs = fx.git(
        &fx.repo,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
    );
    assert_eq!(refs.trim(), "main");
}

/// `new` names one branch. A second name is a mistake, not a second worktree.
#[test]
fn new_refuses_a_second_name() {
    let fx = Fixture::new();
    write_rules(&fx, GROUP_CFG);

    let o = run_wt(&fx.repo, &["new", "feature/a", "feature/b"]);
    assert_eq!(o.status.code(), Some(2), "{}", err(&o));
    assert!(err(&o).contains("unexpected extra argument"), "{}", err(&o));
    assert!(err(&o).contains("usage: wtree new"), "{}", err(&o));
    assert_eq!(branches(&fx), vec!["main".to_string()], "{}", err(&o));
    assert!(!default_dest(&fx, "feature/a").exists(), "a was created");
}

/// `feature/a` nests under nothing until `feature/a/b` exists, and then git
/// cannot hold both. The refusal comes from the policy layer — `fatal: cannot
/// lock ref` never reaches the reader.
#[test]
fn new_refuses_a_name_that_nests_as_a_ref() {
    let fx = Fixture::new();
    write_rules(&fx, GROUP_CFG);
    assert_ok(&run_wt(&fx.repo, &["new", "feature/a"]));

    let o = run_wt(&fx.repo, &["new", "feature/a/b"]);
    assert_fail(&o);
    assert!(err(&o).starts_with("refusal:"), "{}", err(&o));
    assert!(!err(&o).contains("fatal:"), "{}", err(&o));

    // Sharing a prefix is not the same as nesting under one.
    assert_ok(&run_wt(&fx.repo, &["new", "feature/ab"]));
}

/// The occupant is named, because "already exists" fits three situations that
/// want three different things done about them.
#[test]
fn new_names_what_occupies_the_destination() {
    let fx = Fixture::new();
    write_rules(
        &fx,
        "[main]\nchildren = group:feat\n\n[group:feat]\nname-allow = feature/*, feature-*\n",
    );
    assert_ok(&run_wt(&fx.repo, &["new", "feature/x"]));

    // Managed: the branch that folded onto the same directory.
    let o = run_wt(&fx.repo, &["new", "feature-x"]);
    assert_fail(&o);
    assert!(
        err(&o).contains("conflict: branch 'feature/x' occupies it"),
        "{}",
        err(&o)
    );
    assert!(err(&o).contains("--dir <name>"), "{}", err(&o));

    // Unmanaged: still a branch, and still named — being outside the policy is
    // a note on it, not a reason to withhold it.
    let raw = default_dest(&fx, "feature-raw");
    fx.git(
        &fx.repo,
        &["worktree", "add", "-q", raw.to_str().unwrap(), "-b", "odd"],
    );
    let o = run_wt(&fx.repo, &["new", "feature-raw"]);
    assert_fail(&o);
    assert!(
        err(&o).contains("conflict: branch 'odd' (unmanaged) occupies it"),
        "{}",
        err(&o)
    );

    // A plain directory is only in the way; there is nothing to name.
    fs::create_dir_all(default_dest(&fx, "feature-plain")).unwrap();
    let o = run_wt(&fx.repo, &["new", "feature-plain"]);
    assert_fail(&o);
    assert!(err(&o).contains("already exists"), "{}", err(&o));
    assert!(!err(&o).contains("conflict:"), "{}", err(&o));
}

/// `--dir` relabels one worktree. It takes a name and not a path: the policy
/// keeps deciding where worktrees live.
#[test]
fn new_dir_renames_the_directory_but_not_the_place() {
    let fx = Fixture::new();
    write_rules(&fx, GROUP_CFG);
    assert_ok(&run_wt(&fx.repo, &["new", "feature/a"]));

    let o = run_wt(&fx.repo, &["new", "feature/b", "--dir", "elsewhere"]);
    assert_ok(&o);
    assert!(default_dest(&fx, "elsewhere").exists(), "{}", out(&o));
    assert!(!default_dest(&fx, "feature/b").exists(), "{}", out(&o));

    for (args, want) in [
        (vec!["new", "feature/c", "--dir", "../escape"], "not a path"),
        (vec!["new", "feature/c", "--dir", ".."], "not a path"),
        (vec!["new", "feature/c", "--dir", ""], "not a path"),
    ] {
        let o = run_wt(&fx.repo, &args);
        assert_eq!(o.status.code(), Some(2), "{args:?}: {}", err(&o));
        assert!(err(&o).contains(want), "{args:?}: {}", err(&o));
    }
    assert!(!default_dest(&fx, "feature/c").exists(), "c was created");
}

#[test]
fn new_placement_settings_override() {
    // absolute worktree-dir
    let fx = Fixture::new();
    write_rules(&fx, GROUP_CFG);
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
    write_rules(&fx, GROUP_CFG);
    fs::write(fx.repo.join(".git/wtree/settings"), "worktree-dir = wts\n").unwrap();
    assert_ok(&run_wt(&fx.repo, &["new", "feature/b"]));
    assert!(fx.repo.join("wts/feature-b").is_dir());

    // a settings typo aborts instead of silently using the default
    let fx = Fixture::new();
    write_rules(&fx, GROUP_CFG);
    fs::write(fx.repo.join(".git/wtree/settings"), "worktreedir = x\n").unwrap();
    let o = run_wt(&fx.repo, &["new", "feature/c"]);
    assert_fail(&o);
    assert!(err(&o).contains("unknown key 'worktreedir'"), "{}", err(&o));
}

fn install_hook(fx: &Fixture, name: &str, body: &str) {
    let hooks = fx.repo.join(".git/wtree/hooks");
    fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join(name);
    fs::write(&hook, body).unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A hook that appends one line per run to `<repo>/hook.log`, so a test can
/// read back both whether it ran and what it was told. `fields` is a shell
/// format string over the WTREE_* variables.
fn logging_hook(fields: &str) -> String {
    format!("#!/bin/sh\nprintf '%s\\n' \"{fields}\" >> \"$WTREE_REPO/hook.log\"\n")
}

fn hook_log(fx: &Fixture) -> Vec<String> {
    fs::read_to_string(fx.repo.join("hook.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn new_runs_post_create_hook_with_wt_env() {
    let fx = Fixture::new();
    write_rules(&fx, GROUP_CFG);
    install_hook(
        &fx,
        "post-create",
        "#!/bin/sh\nprintf '%s|%s|%s|%s|%s' \"$WTREE_BRANCH\" \"$WTREE_PARENT\" \"$WTREE_REPO\" \"$WTREE_INTERACTIVE\" \"$(pwd)\" > \"$WTREE_PATH/hook-ran\"\n",
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
        "WTREE_REPO must be the primary worktree root"
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
    write_rules(&fx, GROUP_CFG);
    install_hook(&fx, "post-create", "#!/bin/sh\nexit 3\n");
    let o = run_wt(&fx.repo, &["new", "feature/a"]);
    assert_ok(&o); // hook failure is not a verb failure
    assert!(
        err(&o).contains("post-create hook failed (exit 3)"),
        "{}",
        err(&o)
    );
    let dest = default_dest(&fx, "feature/a");
    assert!(dest.is_dir());
    assert!(matches!(
        state::read(&repo::private_git_dir(&dest).unwrap()),
        StateRead::Valid(_)
    ));
}

/// `copy` and `post-create` split one job between them — the parent's files
/// cross, then the hook makes what has to be generated here — so the hook has
/// to find the copied files already in place. Pinned in both verbs that create
/// a worktree, because the order is invisible until a hook reads one.
#[test]
fn the_copy_list_lands_before_post_create_runs() {
    let fx = Fixture::new();
    write_rules(
        &fx,
        "[main]\nchildren = dev\ncopy = .env\n\n[dev]\nchildren = group:feat\ncopy = .env\n\n[group:feat]\nname-allow = feature/*\ncopy = .env\n",
    );
    fs::write(fx.repo.join(".env"), "SECRET=1\n").unwrap();
    // Reads what `copy` was supposed to bring; empty when it has not run yet.
    install_hook(
        &fx,
        "post-create",
        "#!/bin/sh\nprintf '%s\\n' \"$WTREE_VERB:$(cat .env 2>/dev/null || echo MISSING)\" >> \"$WTREE_REPO/hook.log\"\n",
    );

    fx.git(&fx.repo, &["branch", "dev", "main"]);
    assert_ok(&run_wt(&fx.repo, &["open", "dev"]));
    assert_ok(&run_wt(&default_dest(&fx, "dev"), &["new", "feature/a"]));

    assert_eq!(
        hook_log(&fx),
        vec!["open:SECRET=1", "new:SECRET=1"],
        "the hook must see the copied .env, not MISSING"
    );
}

/// `open` makes a worktree the same way `new` does, so it answers to the same
/// pair. `WTREE_VERB` is the only thing that separates them, and a branch the
/// policy has not declared has no parent to name.
#[test]
fn open_runs_the_create_pair_and_says_which_verb_it_is() {
    let fx = Fixture::new();
    write_rules(&fx, MIDDLE_CFG);
    let fields = "$WTREE_HOOK|$WTREE_VERB|$WTREE_BRANCH|$WTREE_PARENT";
    install_hook(&fx, "pre-create", &logging_hook(fields));
    install_hook(&fx, "post-create", &logging_hook(fields));

    // A declared branch: the policy names its parent.
    fx.git(&fx.repo, &["branch", "dev", "main"]);
    assert_ok(&run_wt(&fx.repo, &["open", "dev"]));
    // An undeclared one: managed by nobody, so there is no parent to hand over.
    fx.git(&fx.repo, &["branch", "stray", "main"]);
    assert_ok(&run_wt(&fx.repo, &["open", "stray"]));

    assert_eq!(
        hook_log(&fx),
        vec![
            "pre-create|open|dev|main",
            "post-create|open|dev|main",
            "pre-create|open|stray|",
            "post-create|open|stray|",
        ]
    );
    // The same pair under `new` says `new`, which is the whole point of the
    // field. `feature/*` belongs under dev, so it is cut from the worktree the
    // first `open` just made.
    assert_ok(&run_wt(&default_dest(&fx, "dev"), &["new", "feature/a"]));
    assert_eq!(
        hook_log(&fx)[4..],
        [
            "pre-create|new|feature/a|dev",
            "post-create|new|feature/a|dev"
        ]
    );
}

/// A gate is a gate wherever it runs: `open` keeps nothing when pre-create
/// refuses, and the branch it would have attached is left alone.
#[test]
fn pre_create_refusal_opens_nothing() {
    let fx = Fixture::new();
    write_rules(&fx, MIDDLE_CFG);
    fx.git(&fx.repo, &["branch", "dev", "main"]);
    install_hook(&fx, "pre-create", "#!/bin/sh\nexit 1\n");

    let o = run_wt(&fx.repo, &["open", "dev"]);
    assert_fail(&o);
    assert!(
        err(&o).contains("pre-create hook failed") && err(&o).contains("nothing was opened"),
        "{}",
        err(&o)
    );
    assert!(!default_dest(&fx, "dev").exists());
    // open never touches the branch, and a refusal must not either
    assert!(branches(&fx).contains(&"dev".to_string()));

    // and --no-hooks gets past it
    assert_ok(&run_wt(&fx.repo, &["open", "dev", "--no-hooks"]));
    assert!(default_dest(&fx, "dev").is_dir());
}

/// The other half of the contract: a `pre-` hook is a gate, so its refusal has
/// to leave the repo exactly as it was — no worktree, no branch, not even the
/// placement folder that `new` would otherwise create.
#[test]
fn pre_create_refusal_creates_nothing() {
    let fx = Fixture::new();
    write_rules(&fx, GROUP_CFG);
    install_hook(&fx, "pre-create", "#!/bin/sh\nexit 1\n");
    let o = run_wt(&fx.repo, &["new", "feature/a"]);
    assert_fail(&o);
    assert!(
        err(&o).contains("pre-create hook failed (exit 1)") && err(&o).contains("nothing was"),
        "{}",
        err(&o)
    );
    assert!(!default_dest(&fx, "feature/a").exists());
    assert!(!fx.tmp.0.join("repo.worktrees").exists());
    assert_eq!(branches(&fx), vec!["main".to_string()]);
}

/// A hook that dies of a signal has no exit code; "exit -1" would send the
/// reader looking for a status no shell ever returned.
#[test]
fn a_hook_killed_by_a_signal_is_reported_as_one() {
    let fx = Fixture::new();
    write_rules(&fx, GROUP_CFG);
    install_hook(&fx, "pre-create", "#!/bin/sh\nkill -9 $$\n");
    let o = run_wt(&fx.repo, &["new", "feature/a"]);
    assert_fail(&o);
    assert!(
        err(&o).contains("pre-create hook failed (signal 9)"),
        "{}",
        err(&o)
    );
    assert!(!default_dest(&fx, "feature/a").exists());
}

/// Everything after `--` belongs to the create hooks: both halves see the same
/// "$@", word boundaries intact, nothing shell-expanded on the way — a prompt
/// with spaces and `$`s arrives as one argument, and `--help` past the
/// separator is an argument too, not a question to wtree.
#[test]
fn arguments_after_the_separator_reach_both_create_hooks() {
    let fx = Fixture::new();
    write_rules(&fx, GROUP_CFG);
    let capture = |f: &str| format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$WTREE_REPO/{f}\"\n");
    install_hook(&fx, "pre-create", &capture("pre.txt"));
    install_hook(&fx, "post-create", &capture("post.txt"));

    let o = run_wt(
        &fx.repo,
        &[
            "new",
            "feature/a",
            "--",
            "claude",
            "fix GH #322; echo $HOME",
            "--help",
        ],
    );
    assert_ok(&o);
    assert!(default_dest(&fx, "feature/a").is_dir());
    let want = "claude\nfix GH #322; echo $HOME\n--help\n";
    for f in ["pre.txt", "post.txt"] {
        assert_eq!(fs::read_to_string(fx.repo.join(f)).unwrap(), want, "{f}");
    }
}

/// `open` hands the same pair the same arguments.
#[test]
fn open_hands_the_separator_arguments_to_the_pair_too() {
    let fx = Fixture::new();
    write_rules(&fx, MIDDLE_CFG);
    fx.git(&fx.repo, &["branch", "dev", "main"]);
    install_hook(
        &fx,
        "post-create",
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$WTREE_REPO/post.txt\"\n",
    );
    assert_ok(&run_wt(&fx.repo, &["open", "dev", "--", "codex", "resume"]));
    assert_eq!(
        fs::read_to_string(fx.repo.join("post.txt")).unwrap(),
        "codex\nresume\n"
    );
}

/// Arguments with no hook file to reach: warn and proceed. The worktree is the
/// primary ask, and the same command is sound where a hook is installed.
#[test]
fn separator_arguments_without_a_hook_warn_and_still_create() {
    let fx = Fixture::new();
    write_rules(&fx, GROUP_CFG);
    let o = run_wt(&fx.repo, &["new", "feature/a", "--", "claude"]);
    assert_ok(&o);
    assert!(default_dest(&fx, "feature/a").is_dir());
    assert!(err(&o).contains("nowhere to go"), "{}", err(&o));
}

/// A parked hook speaks for itself: its own skipped-warning already says why
/// the arguments went unused, so the nowhere-to-go line would say it twice.
#[test]
fn a_parked_hook_speaks_for_itself_when_arguments_arrive() {
    let fx = Fixture::new();
    write_rules(&fx, GROUP_CFG);
    let hooks = fx.repo.join(".git/wtree/hooks");
    fs::create_dir_all(&hooks).unwrap();
    fs::write(hooks.join("post-create"), "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(hooks.join("post-create"), fs::Permissions::from_mode(0o644)).unwrap();

    let o = run_wt(&fx.repo, &["new", "feature/a", "--", "claude"]);
    assert_ok(&o);
    assert!(
        err(&o).contains("is not executable") && err(&o).contains("skipped"),
        "{}",
        err(&o)
    );
    assert!(!err(&o).contains("nowhere to go"), "{}", err(&o));
}

/// "Skip the hooks" and "hand the hooks these arguments" cannot both be meant:
/// the contradiction is refused at parse time, before anything exists.
#[test]
fn skipping_hooks_while_handing_them_arguments_is_refused() {
    let fx = Fixture::new();
    write_rules(&fx, GROUP_CFG);
    let o = run_wt(
        &fx.repo,
        &["new", "feature/a", "--no-hooks", "--", "claude"],
    );
    assert_fail(&o);
    assert!(err(&o).contains("--no-hooks"), "{}", err(&o));
    assert!(!default_dest(&fx, "feature/a").exists());
}

/// The state file's name is an on-disk contract: an installed wtree reads
/// records earlier runs wrote, so the literal is pinned here, not just the
/// constant. WTREE_ is the prefix everything else uses; WT_ was cc-toolkit
/// inheritance, retired before the first release.
#[test]
fn the_state_record_lives_in_wtree_head() {
    let fx = Fixture::new();
    write_rules(&fx, GROUP_CFG);
    assert_ok(&run_wt(&fx.repo, &["new", "feature/a"]));
    assert!(
        fx.repo
            .join(".git/worktrees/feature-a/WTREE_HEAD")
            .is_file()
    );
}

/// pre-create runs where the branch is being forked from, and names a WTREE_PATH
/// that does not exist yet — the one hook whose worktree is still in the future.
#[test]
fn pre_create_runs_in_the_parent_with_the_future_path() {
    let fx = Fixture::new();
    write_rules(&fx, GROUP_CFG);
    install_hook(
        &fx,
        "pre-create",
        &logging_hook(
            "$WTREE_HOOK|$WTREE_BRANCH|$WTREE_PARENT|$WTREE_PATH|$(pwd)|$([ -e \"$WTREE_PATH\" ] && echo exists || echo absent)",
        ),
    );
    assert_ok(&run_wt(&fx.repo, &["new", "feature/a"]));
    let log = hook_log(&fx);
    assert_eq!(log.len(), 1, "{log:?}");
    let f: Vec<&str> = log[0].split('|').collect();
    assert_eq!(&f[..3], &["pre-create", "feature/a", "main"]);
    assert_eq!(
        Path::new(f[3]),
        default_dest(&fx, "feature/a"),
        "WTREE_PATH is where the worktree will be"
    );
    assert_eq!(
        Path::new(f[4]).canonicalize().unwrap(),
        fx.repo.canonicalize().unwrap(),
        "cwd is the worktree being forked from"
    );
    assert_eq!(f[5], "absent", "the new worktree does not exist yet");
}

// ------------------------------------------------------------------- copy ----

/// A fresh worktree has only what the branch tracks, so `.env` and friends have
/// to be carried over or nothing runs there.
#[test]
fn new_carries_the_files_the_policy_lists_from_the_parent_worktree() {
    let fx = Fixture::new();
    write_rules(
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
    assert!(
        out(&o).contains("copied .env, .env.local, .vscode from 'main'"),
        "{}",
        out(&o)
    );

    let dest = default_dest(&fx, "feature/a");
    assert_eq!(fs::read_to_string(dest.join(".env")).unwrap(), "SECRET=1\n");
    assert_eq!(
        fs::read_to_string(dest.join(".env.local")).unwrap(),
        "LOCAL=2\n"
    );
    assert_eq!(
        fs::read_to_string(dest.join(".vscode/settings.json")).unwrap(),
        "{}\n"
    );
    assert!(
        !dest.join("untouched").exists(),
        "only listed patterns cross"
    );
}

/// The trailing slash is what makes a directory deliberate. Without it the rule
/// looks like it applies, so the near miss is named rather than passed over.
#[test]
fn a_directory_crosses_only_when_the_pattern_ends_in_a_slash() {
    let fx = Fixture::new();
    write_rules(
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
    write_rules(
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
    assert!(
        ft.is_symlink(),
        "the link was dereferenced into a copied tree"
    );
    assert_eq!(fs::read_link(&link).unwrap(), Path::new("real_modules"));
}

/// The mirror near miss: a trailing slash no longer reaches a symlink, so the
/// pattern that used to copy one has to say what changed instead of going quiet.
#[test]
fn a_slashed_pattern_names_the_symlink_it_no_longer_takes() {
    let fx = Fixture::new();
    write_rules(
        &fx,
        "[main]\nchildren = group:feat\n\n[group:feat]\nname-allow = feature/*\ncopy = node_modules/\n",
    );
    fs::create_dir(fx.repo.join("real_modules")).unwrap();
    std::os::unix::fs::symlink("real_modules", fx.repo.join("node_modules")).unwrap();

    let o = run_wt(&fx.repo, &["new", "feature/a"]);
    assert_ok(&o);
    assert!(
        out(&o).contains("skipped 'node_modules': a symlink crosses as a link"),
        "{}",
        out(&o)
    );
    assert!(!default_dest(&fx, "feature/a").join("node_modules").exists());
}

/// Copying over a tracked file would leave the worktree dirty before the user
/// has touched anything, so what the branch already carries wins.
#[test]
fn copy_never_overwrites_what_the_branch_already_tracks() {
    let fx = Fixture::new();
    write_rules(
        &fx,
        "[main]\nchildren = group:feat\n\n[group:feat]\nname-allow = feature/*\ncopy = tracked.txt\n",
    );
    fs::write(fx.repo.join("tracked.txt"), "committed\n").unwrap();
    fx.git(&fx.repo, &["add", "-A"]);
    fx.git(&fx.repo, &["commit", "-q", "-m", "track it"]);
    fs::write(fx.repo.join("tracked.txt"), "local edit\n").unwrap();

    let o = run_wt(&fx.repo, &["new", "feature/a"]);
    assert_ok(&o);
    assert!(
        out(&o).contains("skipped 'tracked.txt': already in the worktree"),
        "{}",
        out(&o)
    );
    let dest = default_dest(&fx, "feature/a");
    assert_eq!(
        fs::read_to_string(dest.join("tracked.txt")).unwrap(),
        "committed\n"
    );
    assert_eq!(fx.git(&dest, &["status", "--porcelain"]).trim(), "");
}

/// `open` reads the parent from the rules, so the source can be a worktree
/// that is not currently checked out. The worktree is still created — it is
/// usable without the files, and undoing it would be the larger surprise.
#[test]
fn open_says_so_when_the_parent_has_no_worktree_to_copy_from() {
    let fx = Fixture::new();
    write_rules(
        &fx,
        "[main]\nchildren = dev\n\n[dev]\nchildren = staging\n\n[staging]\ncopy = .env\n",
    );
    fs::write(fx.repo.join(".env"), "SECRET=1\n").unwrap();
    fx.git(&fx.repo, &["branch", "staging", "main"]);

    let o = run_wt(&fx.repo, &["open", "staging"]);
    assert_ok(&o);
    assert!(
        out(&o).contains("copied nothing: parent 'dev' has no worktree"),
        "{}",
        out(&o)
    );
    assert!(
        default_dest(&fx, "staging").exists(),
        "the worktree is created regardless"
    );
}

/// A pattern with a separator can never match — entries are matched by name at
/// the worktree root — so it is a policy that silently does nothing.
#[test]
fn a_copy_pattern_with_a_path_separator_is_a_load_error() {
    let fx = Fixture::new();
    write_rules(
        &fx,
        "[main]\nchildren = group:feat\n\n[group:feat]\ncopy = config/*.json\n",
    );
    let o = run_wt(&fx.repo, &["list"]);
    assert_fail(&o);
    assert!(
        err(&o).contains("invalid copy pattern 'config/*.json' in [group:feat]"),
        "{}",
        err(&o)
    );
    assert!(
        err(&o).contains(":5"),
        "the offending line is cited:\n{}",
        err(&o)
    );
}

// --------------------------------------------------------------- list/info ----

#[test]
fn list_shows_identities_unknowns_and_bare_branches() {
    let fx = Fixture::new();
    write_rules(
        &fx,
        "[main]\nchildren = dev, group:feat\n\n[dev]\n\n[group:feat]\nname-allow = feature/*\n",
    );
    assert_ok(&run_wt(&fx.repo, &["new", "feature/a"]));
    fx.git(&fx.repo, &["branch", "dev", "main"]); // declared fixed, no worktree
    fx.git(&fx.repo, &["branch", "loose", "main"]); // neither declared nor recorded
    fx.add_worktree("junk", "main"); // raw worktree, unmanaged
    let o = run_wt(&fx.repo, &["list"]);
    assert_ok(&o);
    let stdout = out(&o);
    // Parentage is position in the tree now, not a `parent: X` column. Piped
    // output uses the ASCII glyphs.
    assert!(
        stdout.contains("* main         fixed       repo"),
        "{stdout}"
    );
    assert!(stdout.contains("`-+ feature/a  group:feat"), "{stdout}");
    assert!(
        stdout.contains("|-. dev        fixed"),
        "declared, no worktree:\n{stdout}"
    );
    // An unmanaged worktree has no identity, so no parent, so no place in the
    // tree. Plain `list` only says how many there are; the entries and their
    // entries wait behind `--unmanaged` rather than crowding the tree.
    // `junk` is a checkout on disk and `loose` is only a branch, so the summary
    // counts them apart rather than calling both the same thing.
    assert!(
        stdout.contains("found 2 unmanaged: 1 worktree, 1 branch. See 'wtree list --unmanaged'."),
        "{stdout}"
    );
    assert!(
        !stdout.contains("wtree-junk"),
        "no entries by default:\n{stdout}"
    );

    // `--unmanaged` is its own screen: the strays under a heading per kind, no
    // tree above them, and the step to take on the heading.
    let oe = run_wt(&fx.repo, &["list", "--unmanaged"]);
    assert_ok(&oe);
    let entries = out(&oe);
    // The recovery rides on the heading rather than on every row beneath it.
    assert!(
        entries.contains("[unmanaged worktree]  (recover with: wtree adopt)"),
        "{entries}"
    );
    assert!(
        entries.contains(
            "[unmanaged branch]  (recover with: 'wtree open <branch>', then 'wtree adopt' there)"
        ),
        "{entries}"
    );
    // No tree above them: this view answers one question.
    assert!(!entries.contains("@ main"), "no tree here:\n{entries}");
    // The worktree row leads with the path in full — an unmanaged checkout
    // sits wherever it was made. The branch row is just the branch.
    assert!(
        entries
            .lines()
            .any(|l| l.starts_with('/') && l.ends_with("  junk")),
        "absolute path then branch:\n{entries}"
    );
    assert!(entries.contains("\nloose\n"), "{entries}");
    // The headings carry the count, so the summary sentence stands down.
    assert!(!entries.contains("found 2 unmanaged"), "{entries}");
    // No reason beneath a row: the heading says the step, and `wtree info`
    // standing in the worktree says why that particular one could not be
    // judged. A branch stray has only ever one reason, so nothing is lost.
    assert!(
        !entries.lines().any(|l| l.trim_start().starts_with('!')),
        "rows carry no reason lines:\n{entries}"
    );

    // The level that printed them is gone with them.
    let od = run_wt(&fx.repo, &["list", "--detail"]);
    assert_eq!(od.status.code(), Some(2), "{}", err(&od));
    assert!(
        err(&od).contains("unknown argument '--detail'"),
        "{}",
        err(&od)
    );
}

/// A branch no worktree has checked out has nothing for the directory column.
/// Run together, its divergence would slide left under the directories above it
/// and read as one, so the columns are sized across every row instead.
#[test]
fn list_keeps_the_divergence_column_when_a_row_has_no_directory() {
    let fx = Fixture::new();
    write_rules(
        &fx,
        "[main]\nchildren = stage, group:feat\n\n[stage]\n\n[group:feat]\nname-allow = feature/*\n",
    );
    // no `v` in either name: the plain glyph for "behind" is `v` too.
    fx.git(&fx.repo, &["branch", "stage", "main"]); // declared, never opened
    member(&fx, "feature/a", "feat", "main"); // opened, so it has a directory
    fx.commit(&fx.repo, "main moves"); // both fall a commit behind

    let stdout = out(&run_wt(&fx.repo, &["list"]));
    let col = |branch: &str| -> usize {
        let l = stdout
            .lines()
            .find(|l| l.contains(branch))
            .unwrap_or_else(|| panic!("no row for {branch}:\n{stdout}"));
        l.find('v')
            .unwrap_or_else(|| panic!("no divergence on {l:?}:\n{stdout}"))
    };
    assert_eq!(
        col("stage"),
        col("feature/a"),
        "the counts share a column whether or not the row has a directory:\n{stdout}"
    );
}

/// Depth comes from recorded parents, so a grandchild has to indent under its
/// own parent and the run of `|` has to survive the level between them.
#[test]
fn list_indents_a_grandchild_under_its_own_parent() {
    let fx = Fixture::new();
    write_rules(
        &fx,
        "[main]\nchildren = dev, group:feat\n\n[dev]\nchildren = group:feat\n\n[group:feat]\nname-allow = feature/*\n",
    );
    fx.add_worktree("dev", "main");
    member(&fx, "feature/a", "feat", "dev");
    member(&fx, "feature/b", "feat", "main");

    let stdout = out(&run_wt(&fx.repo, &["list"]));
    let l: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        l.len(),
        4,
        "one row per branch, no section headers:\n{stdout}"
    );
    assert!(l[0].starts_with("* main"), "{stdout}");
    assert!(l[1].starts_with("|-+ dev"), "{stdout}");
    // the leading `|` is main's line continuing past dev down to feature/b
    assert!(l[2].starts_with("| `-+ feature/a"), "{stdout}");
    assert!(l[3].starts_with("`-+ feature/b"), "{stdout}");
}

/// A recorded parent can name a branch that no longer exists. Dropping the
/// child would hide a worktree that is really there, so it is shown at the root
/// instead — the tree never loses a branch it was handed.
#[test]
fn list_keeps_a_branch_whose_recorded_parent_is_gone() {
    let fx = Fixture::new();
    write_rules(&fx, GROUP_CFG);
    let wt = fx.add_worktree("feature/a", "main");
    fx.write_state(&wt, "feature/a", "group:feat", "ghost");

    let stdout = out(&run_wt(&fx.repo, &["list"]));
    assert!(
        stdout.lines().any(|l| l.starts_with("+ feature/a")),
        "shown at the root, not swallowed:\n{stdout}"
    );
    assert!(stdout.contains("* main"), "{stdout}");
}

/// The counts are what say `sync` or `merge` is due; nothing else on screen
/// does. Both directions are read off one walk, a root branch has no parent to
/// diverge from, and a worktree level with its parent stays quiet. Piped
/// output spells the arrows `^`/`v`.
#[test]
fn list_counts_divergence_from_the_parent_in_both_directions() {
    let fx = Fixture::new();
    write_rules(
        &fx,
        "[main]\nchildren = group:feat\n\n[group:feat]\nname-allow = feature/*\n",
    );
    assert_ok(&run_wt(&fx.repo, &["new", "feature/a"]));
    let wt = default_dest(&fx, "feature/a");

    let level = out(&run_wt(&fx.repo, &["list"]));
    assert!(
        level.contains("`-+ feature/a  group:feat  feature-a\n"),
        "just created, level with main, so no counts at all:\n{level}"
    );

    commit_other(&fx, &fx.repo, "one.txt", "one");
    commit_other(&fx, &fx.repo, "two.txt", "two");
    let behind = out(&run_wt(&fx.repo, &["list"]));
    assert!(
        behind.contains("`-+ feature/a  group:feat  feature-a  v2"),
        "{behind}"
    );

    fx.commit(&wt, "work of its own");
    let both = out(&run_wt(&fx.repo, &["list"]));
    assert!(
        both.contains("`-+ feature/a  group:feat  feature-a  ^1 v2"),
        "ahead is listed before behind:\n{both}"
    );
    assert!(
        both.lines()
            .any(|l| l.contains("* main") && !l.contains("^") && !l.contains("v2")),
        "main is the root — it has no parent to diverge from:\n{both}"
    );
}

/// Cutting rows to the terminal is for terminals. A reader that is not one is
/// grepping, diffing or feeding another tool, and half a branch name serves
/// none of those — so piped output stays whole however long the names get.
#[test]
fn list_never_cuts_piped_output() {
    let long = "feature/an-extremely-long-branch-name-that-keeps-going-and-going";
    let fx = Fixture::new();
    write_rules(&fx, GROUP_CFG);
    assert_ok(&run_wt(&fx.repo, &["new", long]));

    let stdout = out(&run_wt(&fx.repo, &["list"]));
    assert!(stdout.contains(long), "the branch name survives:\n{stdout}");
    assert!(!stdout.contains('…'), "nothing was cut:\n{stdout}");
    // The directory name is derived from the branch, so it is long too and sits
    // past the branch column — the far end of the row is intact as well.
    assert!(
        stdout.contains("feature-an-extremely-long-branch-name-that-keeps-going-and-going"),
        "{stdout}"
    );
}

/// `list` never asks whether work would be lost, so it must not pay for the
/// answer. The probe behind `[unreflected]` runs a merge-tree simulation per
/// worktree; `destroy` is where that question is asked and answered.
#[test]
fn list_does_not_run_the_work_loss_probe() {
    let fx = Fixture::new();
    write_rules(&fx, GROUP_CFG);
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "unmerged work");

    let stdout = out(&run_wt(&fx.repo, &["list"]));
    assert!(
        !stdout.contains("unreflected"),
        "the fact is not gathered, so it cannot be shown:\n{stdout}"
    );
    // The same branch, asked the same question by the verb that needs it.
    let o = run_wt(&wt, &["destroy"]);
    assert_fail(&o);
    assert!(
        err(&o).contains("commits not reflected in parent"),
        "destroy still gathers it:\n{}",
        err(&o)
    );
}

#[test]
fn info_managed_shows_rules_and_previews() {
    let fx = Fixture::new();
    write_rules(
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
    assert!(
        stdout.contains("merge to 'main': squash (flag optional"),
        "{stdout}"
    );
    assert!(
        stdout.contains("merge: 'feature/a' -> 'main' (--squash)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("sync: merge 'main' into 'feature/a'"),
        "{stdout}"
    );
    assert!(
        stdout.contains("destroy: would remove 'feature/a'"),
        "{stdout}"
    );
    assert!(
        stdout.contains("children: none declared — nothing may be created here"),
        "{stdout}"
    );
}

#[test]
fn info_unknown_shows_reasons_and_adopt_hint() {
    let fx = Fixture::new();
    write_rules(&fx, GROUP_CFG);
    let junk = fx.add_worktree("junk", "main");
    let o = run_wt(&junk, &["info"]);
    assert_ok(&o);
    let stdout = out(&o);
    assert!(stdout.contains("identity: unknown"), "{stdout}");
    assert!(stdout.contains("not a declared [branch]"), "{stdout}");
    assert!(stdout.contains("wtree adopt"), "{stdout}");
    assert!(
        stdout.contains("allowed verbs here: open, close, list, info, rule, init, save, adopt, llm"),
        "{stdout}"
    );
}

// -------------------------------------------------------------------- rule ----

/// One line per line of the screen, runs of spaces collapsed, so the assertions
/// read a key and its value without owning the column widths.
fn flat_lines(o: &Output) -> Vec<String> {
    out(o)
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect()
}

#[test]
fn rule_prints_every_key_and_marks_the_filled_in_ones() {
    let fx = Fixture::new();
    write_rules(
        &fx,
        "[main]\nchildren = group:feat\nmerge-mode = squash\n\n\
         [group:feat]\nname-allow = feature/*\nephemeral = true\n",
    );
    let o = run_wt(&fx.repo, &["rule"]);
    assert_ok(&o);
    let lines = flat_lines(&o);
    let stdout = out(&o);
    let has = |s: &str| lines.iter().any(|l| l == s);
    assert!(stdout.starts_with(".git/wtree/rules\n"), "{stdout}");
    for want in [
        "[main]",
        "children = group:feat",
        "merge-mode = squash",
        // absent from the file, and judged by all the same
        "destroyable = true (default)",
        "copy = (none) (default)",
        "description = (none) (default)",
        "[group:feat]",
        "name-allow = feature/*",
        "ephemeral = true",
        // no merge-mode means every mode, not none
        "merge-mode = squash, rebase, no-ff, ff (default)",
        // an empty allow-list takes any name; an empty deny-list denies nothing
        "name-deny = (none) (default)",
        "children = (none) (default)",
    ] {
        assert!(has(want), "missing '{want}':\n{stdout}");
    }
    // `merge-mode = none` is a declared refusal of every mode, not an absence
    write_rules(&fx, "[main]\nchildren = *\nmerge-mode = none\n");
    assert!(
        flat_lines(&run_wt(&fx.repo, &["rule"]))
            .iter()
            .any(|l| l == "merge-mode = none"),
        "{}",
        out(&run_wt(&fx.repo, &["rule"]))
    );
}

/// The policy does not depend on where it is read from: an unmanaged worktree
/// gets the same screen, since nothing here is judged against an identity.
#[test]
fn rule_reads_the_same_from_an_unmanaged_worktree_and_takes_no_arguments() {
    let fx = Fixture::new();
    write_rules(&fx, GROUP_CFG);
    let junk = fx.add_worktree("junk", "main");
    let o = run_wt(&junk, &["rule"]);
    assert_ok(&o);
    assert_eq!(flat_lines(&o), flat_lines(&run_wt(&fx.repo, &["rule"])));

    let o = run_wt(&fx.repo, &["rule", "--all"]);
    assert_fail(&o);
    assert!(err(&o).contains("takes no arguments"), "{}", err(&o));
}

/// A rules file with nothing in it loads, so the screen has to say that it is
/// the policy that is empty and not the command that gave up.
#[test]
fn rule_says_so_when_nothing_is_declared() {
    let fx = Fixture::new();
    write_rules(&fx, "# emptied by hand\n");
    let o = run_wt(&fx.repo, &["rule"]);
    assert_ok(&o);
    assert!(
        out(&o).contains("no rule — nothing is declared"),
        "{}",
        out(&o)
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
    write_rules(&fx, &merge_cfg("squash"));
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "one");
    fx.commit(&wt, "two");
    let before = rev(&fx, "main");
    // single allowed mode: the flag may be omitted
    let o = run_wt(&wt, &["merge", "-m", "feat: a"]);
    assert_ok(&o);
    assert!(
        out(&o).contains("merged 'feature/a' onto 'main'"),
        "{}",
        out(&o)
    );
    let count = fx.git(
        &fx.repo,
        &["rev-list", "--count", &format!("{before}..main")],
    );
    assert_eq!(count.trim(), "1", "squash lands exactly one commit");
    assert_eq!(
        fx.git(&fx.repo, &["log", "-1", "--format=%s", "main"])
            .trim(),
        "feat: a"
    );
    // convergence: the branch sits exactly on the target
    assert_eq!(rev(&fx, "main"), rev(&fx, "feature/a"));
    // the target's checked-out worktree received the files
    assert!(
        fs::read_to_string(fx.repo.join("f.txt"))
            .unwrap()
            .contains("two")
    );
}

#[test]
fn merge_rebase_replays_each_commit_onto_moved_target() {
    let fx = Fixture::new();
    write_rules(&fx, &merge_cfg("rebase"));
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "one");
    fx.commit(&wt, "two");
    let before = rev(&fx, "main");
    commit_other(&fx, &fx.repo, "other.txt", "main moved"); // non-conflicting
    let o = run_wt(&wt, &["merge"]);
    assert_ok(&o);
    assert!(out(&o).contains("2 commits"), "{}", out(&o));
    let count = fx.git(
        &fx.repo,
        &["rev-list", "--count", &format!("{before}..main")],
    );
    assert_eq!(count.trim(), "3", "2 replayed + the target's own commit");
    let subjects = fx.git(&fx.repo, &["log", "-3", "--format=%s", "main"]);
    assert_eq!(subjects.trim(), "two\none\nmain moved");
    assert_eq!(rev(&fx, "main"), rev(&fx, "feature/a"));
}

#[test]
fn merge_no_ff_creates_merge_commit_without_target_checkout() {
    let fx = Fixture::new();
    write_rules(&fx, &merge_cfg("no-ff"));
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
        fx.git(&fx.repo, &["log", "-1", "--format=%s", "main"])
            .trim(),
        "merge feature/a"
    );
    assert_eq!(rev(&fx, "main"), rev(&fx, "feature/a")); // convergence
    // the primary worktree (main checked out) moved with the ff
    assert!(
        fs::read_to_string(fx.repo.join("f.txt"))
            .unwrap()
            .contains("one")
    );
}

#[test]
fn merge_ff_moves_target_and_refuses_without_fallback() {
    let fx = Fixture::new();
    write_rules(&fx, &merge_cfg("ff"));
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "one");
    let feat_tip = rev(&fx, "feature/a");
    let o = run_wt(&wt, &["merge"]);
    assert_ok(&o);
    assert!(
        out(&o).contains("fast-forwarded 'main' to 'feature/a'"),
        "{}",
        out(&o)
    );
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
    write_rules(&fx, &merge_cfg("squash"));
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
    write_rules(&fx, &merge_cfg("squash"));
    let wt = member(&fx, "feature/a", "feat", "main"); // no commits of its own
    let o = run_wt(&wt, &["merge", "-m", "empty"]);
    assert_fail(&o);
    assert!(err(&o).contains("nothing to merge"), "{}", err(&o));
}

#[test]
fn merge_stashes_and_restores_uncommitted_work() {
    let fx = Fixture::new();
    write_rules(&fx, &merge_cfg("squash"));
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
    assert!(
        !fs::read_to_string(fx.repo.join("f.txt"))
            .unwrap()
            .contains("WIP")
    );
    // and the uncommitted work came back, not left in the stash
    assert!(fs::read_to_string(&f).unwrap().contains("WIP"));
    assert_eq!(
        fs::read_to_string(wt.join("scratch.txt")).unwrap(),
        "notes\n"
    );
    assert_eq!(fx.git(&wt, &["stash", "list"]).trim(), "");
}

#[test]
fn merge_moves_uncheckedout_target_by_ref_update() {
    let fx = Fixture::new();
    write_rules(
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
    assert!(
        !fs::read_to_string(fx.repo.join("f.txt"))
            .unwrap()
            .contains("one")
    );
}

#[test]
fn merge_rolls_back_branch_and_stash_when_target_ff_fails() {
    let fx = Fixture::new();
    write_rules(&fx, &merge_cfg("squash"));
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
    assert_eq!(
        fs::read_to_string(wt.join("scratch.txt")).unwrap(),
        "notes\n"
    );
    assert_eq!(fx.git(&wt, &["stash", "list"]).trim(), "");
}

#[test]
fn merge_flag_and_message_rules() {
    let fx = Fixture::new();
    // two allowed modes: the flag is mandatory
    write_rules(&fx, &merge_cfg("squash, rebase"));
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "one");
    let o = run_wt(&wt, &["merge", "-m", "x"]);
    assert_fail(&o);
    assert!(err(&o).contains("multiple merge modes"), "{}", err(&o));
    // a mode outside the allowed set is refused by the judge
    let o = run_wt(&wt, &["merge", "--ff"]);
    assert_fail(&o);
    assert!(
        err(&o).contains("accepts squash, rebase merges only"),
        "{}",
        err(&o)
    );
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
    write_rules(&fx, GROUP_CFG);
    let wt = member(&fx, "feature/a", "feat", "main");
    // own work + parent work in different files: a true merge, no conflict
    commit_other(&fx, &wt, "mine.txt", "mine");
    fx.commit(&fx.repo, "parent work");
    // uncommitted work survives the sync
    fs::write(wt.join("scratch.txt"), "notes\n").unwrap();
    let o = run_wt(&wt, &["sync"]);
    assert_ok(&o);
    assert!(
        out(&o).contains("synced 'feature/a' with 'main'"),
        "{}",
        out(&o)
    );
    // parent contained; own commit kept; uncommitted work restored
    fx.git(&wt, &["merge-base", "--is-ancestor", "main", "feature/a"]);
    assert!(
        fs::read_to_string(wt.join("f.txt"))
            .unwrap()
            .contains("parent work")
    );
    assert!(wt.join("mine.txt").exists());
    assert_eq!(
        fs::read_to_string(wt.join("scratch.txt")).unwrap(),
        "notes\n"
    );
    assert_eq!(fx.git(&wt, &["stash", "list"]).trim(), "");
    // a second sync has nothing to do
    let o2 = run_wt(&wt, &["sync"]);
    assert_ok(&o2);
    assert!(out(&o2).contains("already up to date"), "{}", out(&o2));
}

#[test]
fn sync_conflict_refused_with_guidance() {
    let fx = Fixture::new();
    write_rules(&fx, GROUP_CFG);
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
    write_rules(&fx, ADOPT_CFG);
    let wt = fx.add_worktree("feature/a", "main"); // made with raw git: no record
    assert_eq!(state_read(&wt), StateRead::Missing);
    let o = run_wt(&wt, &["adopt", "--group", "feat", "--parent", "main"]);
    assert_ok(&o);
    let stdout = out(&o);
    assert!(
        stdout.contains("adopted 'feature/a' (group:feat) with parent 'main'"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("replacing"),
        "nothing to replace: {stdout}"
    );
    let s = state_of(&wt);
    assert_eq!(s.branch, "feature/a");
    assert_eq!(s.kind, Kind::Group("feat".into()));
    assert_eq!(s.parent, "main");
    // managed from here on
    let info = run_wt(&wt, &["info"]);
    assert_ok(&info);
    assert!(
        out(&info).contains("identity: group:feat"),
        "{}",
        out(&info)
    );
}

#[test]
fn adopt_free_needs_a_star_in_the_parents_children() {
    let fx = Fixture::new();
    write_rules(&fx, ADOPT_CFG);
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
    write_rules(&fx, ADOPT_CFG);
    fx.git(&fx.repo, &["branch", "dev", "main"]);
    let wt = member(&fx, "feature/a", "feat", "main");
    let o = run_wt(&wt, &["adopt", "--group", "feat2", "--parent", "dev"]);
    assert_ok(&o);
    let stdout = out(&o);
    assert!(
        stdout.contains(
            "replacing the existing record: branch=feature/a, kind=group:feat, parent=main"
        ),
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
    write_rules(&fx, ADOPT_CFG);
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
    write_rules(&fx, ADOPT_CFG);

    // a declared group that is not in the parent's children
    let a = fx.add_worktree("feature/a", "main");
    let o = run_wt(&a, &["adopt", "--group", "other", "--parent", "main"]);
    assert_fail(&o);
    let stderr = err(&o);
    assert!(stderr.contains("refusal: adopt "), "{stderr}");
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
        assert!(
            err(&o).contains("name reservation"),
            "{:?}: {}",
            flags,
            err(&o)
        );
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
    assert!(
        err(&o).contains("parent branch 'ghost' does not exist"),
        "{}",
        err(&o)
    );
}

#[test]
fn adopt_flag_combinations_fail_as_usage_errors() {
    let fx = Fixture::new();
    write_rules(&fx, ADOPT_CFG);
    let a = fx.add_worktree("feature/a", "main");
    for (flags, needle) in [
        (
            vec!["adopt", "--group", "feat", "--free", "--parent", "main"],
            "mutually exclusive",
        ),
        (
            vec!["adopt", "--parent", "main"],
            "one of --group <X> or --free",
        ),
        (
            vec!["adopt", "--group", "feat"],
            "--parent <branch> is required",
        ),
        (
            vec!["adopt", "--free", "--parent", "main", "x"],
            "unknown argument 'x'",
        ),
    ] {
        let o = run_wt(&a, &flags);
        assert_eq!(o.status.code(), Some(2), "{flags:?} must be a usage error");
        assert!(err(&o).contains(needle), "{:?}: {}", flags, err(&o));
        assert!(
            err(&o).contains("usage: wtree adopt"),
            "{:?}: {}",
            flags,
            err(&o)
        );
    }
    assert_eq!(state_read(&a), StateRead::Missing);
}

#[test]
fn merge_refused_while_unmanaged_then_allowed_after_adopt() {
    let fx = Fixture::new();
    write_rules(&fx, ADOPT_CFG);
    let wt = fx.add_worktree("feature/a", "main"); // raw worktree, unmanaged
    fx.commit(&wt, "one");
    let o = run_wt(&wt, &["merge", "-m", "feat: a"]);
    assert_fail(&o);
    let stderr = err(&o);
    assert!(stderr.contains("refusal: merge "), "{stderr}");
    assert!(stderr.contains("unmanaged (fail closed)"), "{stderr}");
    assert!(stderr.contains("wtree adopt"), "{stderr}");
    let main_before = rev(&fx, "main");

    assert_ok(&run_wt(
        &wt,
        &["adopt", "--group", "feat", "--parent", "main"],
    ));
    let o = run_wt(&wt, &["merge", "-m", "feat: a"]);
    assert_ok(&o);
    assert_ne!(rev(&fx, "main"), main_before);
    assert_eq!(rev(&fx, "main"), rev(&fx, "feature/a"));
}

// ----------------------------------------------------------------- destroy ----

fn branches(fx: &Fixture) -> Vec<String> {
    fx.git(
        &fx.repo,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
    )
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
    write_rules(&fx, GROUP_CFG);
    let wt = member(&fx, "feature/a", "feat", "main");
    let o = run_wt(&wt, &["destroy"]);
    assert_ok(&o);
    assert!(out(&o).contains("destroyed worktree"), "{}", out(&o));
    assert!(out(&o).contains("Deleted branch feature/a"), "{}", out(&o));
    assert!(!wt.exists(), "worktree directory survived");
    assert_eq!(branches(&fx), vec!["main".to_string()]);
    // git no longer knows the worktree either
    assert!(
        !fx.git(&fx.repo, &["worktree", "list"])
            .contains("wtree-feature-a")
    );
}

#[test]
fn destroy_refuses_undestroyable_branch_even_with_force() {
    let fx = Fixture::new();
    write_rules(
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
        assert!(
            stderr.contains("destroyable = false"),
            "{flags:?}: {stderr}"
        );
        assert!(
            stderr.contains("--force cannot override"),
            "{flags:?}: {stderr}"
        );
    }
    assert_eq!(branches(&fx), vec!["dev".to_string(), "main".into()]);
    assert!(dev.join("f.txt").exists());
}

#[test]
fn destroy_refuses_the_primary_worktree() {
    let fx = Fixture::new();
    write_rules(&fx, GROUP_CFG);
    let o = run_wt(&fx.repo, &["destroy", "--force"]);
    assert_fail(&o);
    assert!(err(&o).contains("primary worktree"), "{}", err(&o));
    assert_eq!(branches(&fx), vec!["main".to_string()]);
    assert!(fx.repo.join("f.txt").exists());
}

#[test]
fn destroy_refuses_a_live_non_ephemeral_child_even_with_force() {
    let fx = Fixture::new();
    write_rules(&fx, NESTED_CFG);
    let a = member(&fx, "feature/a", "feat", "main");
    let s = member(&fx, "sub/x", "sub", "feature/a");
    for flags in [vec!["destroy"], vec!["destroy", "--force"]] {
        let o = run_wt(&a, &flags);
        assert_fail(&o);
        let stderr = err(&o);
        assert!(stderr.contains("'sub/x'"), "{flags:?}: {stderr}");
        assert!(stderr.contains("not ephemeral"), "{flags:?}: {stderr}");
        assert!(
            stderr.contains("--force cannot override"),
            "{flags:?}: {stderr}"
        );
    }
    assert!(a.is_dir() && s.is_dir());
    assert_eq!(branches(&fx).len(), 3);
}

#[test]
fn destroy_cascades_ephemeral_children_leaf_first() {
    let fx = Fixture::new();
    write_rules(&fx, EPH_CFG);
    let mid = member(&fx, "mid/a", "mid", "main");
    let e1 = member(&fx, "eph/1", "eph", "mid/a");
    let e2 = member(&fx, "eph/2", "eph", "eph/1");
    let o = run_wt(&mid, &["destroy"]);
    assert_ok(&o);
    let stdout = out(&o);
    let at = |s: &str| {
        stdout
            .find(s)
            .unwrap_or_else(|| panic!("missing {s} in:\n{stdout}"))
    };
    assert!(
        at("wtree-eph-2") < at("wtree-eph-1"),
        "leaf first:\n{stdout}"
    );
    assert!(
        at("wtree-eph-1") < at("wtree-mid-a"),
        "children before the parent:\n{stdout}"
    );
    assert!(!mid.exists() && !e1.exists() && !e2.exists());
    assert_eq!(branches(&fx), vec!["main".to_string()]);
}

#[test]
fn destroy_refuses_the_whole_cascade_when_one_child_is_dirty() {
    let fx = Fixture::new();
    write_rules(&fx, EPH_CFG);
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
    write_rules(&fx, GROUP_CFG);
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
    write_rules(&fx, GROUP_CFG);
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "one");
    fx.commit(&wt, "two");
    // squashed into main by hand: the content is reflected, the ancestry is not
    fx.git(&fx.repo, &["merge", "--squash", "feature/a"]);
    fx.git(&fx.repo, &["commit", "-q", "-m", "squashed"]);
    commit_other(&fx, &fx.repo, "other.txt", "main moved on");
    assert!(
        !fx.git(&fx.repo, &["branch", "--merged", "main"])
            .contains("feature/a"),
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
    write_rules(&fx, MIDDLE_CFG);
    fx.git(&fx.repo, &["branch", "dev", "main"]); // exists, has no worktree
    let o = run_wt(&fx.repo, &["open", "dev"]);
    assert_ok(&o);
    let dest = default_dest(&fx, "dev");
    let stdout = out(&o);
    assert!(stdout.contains("opened 'dev' (fixed)"), "{stdout}");
    assert!(
        stdout.contains(&format!("cd {}", dest.display())),
        "{stdout}"
    );
    assert!(
        !stdout.contains("wtree adopt"),
        "a declared branch is managed already: {stdout}"
    );
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
    write_rules(&fx, ADOPT_CFG);
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
    assert_ok(&run_wt(
        &dest,
        &["adopt", "--group", "feat", "--parent", "main"],
    ));
    let s = state_of(&dest);
    assert_eq!(s.branch, "feature/a");
    assert_eq!(s.kind, Kind::Group("feat".into()));
    assert!(out(&run_wt(&dest, &["info"])).contains("identity: group:feat"));
}

#[test]
fn open_and_new_point_at_each_other() {
    let fx = Fixture::new();
    write_rules(&fx, GROUP_CFG);
    // open has no branch to attach to
    let o = run_wt(&fx.repo, &["open", "feature/ghost"]);
    assert_fail(&o);
    let stderr = err(&o);
    assert!(stderr.contains("refusal: open "), "{stderr}");
    assert!(
        stderr.contains("branch 'feature/ghost' does not exist"),
        "{stderr}"
    );
    assert!(stderr.contains("wtree new feature/ghost"), "{stderr}");
    assert!(!default_dest(&fx, "feature/ghost").exists());

    let wt = member(&fx, "feature/a", "feat", "main");
    // new has a branch already, and open is what the caller meant
    let o = run_wt(&fx.repo, &["new", "feature/a"]);
    assert_fail(&o);
    let stderr = err(&o);
    assert!(
        stderr.contains("branch 'feature/a' already exists"),
        "{stderr}"
    );
    assert!(stderr.contains("wtree open feature/a"), "{stderr}");

    // and that open is refused too, because the branch is checked out already
    let o = run_wt(&fx.repo, &["open", "feature/a"]);
    assert_fail(&o);
    let stderr = err(&o);
    assert!(stderr.contains("already checked out at"), "{stderr}");
    assert!(
        stderr.contains("wtree-feature-a"),
        "the path must be named: {stderr}"
    );
    assert!(wt.is_dir());
}

#[test]
fn close_keeps_a_protected_fixed_branch() {
    let fx = Fixture::new();
    write_rules(
        &fx,
        "[main]\nchildren = dev\n\n[dev]\ndestroyable = false\n",
    );
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
    assert!(
        !stdout.contains("unmanaged now"),
        "a declared branch stays managed: {stdout}"
    );
    assert!(!dev.exists());
    assert_eq!(branches(&fx), vec!["dev".to_string(), "main".into()]);
    assert!(
        !fx.git(&fx.repo, &["worktree", "list"])
            .contains("repo.worktrees")
    );
}

#[test]
fn close_fixed_parent_still_receives_its_children() {
    let fx = Fixture::new();
    write_rules(&fx, MIDDLE_CFG);
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
    assert!(
        out(&o).contains("merged 'feature/a' onto 'dev'"),
        "{}",
        out(&o)
    );
    assert_eq!(rev(&fx, "dev"), rev(&fx, "feature/a"));
}

/// A parent with no worktree is moved by `update-ref`, not by `git merge`, so
/// the reflog line is whatever wtree hands over — and it is the only record of
/// who moved a branch the user was not standing on.
#[test]
fn moving_a_parent_nobody_stands_on_names_the_verb_in_its_reflog() {
    let fx = Fixture::new();
    write_rules(&fx, MIDDLE_CFG);
    assert_ok(&run_wt(&fx.repo, &["new", "dev"]));
    let dev = default_dest(&fx, "dev");
    assert_ok(&run_wt(&dev, &["new", "feature/a"]));
    let a = default_dest(&fx, "feature/a");
    assert_ok(&run_wt(&dev, &["close"]));

    let subject = |fx: &Fixture| {
        fx.git(&fx.repo, &["reflog", "show", "--format=%gs", "dev"])
            .lines()
            .next()
            .unwrap_or_default()
            .to_string()
    };

    fx.commit(&a, "work");
    assert_ok(&run_wt(&a, &["merge", "-m", "feat: a"]));
    assert_eq!(subject(&fx), "wtree merge: feature/a");

    // land goes through the same step and says land, matching the name the
    // stash it parked would have carried.
    fx.commit(&a, "more");
    assert_ok(&run_wt(&a, &["land", "-m", "feat: a again"]));
    assert_eq!(subject(&fx), "wtree land: feature/a");
}

#[test]
fn close_refuses_a_group_branch_with_live_children() {
    let fx = Fixture::new();
    write_rules(&fx, NESTED_CFG);
    let a = member(&fx, "feature/a", "feat", "main");
    let sub = member(&fx, "sub/x", "sub", "feature/a");
    let o = run_wt(&a, &["close"]);
    assert_fail(&o);
    let stderr = err(&o);
    assert!(stderr.contains("refusal: close "), "{stderr}");
    assert!(stderr.contains("orphans its children"), "{stderr}");
    assert!(stderr.contains("'sub/x'"), "{stderr}");
    assert!(a.is_dir() && sub.is_dir());

    // an ephemeral child blocks just the same: close never cascades, so it
    // would be left behind with an unmanaged parent
    let fx = Fixture::new();
    write_rules(&fx, EPH_CFG);
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
    write_rules(&fx, GROUP_CFG);
    let wt = member(&fx, "feature/a", "feat", "main");
    let o = run_wt(&wt, &["close"]);
    assert_ok(&o);
    let stdout = out(&o);
    assert!(stdout.contains("branch 'feature/a' is kept"), "{stdout}");
    assert!(stdout.contains("unmanaged now"), "{stdout}");
    assert!(!wt.exists());
    assert_eq!(branches(&fx), vec!["feature/a".to_string(), "main".into()]);
    // the record lived in the worktree, so the branch reads as unknown now
    let l = run_wt(&fx.repo, &["list", "--unmanaged"]);
    assert_ok(&l);
    assert!(out(&l).contains("[unmanaged branch]"), "{}", out(&l));
    assert!(out(&l).contains("\nfeature/a\n"), "{}", out(&l));
}

#[test]
fn close_dirty_worktree_needs_the_confirmation_key() {
    let fx = Fixture::new();
    write_rules(&fx, GROUP_CFG);
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "unmerged work");
    fx.make_dirty(&wt);

    let o = run_wt(&wt, &["close"]);
    assert_fail(&o);
    let stderr = err(&o);
    assert!(
        stderr.contains("uncommitted changes go with the worktree"),
        "{stderr}"
    );
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
    write_rules(&fx, GROUP_CFG);
    let o = run_wt(&fx.repo, &["close"]);
    assert_fail(&o);
    assert!(err(&o).contains("primary worktree"), "{}", err(&o));
    assert!(fx.repo.join("f.txt").exists());
    assert_eq!(branches(&fx), vec!["main".to_string()]);
}

#[test]
fn open_close_round_trip_keeps_a_fixed_branch_working() {
    let fx = Fixture::new();
    write_rules(&fx, MIDDLE_CFG);
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
    write_rules(
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
    assert!(
        err(&o).contains("accepts squash merges only"),
        "{}",
        err(&o)
    );

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
        assert!(
            stderr.contains("parent 'mid/a' is unmanaged"),
            "{flags:?}: {stderr}"
        );
        assert!(stderr.contains("fail closed"), "{flags:?}: {stderr}");
        assert!(stderr.contains("wtree open mid/a"), "{flags:?}: {stderr}");
    }
    assert_eq!(rev(&fx, "mid/a"), mid_before, "nothing may have landed");
    // info does not advertise the modes merge will refuse, either
    let info = out(&run_wt(&leaf, &["info"]));
    assert!(
        info.contains("merge to 'mid/a': no rules readable"),
        "{info}"
    );

    // reopening and re-adopting the parent puts its rules back in force
    assert_ok(&run_wt(&fx.repo, &["open", "mid/a"]));
    let reopened = default_dest(&fx, "mid/a");
    assert_ok(&run_wt(
        &reopened,
        &["adopt", "--group", "mid", "--parent", "main"],
    ));
    let o = run_wt(&leaf, &["merge", "--no-ff", "-m", "x"]);
    assert_fail(&o);
    assert!(
        err(&o).contains("accepts squash merges only"),
        "{}",
        err(&o)
    );
    assert_ok(&run_wt(&leaf, &["merge", "--squash", "-m", "feat: x"]));
    assert_eq!(rev(&fx, "mid/a"), rev(&fx, "leaf/x"));
}

// -------------------------------------------------------------------- land ----

#[test]
fn land_merges_then_destroys() {
    let fx = Fixture::new();
    write_rules(&fx, &merge_cfg("squash"));
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "one");
    fx.commit(&wt, "two");
    let before = rev(&fx, "main");
    let o = run_wt(&wt, &["land", "-m", "feat: a"]);
    assert_ok(&o);
    let stdout = out(&o);
    assert!(
        stdout.contains("merged 'feature/a' onto 'main'"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("worktree kept"),
        "land removes it: {stdout}"
    );
    assert!(stdout.contains("destroyed worktree"), "{stdout}");
    let count = fx.git(
        &fx.repo,
        &["rev-list", "--count", &format!("{before}..main")],
    );
    assert_eq!(count.trim(), "1");
    assert_eq!(
        fx.git(&fx.repo, &["log", "-1", "--format=%s", "main"])
            .trim(),
        "feat: a"
    );
    assert!(
        fs::read_to_string(fx.repo.join("f.txt"))
            .unwrap()
            .contains("two")
    );
    assert!(!wt.exists());
    assert_eq!(branches(&fx), vec!["main".to_string()]);
}

#[test]
fn land_preflight_refuses_before_merging() {
    let fx = Fixture::new();
    write_rules(&fx, NESTED_CFG);
    let a = member(&fx, "feature/a", "feat", "main");
    let s = member(&fx, "sub/x", "sub", "feature/a");
    fx.commit(&a, "one");
    let main_before = rev(&fx, "main");
    let feat_before = rev(&fx, "feature/a");
    let o = run_wt(&a, &["land", "-m", "feat: a"]);
    assert_fail(&o);
    let stderr = err(&o);
    // attributed to the verb that was typed, not to the half that judged it
    assert!(stderr.contains("refusal: land "), "{stderr}");
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
    write_rules(&fx, &merge_cfg("squash"));
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "one");
    fx.make_dirty(&wt);
    let main_before = rev(&fx, "main");
    let o = run_wt(&wt, &["land", "-m", "feat: a"]);
    assert_fail(&o);
    let stderr = err(&o);
    assert!(stderr.contains("uncommitted changes"), "{stderr}");
    assert!(
        stderr.contains("`wtree merge` and then `wtree destroy`"),
        "{stderr}"
    );
    assert_eq!(rev(&fx, "main"), main_before);
    assert!(wt.is_dir());
    assert!(wt.join("scratch.txt").exists());
}

#[test]
fn land_with_nothing_to_merge_goes_straight_to_destroy() {
    let fx = Fixture::new();
    write_rules(&fx, &merge_cfg("squash"));
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

// ------------------------------------------------- hooks: merge / destroy ----
//
// The create pair is covered up in the `new` section. What is left is the two
// pairs that wrap operations with something to lose, and the flag that skips
// all of them.

/// pre-merge is the gate a test suite would sit behind, so its refusal has to
/// mean the target never moved and the branch was never rewritten.
#[test]
fn pre_merge_refusal_leaves_both_branches_alone() {
    let fx = Fixture::new();
    write_rules(&fx, &merge_cfg("squash"));
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "one");
    let (main_before, feat_before) = (rev(&fx, "main"), rev(&fx, "feature/a"));

    install_hook(&fx, "pre-merge", "#!/bin/sh\nexit 2\n");
    let o = run_wt(&wt, &["merge", "-m", "feat: a"]);
    assert_fail(&o);
    assert!(
        err(&o).contains("pre-merge hook failed (exit 2)")
            && err(&o).contains("nothing was changed"),
        "{}",
        err(&o)
    );
    assert_eq!(rev(&fx, "main"), main_before);
    assert_eq!(rev(&fx, "feature/a"), feat_before, "no squash was applied");
}

/// The merge pair's environment, including the one field only post-merge gets.
#[test]
fn merge_hooks_report_mode_target_and_tip() {
    let fx = Fixture::new();
    write_rules(&fx, &merge_cfg("squash"));
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "one");
    let fields = "$WTREE_HOOK|$WTREE_BRANCH|$WTREE_TARGET|$WTREE_MODE|$WTREE_MESSAGE|$WTREE_VERB|$WTREE_TIP|$(pwd)";
    install_hook(&fx, "pre-merge", &logging_hook(fields));
    install_hook(&fx, "post-merge", &logging_hook(fields));

    assert_ok(&run_wt(&wt, &["merge", "-m", "feat: a"]));
    let log = hook_log(&fx);
    assert_eq!(log.len(), 2, "both halves run: {log:?}");
    let pre: Vec<&str> = log[0].split('|').collect();
    let post: Vec<&str> = log[1].split('|').collect();
    assert_eq!(
        &pre[..6],
        &[
            "pre-merge",
            "feature/a",
            "main",
            "squash",
            "feat: a",
            "merge"
        ]
    );
    assert_eq!(pre[6], "", "the tip is not known before the merge");
    assert_eq!(post[0], "post-merge");
    assert_eq!(
        rev(&fx, "main")[..post[6].len()].to_string(),
        post[6],
        "WTREE_TIP is the commit the merge produced"
    );
    for h in [&pre, &post] {
        assert_eq!(
            Path::new(h[7]).canonicalize().unwrap(),
            wt.canonicalize().unwrap(),
            "merge hooks run in the worktree being merged"
        );
    }
}

/// --ff returns from `run_merge` before the precheck, the stash and the
/// rewrite, so it reaches its own post-merge call. Both halves still run, and
/// the mode that takes no message reports an empty one.
#[test]
fn merge_hooks_run_on_the_ff_path_too() {
    let fx = Fixture::new();
    write_rules(&fx, &merge_cfg("ff"));
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "one");
    let fields = "$WTREE_HOOK|$WTREE_MODE|$WTREE_MESSAGE|$WTREE_TIP";
    install_hook(&fx, "pre-merge", &logging_hook(fields));
    install_hook(&fx, "post-merge", &logging_hook(fields));

    assert_ok(&run_wt(&wt, &["merge"]));
    let log = hook_log(&fx);
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!(log[0], "pre-merge|ff||", "--ff carries no -m text");
    let post: Vec<&str> = log[1].split('|').collect();
    assert_eq!(&post[..3], &["post-merge", "ff", ""]);
    assert_eq!(rev(&fx, "main")[..post[3].len()].to_string(), post[3]);
}

/// The `post-` contract away from `new`: a failure is a warning naming what
/// stands, and the verb still succeeds. The merge and the destroy have
/// different things standing, so each says its own.
#[test]
fn post_hook_failure_warns_without_undoing_the_verb() {
    let fx = Fixture::new();
    write_rules(&fx, &merge_cfg("squash"));
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "one");
    install_hook(&fx, "post-merge", "#!/bin/sh\nexit 4\n");
    install_hook(&fx, "post-destroy", "#!/bin/sh\nexit 5\n");

    let o = run_wt(&wt, &["merge", "-m", "feat: a"]);
    assert_ok(&o);
    assert!(
        err(&o).contains("post-merge hook failed (exit 4); the merge already happened"),
        "{}",
        err(&o)
    );
    assert_eq!(rev(&fx, "main"), rev(&fx, "feature/a"), "the merge stands");

    let o = run_wt(&wt, &["destroy"]);
    assert_ok(&o);
    assert!(
        err(&o).contains("post-destroy hook failed (exit 5); the branch was already removed"),
        "{}",
        err(&o)
    );
    assert_eq!(branches(&fx), vec!["main".to_string()]);
}

/// A hook file that is present but not executable is the shape a half-finished
/// install leaves behind. It is called out rather than run or ignored, and it
/// does not refuse for the `pre-` half — an unrunnable gate is not a verdict.
#[test]
fn a_non_executable_hook_is_reported_and_skipped() {
    let fx = Fixture::new();
    write_rules(&fx, GROUP_CFG);
    let hooks = fx.repo.join(".git/wtree/hooks");
    fs::create_dir_all(&hooks).unwrap();
    fs::write(hooks.join("pre-create"), "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(hooks.join("pre-create"), fs::Permissions::from_mode(0o644)).unwrap();

    let o = run_wt(&fx.repo, &["new", "feature/a"]);
    assert_ok(&o);
    assert!(
        err(&o).contains("is not executable") && err(&o).contains("skipped"),
        "{}",
        err(&o)
    );
    assert!(default_dest(&fx, "feature/a").is_dir());
}

/// A cascade removes several branches, so every gate has to clear before the
/// first removal — a hook that refused halfway could not put back what was
/// already gone.
#[test]
fn pre_destroy_refuses_the_whole_cascade_before_removing_anything() {
    let fx = Fixture::new();
    write_rules(&fx, EPH_CFG);
    let mid = member(&fx, "mid/a", "mid", "main");
    let e1 = member(&fx, "eph/1", "eph", "mid/a");
    // Clears for the leaf, refuses for its parent: the refusal is reached only
    // after a gate has already passed, which is where a partial run would show.
    install_hook(
        &fx,
        "pre-destroy",
        "#!/bin/sh\ncase \"$WTREE_BRANCH\" in mid/*) exit 1 ;; *) exit 0 ;; esac\n",
    );
    let o = run_wt(&mid, &["destroy"]);
    assert_fail(&o);
    assert!(
        err(&o).contains("pre-destroy hook failed") && err(&o).contains("nothing was removed"),
        "{}",
        err(&o)
    );
    assert!(mid.exists() && e1.exists());
    assert_eq!(
        branches(&fx),
        vec!["eph/1".to_string(), "main".to_string(), "mid/a".to_string()]
    );
}

/// post-destroy has nowhere to stand but the repo root, and the path it is
/// handed is one that has just stopped existing.
#[test]
fn post_destroy_runs_from_the_root_after_the_worktree_is_gone() {
    let fx = Fixture::new();
    write_rules(&fx, GROUP_CFG);
    let wt = member(&fx, "feature/a", "feat", "main");
    install_hook(
        &fx,
        "post-destroy",
        &logging_hook(
            "$WTREE_HOOK|$WTREE_BRANCH|$WTREE_VERB|$(pwd)|$([ -e \"$WTREE_PATH\" ] && echo exists || echo absent)",
        ),
    );
    assert_ok(&run_wt(&wt, &["destroy"]));
    let log = hook_log(&fx);
    assert_eq!(log.len(), 1, "{log:?}");
    let f: Vec<&str> = log[0].split('|').collect();
    assert_eq!(&f[..3], &["post-destroy", "feature/a", "destroy"]);
    assert_eq!(
        Path::new(f[3]).canonicalize().unwrap(),
        fx.repo.canonicalize().unwrap()
    );
    assert_eq!(f[4], "absent", "the worktree is already removed");
}

/// The destroy pair is per branch, not per command: a cascade fires it once
/// for each, in the order the removals happen.
#[test]
fn destroy_hooks_fire_once_per_branch_in_a_cascade() {
    let fx = Fixture::new();
    write_rules(&fx, EPH_CFG);
    let mid = member(&fx, "mid/a", "mid", "main");
    member(&fx, "eph/1", "eph", "mid/a");
    for h in ["pre-destroy", "post-destroy"] {
        install_hook(&fx, h, &logging_hook("$WTREE_HOOK|$WTREE_BRANCH"));
    }
    assert_ok(&run_wt(&mid, &["destroy"]));
    assert_eq!(
        hook_log(&fx),
        vec![
            // every gate first, leaf first, then the removals in the same order
            "pre-destroy|eph/1",
            "pre-destroy|mid/a",
            "post-destroy|eph/1",
            "post-destroy|mid/a",
        ]
    );
}

/// `land` is a merge and a destroy, so it fires both pairs; WTREE_VERB is how a
/// hook tells it apart from the verbs it is made of. Both gates come before
/// the merge — the same both-or-neither reason the preflight judges both
/// halves: a pre-destroy veto after the merge could no longer abort the verb.
#[test]
fn land_fires_both_pairs_and_names_itself() {
    let fx = Fixture::new();
    write_rules(&fx, &merge_cfg("squash"));
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "one");
    for h in ["pre-merge", "post-merge", "pre-destroy", "post-destroy"] {
        install_hook(&fx, h, &logging_hook("$WTREE_HOOK|$WTREE_VERB"));
    }
    assert_ok(&run_wt(&wt, &["land", "-m", "feat: a"]));
    assert_eq!(
        hook_log(&fx),
        vec![
            "pre-merge|land",
            "pre-destroy|land",
            "post-merge|land",
            "post-destroy|land",
        ]
    );
}

/// The escape hatch: one run with neither gate nor report, so a broken hook
/// never has to be chmod-ed off and remembered back.
#[test]
fn no_hooks_skips_both_halves() {
    let fx = Fixture::new();
    write_rules(&fx, &merge_cfg("squash"));
    for h in [
        "pre-create",
        "post-create",
        "pre-merge",
        "post-merge",
        "pre-destroy",
        "post-destroy",
    ] {
        // Refuses and logs: --no-hooks has to defeat both halves of every pair.
        install_hook(
            &fx,
            h,
            &format!("{}exit 1\n", logging_hook("$WTREE_HOOK|ran")),
        );
    }
    assert_ok(&run_wt(&fx.repo, &["new", "feature/a", "--no-hooks"]));
    let wt = default_dest(&fx, "feature/a");
    fx.commit(&wt, "one");
    assert_ok(&run_wt(&wt, &["merge", "-m", "feat: a", "--no-hooks"]));
    assert_ok(&run_wt(&wt, &["destroy", "--no-hooks"]));
    assert!(hook_log(&fx).is_empty(), "{:?}", hook_log(&fx));
    assert_eq!(branches(&fx), vec!["main".to_string()]);
}

/// The two verbs that move nothing of the caller's own making stay silent:
/// hooks wrap operations with something to lose, and these have none.
#[test]
fn sync_and_close_run_no_hooks() {
    let fx = Fixture::new();
    write_rules(&fx, &merge_cfg("squash"));
    let wt = member(&fx, "feature/a", "feat", "main");
    commit_other(&fx, &fx.repo.clone(), "p.txt", "parent moved");
    for h in [
        "pre-create",
        "post-create",
        "pre-merge",
        "post-merge",
        "pre-destroy",
        "post-destroy",
    ] {
        install_hook(&fx, h, &logging_hook("$WTREE_HOOK"));
    }
    assert_ok(&run_wt(&wt, &["sync"]));
    assert_ok(&run_wt(&wt, &["close"]));
    assert!(hook_log(&fx).is_empty(), "{:?}", hook_log(&fx));
}

/// The gate fires before the stash machinery: a veto must leave the working
/// tree exactly as it was, with nothing parked in the stash.
#[test]
fn a_pre_merge_veto_stashes_nothing() {
    let fx = Fixture::new();
    write_rules(&fx, &merge_cfg("squash"));
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "one");
    fs::write(wt.join("wip.txt"), "work in progress\n").unwrap();
    install_hook(&fx, "pre-merge", "#!/bin/sh\nexit 2\n");
    let o = run_wt(&wt, &["merge", "-m", "feat: a"]);
    assert_fail(&o);
    assert_eq!(
        fs::read_to_string(wt.join("wip.txt")).unwrap(),
        "work in progress\n",
        "the veto must leave the working tree as it was"
    );
    assert_eq!(
        fx.git(&wt, &["stash", "list"]).trim(),
        "",
        "a veto must not park the caller's work in the stash"
    );
}

/// WTREE_MODE names the mode that actually ran, and WTREE_TIP the commit the
/// merge produced — on the modes the squash-centric tests don't touch.
#[test]
fn merge_hooks_name_rebase_and_no_ff() {
    for (mode, args) in [
        ("rebase", vec!["merge", "--rebase"]),
        ("no-ff", vec!["merge", "--no-ff", "-m", "feat: a"]),
    ] {
        let fx = Fixture::new();
        write_rules(&fx, &merge_cfg("squash, rebase, no-ff, ff"));
        let wt = member(&fx, "feature/a", "feat", "main");
        fx.commit(&wt, "one");
        let fields = "$WTREE_HOOK|$WTREE_MODE|$WTREE_MESSAGE|$WTREE_TIP";
        install_hook(&fx, "pre-merge", &logging_hook(fields));
        install_hook(&fx, "post-merge", &logging_hook(fields));
        let o = run_wt(&wt, &args);
        assert_ok(&o);
        let log = hook_log(&fx);
        assert_eq!(log.len(), 2, "{mode}: {log:?}");
        let pre: Vec<&str> = log[0].split('|').collect();
        let post: Vec<&str> = log[1].split('|').collect();
        assert_eq!(pre[1], mode, "pre WTREE_MODE: {log:?}");
        assert_eq!(post[1], mode, "post WTREE_MODE: {log:?}");
        assert_eq!(pre[3], "", "the tip is not known before the merge");
        assert_eq!(
            rev(&fx, "main")[..post[3].len()].to_string(),
            post[3],
            "{mode}: WTREE_TIP is the commit the merge produced"
        );
    }
}

/// "nothing to merge; going straight to destroy" skips the merge pair along
/// with the merge, but the destroy pair still wraps the removal.
#[test]
fn land_with_nothing_to_merge_runs_only_the_destroy_pair() {
    let fx = Fixture::new();
    write_rules(&fx, &merge_cfg("squash"));
    let wt = member(&fx, "feature/a", "feat", "main");
    for h in ["pre-merge", "post-merge", "pre-destroy", "post-destroy"] {
        install_hook(&fx, h, &logging_hook("$WTREE_HOOK"));
    }
    assert_ok(&run_wt(&wt, &["land", "-m", "feat: a"]));
    assert_eq!(hook_log(&fx), vec!["pre-destroy", "post-destroy"]);
}

/// `nothing to merge` is decided before the gate: a refusal there would blame
/// a hook for a merge that was never going to happen.
#[test]
fn merge_with_nothing_to_merge_never_reaches_the_gate() {
    let fx = Fixture::new();
    write_rules(&fx, &merge_cfg("squash"));
    let wt = member(&fx, "feature/a", "feat", "main");
    install_hook(&fx, "pre-merge", &logging_hook("$WTREE_HOOK"));
    let o = run_wt(&wt, &["merge", "-m", "feat: a"]);
    assert_fail(&o);
    assert!(hook_log(&fx).is_empty(), "{:?}", hook_log(&fx));
}

#[test]
fn open_no_hooks_skips_the_post_half() {
    let fx = Fixture::new();
    write_rules(&fx, MIDDLE_CFG);
    fx.git(&fx.repo, &["branch", "dev", "main"]);
    install_hook(&fx, "post-create", &logging_hook("$WTREE_HOOK"));
    assert_ok(&run_wt(&fx.repo, &["open", "dev", "--no-hooks"]));
    assert!(hook_log(&fx).is_empty(), "{:?}", hook_log(&fx));
}

#[test]
fn land_no_hooks_skips_every_pair() {
    let fx = Fixture::new();
    write_rules(&fx, &merge_cfg("squash"));
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "one");
    for h in ["pre-merge", "post-merge", "pre-destroy", "post-destroy"] {
        install_hook(&fx, h, &format!("{}exit 1\n", logging_hook("$WTREE_HOOK")));
    }
    assert_ok(&run_wt(&wt, &["land", "-m", "feat: a", "--no-hooks"]));
    assert!(hook_log(&fx).is_empty(), "{:?}", hook_log(&fx));
    assert_eq!(branches(&fx), vec!["main".to_string()]);
}

/// The merge pair addresses the worktree being merged, not wherever the
/// command happens to run.
#[test]
fn merge_hooks_get_the_worktree_path() {
    let fx = Fixture::new();
    write_rules(&fx, &merge_cfg("squash"));
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "one");
    install_hook(&fx, "pre-merge", &logging_hook("$WTREE_HOOK|$WTREE_PATH"));
    assert_ok(&run_wt(&wt, &["merge", "-m", "feat: a"]));
    let f: Vec<String> = hook_log(&fx)[0].split('|').map(str::to_string).collect();
    assert_eq!(
        Path::new(&f[1]).canonicalize().unwrap(),
        wt.canonicalize().unwrap()
    );
}

/// The gate runs while the worktree still exists — a last look before removal.
#[test]
fn pre_destroy_gets_the_worktree_that_is_still_there() {
    let fx = Fixture::new();
    write_rules(&fx, GROUP_CFG);
    let wt = member(&fx, "feature/a", "feat", "main");
    install_hook(
        &fx,
        "pre-destroy",
        &logging_hook("$WTREE_PATH|$([ -e \"$WTREE_PATH\" ] && echo exists || echo absent)"),
    );
    assert_ok(&run_wt(&wt, &["destroy"]));
    let f: Vec<String> = hook_log(&fx)[0].split('|').map(str::to_string).collect();
    assert_eq!(f[1], "exists", "the worktree is still there under the gate");
    assert_eq!(Path::new(&f[0]), wt);
}

/// A hook that exists but cannot run (broken shebang) is a refusal for the
/// pre- half, not a silent pass: the gate's absence must be deliberate.
#[test]
fn an_unrunnable_hook_refuses_for_the_pre_half() {
    let fx = Fixture::new();
    write_rules(&fx, GROUP_CFG);
    install_hook(&fx, "pre-create", "#!/nonexistent/interpreter\n");
    let o = run_wt(&fx.repo, &["new", "feature/a"]);
    assert_fail(&o);
    assert!(
        err(&o).contains("cannot run pre-create hook"),
        "{}",
        err(&o)
    );
    assert!(!default_dest(&fx, "feature/a").exists());
}

/// --force is spent only on what the plan approved. Dirt that appears after
/// the judgment — here, a gate hook's own file — was confirmed by nothing,
/// so the removal refuses instead of discarding it.
#[test]
fn destroy_refuses_dirt_a_gate_hook_just_made() {
    let fx = Fixture::new();
    write_rules(&fx, GROUP_CFG);
    let wt = member(&fx, "feature/a", "feat", "main");
    install_hook(
        &fx,
        "pre-destroy",
        "#!/bin/sh\nprintf x > \"$WTREE_PATH/made.tmp\"\n",
    );
    let o = run_wt(&wt, &["destroy"]);
    assert_fail(&o);
    assert!(
        wt.join("made.tmp").exists(),
        "the file must survive:\n{}",
        err(&o)
    );
    assert!(
        err(&o).contains("changed since its removal was approved"),
        "{}",
        err(&o)
    );
}

/// The gates ran against the preflight's target set: a destroy target that
/// appears afterwards (here, a hook creating an ephemeral child mid-land)
/// has had no gate, so land refuses to remove anything at all.
#[test]
fn land_refuses_a_destroy_target_that_appeared_after_the_gates() {
    let fx = Fixture::new();
    write_rules(
        &fx,
        "[main]\nchildren = group:mid\nmerge-mode = squash\n\n\
         [group:mid]\nname-allow = mid/*\nchildren = group:eph\n\n\
         [group:eph]\nname-allow = eph/*\nephemeral = true\n",
    );
    let mid = member(&fx, "mid/a", "mid", "main");
    fx.commit(&mid, "one");
    install_hook(
        &fx,
        "post-merge",
        &format!(
            "#!/bin/sh\n{} new eph/1 >/dev/null\n",
            env!("CARGO_BIN_EXE_wtree")
        ),
    );
    let o = run_wt(&mid, &["land", "-m", "feat: a"]);
    assert_fail(&o);
    assert!(err(&o).contains("eph/1"), "{}", err(&o));
    assert_eq!(rev(&fx, "main"), rev(&fx, "mid/a"), "the merge stands");
    assert!(mid.is_dir(), "nothing was destroyed");
    assert!(
        default_dest(&fx, "eph/1").is_dir(),
        "the ungated child survives"
    );
}

/// "nothing to merge" is decided before the gates run; a gate hook that
/// commits reverses that decision, and the destroy would discard the commit
/// on land's own key. The question is re-asked after the gates.
#[test]
fn land_with_nothing_to_merge_stops_when_a_gate_commits() {
    let fx = Fixture::new();
    write_rules(&fx, &merge_cfg("squash"));
    let wt = member(&fx, "feature/a", "feat", "main");
    install_hook(
        &fx,
        "pre-destroy",
        "#!/bin/sh\ncd \"$WTREE_PATH\" && printf x > s.txt && git add s.txt && git commit -q -m stray\n",
    );
    let o = run_wt(&wt, &["land", "-m", "feat: a"]);
    assert_fail(&o);
    assert!(err(&o).starts_with("stopped:"), "{}", err(&o));
    assert!(err(&o).contains("stray"), "{}", err(&o));
    assert!(wt.is_dir(), "the worktree survives");
    assert!(
        branches(&fx).contains(&"feature/a".to_string()),
        "the branch keeps the commit"
    );
}

/// A commit made during the run (a version-bump post-merge hook, a replaced
/// source with new history) moves the source past the merge result, and the
/// destroy half would discard it on the strength of land's own key. land
/// verifies the merge's outcome still holds — source reflected in target —
/// before removing anything.
#[test]
fn land_stops_when_the_source_moved_past_the_merge_result() {
    let fx = Fixture::new();
    write_rules(&fx, &merge_cfg("squash"));
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "one");
    install_hook(
        &fx,
        "post-merge",
        "#!/bin/sh\ncd \"$WTREE_PATH\" && printf x > bump.txt && git add bump.txt && git commit -q -m 'v2: bump'\n",
    );
    let o = run_wt(&wt, &["land", "-m", "feat: a"]);
    assert_fail(&o);
    assert!(err(&o).starts_with("stopped:"), "{}", err(&o));
    assert!(err(&o).contains("bump"), "{}", err(&o));
    assert!(wt.is_dir(), "the worktree survives");
    assert_ne!(
        rev(&fx, "feature/a"),
        rev(&fx, "main"),
        "the stray commit is kept on the branch"
    );
}

/// Same-name replacement is not the same target: a hook that destroys a
/// gated child and recreates the branch leaves the name in the plan but the
/// gate's judgment behind, so land refuses the destroy half whole.
#[test]
fn land_refuses_a_gated_target_that_was_replaced() {
    let fx = Fixture::new();
    write_rules(
        &fx,
        "[main]\nchildren = group:mid\nmerge-mode = squash\n\n\
         [group:mid]\nname-allow = mid/*\nchildren = group:eph\n\n\
         [group:eph]\nname-allow = eph/*\nephemeral = true\n",
    );
    let mid = member(&fx, "mid/a", "mid", "main");
    let kid = member(&fx, "eph/1", "eph", "mid/a");
    fx.commit(&mid, "one");
    let bin = env!("CARGO_BIN_EXE_wtree");
    install_hook(
        &fx,
        "post-merge",
        &format!(
            "#!/bin/sh\ncd {} && {bin} destroy >/dev/null\ncd \"$WTREE_PATH\" && {bin} new eph/1 >/dev/null\n",
            kid.display()
        ),
    );
    let o = run_wt(&mid, &["land", "-m", "feat: a"]);
    assert_fail(&o);
    assert!(err(&o).contains("eph/1"), "{}", err(&o));
    assert_eq!(rev(&fx, "main"), rev(&fx, "mid/a"), "the merge stands");
    assert!(mid.is_dir(), "nothing was destroyed");
    assert!(
        default_dest(&fx, "eph/1").is_dir(),
        "the replacement survives"
    );
}

/// A commit is clean by every dirtiness test, but it is still state the plan
/// never judged: a post-destroy hook committing into a later cascade target
/// must stop that target's removal, not watch branch -D discard the commit.
#[test]
fn removal_refuses_a_commit_made_behind_the_plan() {
    let fx = Fixture::new();
    write_rules(&fx, EPH_CFG);
    let mid = member(&fx, "mid/a", "mid", "main");
    member(&fx, "eph/1", "eph", "mid/a");
    install_hook(
        &fx,
        "post-destroy",
        &format!(
            "#!/bin/sh\ncd {} && printf x > s.txt && git add s.txt && git commit -q -m stray\n",
            mid.display()
        ),
    );
    let o = run_wt(&mid, &["destroy"]);
    assert_fail(&o);
    assert!(err(&o).contains("changed since"), "{}", err(&o));
    assert!(
        branches(&fx).contains(&"mid/a".to_string()),
        "the branch keeps the commit: {:?}",
        branches(&fx)
    );
}

/// The key confirms one exact state. A hook that writes between the key's
/// acceptance and the removal changes that state, so the force the key
/// earned no longer applies — to the hook's file or to anything else.
#[test]
fn a_confirmed_key_does_not_cover_what_a_hook_adds_after_it() {
    let fx = Fixture::new();
    write_rules(&fx, GROUP_CFG);
    let wt = member(&fx, "feature/a", "feat", "main");
    fs::write(wt.join("mine.txt"), "precious\n").unwrap();
    let o = run_wt(&wt, &["destroy"]);
    assert_fail(&o);
    let key = err(&o)
        .split("--key ")
        .nth(1)
        .expect("the refusal names the key")
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();
    install_hook(
        &fx,
        "pre-destroy",
        "#!/bin/sh\nprintf x > \"$WTREE_PATH/extra.tmp\"\n",
    );
    let o = run_wt(&wt, &["destroy", "--key", &key]);
    assert_fail(&o);
    assert!(err(&o).contains("changed since"), "{}", err(&o));
    assert!(
        wt.join("mine.txt").exists() && wt.join("extra.tmp").exists(),
        "both files survive:\n{}",
        err(&o)
    );
}

/// The seam checks look at every worktree the destroy half will touch, not
/// just the one being merged: a gate that dirties an ephemeral child also
/// stops land while the merge has not started.
#[test]
fn land_stops_before_the_merge_when_a_gate_dirties_a_child() {
    let fx = Fixture::new();
    write_rules(
        &fx,
        "[main]\nchildren = group:mid\nmerge-mode = squash\n\n\
         [group:mid]\nname-allow = mid/*\nchildren = group:eph\n\n\
         [group:eph]\nname-allow = eph/*\nephemeral = true\n",
    );
    let mid = member(&fx, "mid/a", "mid", "main");
    let kid = member(&fx, "eph/1", "eph", "mid/a");
    fx.commit(&mid, "one");
    let before = rev(&fx, "main");
    install_hook(
        &fx,
        "pre-destroy",
        "#!/bin/sh\n[ \"$WTREE_BRANCH\" = eph/1 ] || exit 0\nprintf x > \"$WTREE_PATH/kid.tmp\"\n",
    );
    let o = run_wt(&mid, &["land", "-m", "feat: a"]);
    assert_fail(&o);
    assert!(err(&o).starts_with("stopped:"), "{}", err(&o));
    assert!(err(&o).contains("kid.tmp"), "{}", err(&o));
    assert_eq!(rev(&fx, "main"), before, "the merge must not start");
    assert!(
        kid.join("kid.tmp").exists(),
        "the file survives for inspection"
    );
}

/// A hook sees only its own invocation's values: names another pair sets, or
/// a WTREE_* the calling shell happens to export (say, wtree run from inside
/// a hook), arrive empty rather than leaking through.
#[test]
fn a_hook_does_not_inherit_another_invocations_env() {
    let fx = Fixture::new();
    write_rules(&fx, GROUP_CFG);
    install_hook(
        &fx,
        "pre-destroy",
        &logging_hook("$WTREE_TARGET|$WTREE_MODE|$WTREE_TIP"),
    );
    let wt = member(&fx, "feature/a", "feat", "main");
    let o = Command::new(env!("CARGO_BIN_EXE_wtree"))
        .current_dir(&wt)
        .args(["destroy"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("WTREE_TARGET", "stale-target")
        .env("WTREE_MODE", "stale-mode")
        .env("WTREE_TIP", "stale-tip")
        .output()
        .expect("failed to spawn wtree");
    assert_ok(&o);
    assert_eq!(hook_log(&fx), vec!["||"]);
}

/// Only "not there" means "not installed": a hooks/ that answers anything
/// else to the lookup (here, a file where the directory should be) must not
/// wave the gate through as if no hook existed.
#[test]
fn an_unreadable_hooks_dir_refuses_instead_of_passing_silently() {
    let fx = Fixture::new();
    write_rules(&fx, GROUP_CFG);
    let dir = fx.repo.join(".git/wtree/hooks");
    let _ = fs::remove_dir_all(&dir);
    fs::write(&dir, "not a directory").unwrap();
    let o = run_wt(&fx.repo, &["new", "feature/a"]);
    assert_fail(&o);
    assert!(err(&o).contains("cannot read"), "{}", err(&o));
    assert!(!default_dest(&fx, "feature/a").exists());
}

/// The state land's destroy half sees after a file-writing hook, under the
/// verb it is made of: a bare destroy refuses it with a work-loss refusal.
#[test]
fn destroy_refuses_an_untracked_file_as_work_loss() {
    let fx = Fixture::new();
    write_rules(&fx, &merge_cfg("squash"));
    let wt = member(&fx, "feature/a", "feat", "main");
    fs::write(wt.join("artifact.txt"), "precious\n").unwrap();
    let o = run_wt(&wt, &["destroy"]);
    assert_fail(&o);
    assert!(err(&o).contains("work-loss risk"), "{}", err(&o));
}

/// The gates run before the merge, so a file a gate hook leaves behind stops
/// `land` while "nothing was changed" is still true: no merge, no stash, the
/// file itself kept for inspection — never swept into the squash commit or
/// force-deleted by the destroy half.
#[test]
fn land_stops_before_the_merge_when_a_gate_dirties_the_tree() {
    for hook in ["pre-merge", "pre-destroy"] {
        let fx = Fixture::new();
        write_rules(&fx, &merge_cfg("squash"));
        let wt = member(&fx, "feature/a", "feat", "main");
        fx.commit(&wt, "one");
        let before = rev(&fx, "main");
        install_hook(
            &fx,
            hook,
            "#!/bin/sh\nprintf 'precious\\n' > \"$WTREE_PATH/artifact.txt\"\n",
        );
        let o = run_wt(&wt, &["land", "-m", "feat: a"]);
        assert_fail(&o);
        assert!(err(&o).starts_with("stopped:"), "{hook}: {}", err(&o));
        assert!(err(&o).contains("artifact.txt"), "{hook}: {}", err(&o));
        assert!(err(&o).contains(hook), "{hook} not named:\n{}", err(&o));
        assert_eq!(rev(&fx, "main"), before, "{hook}: the merge must not start");
        assert_eq!(
            fs::read_to_string(wt.join("artifact.txt")).unwrap(),
            "precious\n",
            "{hook}: the file survives for inspection"
        );
    }
}

/// After the merge only `post-merge` has the pen. Its dirt stops the destroy
/// half: the merge stands (a report cannot un-merge), the worktree and the
/// file stay, and the message hands over to `wtree destroy`, whose
/// confirmation key is the mechanism for approving the discard.
#[test]
fn land_keeps_the_merge_but_stops_the_destroy_on_post_merge_dirt() {
    let fx = Fixture::new();
    write_rules(&fx, &merge_cfg("squash"));
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "one");
    install_hook(
        &fx,
        "post-merge",
        "#!/bin/sh\nprintf 'precious\\n' > \"$WTREE_PATH/artifact.txt\"\n",
    );
    let o = run_wt(&wt, &["land", "-m", "feat: a"]);
    assert_fail(&o);
    assert!(err(&o).starts_with("stopped:"), "{}", err(&o));
    assert!(err(&o).contains("artifact.txt"), "{}", err(&o));
    assert!(err(&o).contains("wtree destroy"), "{}", err(&o));
    assert_eq!(rev(&fx, "main"), rev(&fx, "feature/a"), "the merge stands");
    assert!(wt.is_dir(), "the worktree survives");
    assert_eq!(
        fs::read_to_string(wt.join("artifact.txt")).unwrap(),
        "precious\n"
    );
}

/// A pre-destroy veto under `land` aborts the whole verb before the merge —
/// the gate would be pointless where it can no longer stop anything.
#[test]
fn a_pre_destroy_veto_under_land_aborts_before_the_merge() {
    let fx = Fixture::new();
    write_rules(&fx, &merge_cfg("squash"));
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "one");
    let before = rev(&fx, "main");
    install_hook(&fx, "pre-destroy", "#!/bin/sh\nexit 1\n");
    let o = run_wt(&wt, &["land", "-m", "feat: a"]);
    assert_fail(&o);
    assert!(err(&o).contains("nothing was changed"), "{}", err(&o));
    assert_eq!(rev(&fx, "main"), before, "the merge must not have run");
    assert!(wt.is_dir(), "the worktree survives");
}

/// Only commits are merged, but the gate runs on the working tree: WTREE_DIRTY
/// is how a suite-running hook learns the two differ and declines to judge.
#[test]
fn wtree_dirty_tells_the_gate_about_uncommitted_changes() {
    let fx = Fixture::new();
    write_rules(&fx, &merge_cfg("squash"));
    let wt = member(&fx, "feature/a", "feat", "main");
    fx.commit(&wt, "one");
    install_hook(&fx, "pre-merge", &logging_hook("$WTREE_DIRTY"));
    assert_ok(&run_wt(&wt, &["merge", "-m", "feat: a"]));
    fx.commit(&wt, "two");
    fs::write(wt.join("wip.txt"), "wip\n").unwrap();
    assert_ok(&run_wt(&wt, &["merge", "-m", "feat: a"]));
    assert_eq!(hook_log(&fx), vec!["0", "1"]);
}

// ------------------------------------------------------------ rules gating ----

#[test]
fn verbs_require_init_and_valid_rules() {
    let fx = Fixture::new();
    // no rules yet
    for verb in ["list", "info", "rule", "new"] {
        let o = if verb == "new" {
            run_wt(&fx.repo, &["new", "feature/a"])
        } else {
            run_wt(&fx.repo, &[verb])
        };
        assert_fail(&o);
        assert!(
            err(&o).contains("run `wtree init` first"),
            "{verb}: {}",
            err(&o)
        );
    }
    // a rules with errors blocks execution, citing label:line. `rule` among
    // them: a policy that does not parse is not one to be read out as if the
    // verbs would go by it.
    write_rules(&fx, "[main]\nchildren = group:ghost\n");
    for verb in ["list", "rule"] {
        let o = run_wt(&fx.repo, &[verb]);
        assert_fail(&o);
        let stderr = err(&o);
        assert!(
            stderr.contains("undeclared group 'group:ghost'"),
            "{verb}: {stderr}"
        );
        assert!(stderr.contains("(.git/wtree/rules:2)"), "{verb}: {stderr}");
    }
}

#[test]
fn a_closed_reader_does_not_panic() {
    // Rust ignores SIGPIPE by default, which turns `wtree ... | head` into a
    // panic (exit 101) instead of the quiet death every unix tool performs.
    // Dropping the pipe before the verb reaches its first print reproduces it:
    // the child spends several git invocations gathering facts first.
    let fx = Fixture::new();
    write_rules(
        &fx,
        "[main]\nchildren = group:g\n\n[group:g]\nname-allow = feature/*\n",
    );
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
    assert_ne!(
        status.code(),
        Some(101),
        "printing to a closed pipe panicked"
    );
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
    write_rules(
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
        assert!(
            !menu.contains(&format!("  {hidden}")),
            "'{hidden}' in:\n{menu}"
        );
    }
    // list/info are unconditional, and the hint names the way to the rest
    assert!(menu.contains("  list "), "{menu}");
    assert!(menu.contains("  info "), "{menu}");
    assert!(menu.contains("wtree -h"), "{menu}");
    assert_hidden_verbs_refuse(&fx.repo, &menu);

    // A group member: the leaf where the work happens, so nearly everything is.
    assert_ok(&run_wt(&fx.repo, &["new", "feature/a"]));
    let wt = default_dest(&fx, "feature/a");
    let menu = out(&run_wt(&wt, &[]));
    assert!(menu.starts_with("feature/a (group:feat)"), "{menu}");
    for shown in ["merge", "sync", "land", "close", "destroy", "adopt"] {
        assert!(
            menu.contains(&format!("  {shown}")),
            "'{shown}' missing:\n{menu}"
        );
    }
    // [group:feat] declares no children, so nothing forks from here
    assert!(!menu.contains("new <name>"), "{menu}");
    assert_hidden_verbs_refuse(&wt, &menu);
}

#[test]
fn description_says_what_the_current_branch_is_for() {
    let fx = Fixture::new();
    write_rules(
        &fx,
        "[main]\nchildren = group:feat\ndestroyable = false\ndescription = release line\n\n\
         [group:feat]\nname-allow = feature/*\ndescription = one feature each, land into main\n",
    );

    // Menu: under the head line, above the verbs.
    let menu = out(&run_wt(&fx.repo, &[]));
    assert!(menu.starts_with("main (fixed)\n  release line\n"), "{menu}");
    assert!(out(&run_wt(&fx.repo, &["info"])).contains("description: release line"));

    // Standing in a group member, the group's description is the one that
    // applies — main's does not follow the merge target into view.
    assert_ok(&run_wt(&fx.repo, &["new", "feature/a"]));
    let wt = default_dest(&fx, "feature/a");
    let menu = out(&run_wt(&wt, &[]));
    assert!(
        menu.starts_with("feature/a (group:feat)\n  one feature each, land into main\n"),
        "{menu}"
    );
    assert!(!menu.contains("release line"), "{menu}");
    assert!(out(&run_wt(&wt, &["info"])).contains("description: one feature each, land into main"));
}

#[test]
fn description_absent_prints_no_line() {
    let fx = Fixture::new();
    write_rules(
        &fx,
        "[main]\nchildren = group:feat\n\n[group:feat]\nname-allow = feature/*\n",
    );
    let menu = out(&run_wt(&fx.repo, &[]));
    assert!(
        menu.starts_with("main (fixed)\n\n"),
        "no blank description line:\n{menu}"
    );
    assert!(
        !out(&run_wt(&fx.repo, &["info"])).contains("description:"),
        "{menu}"
    );
}

#[test]
fn the_menu_spells_merge_modes_but_never_key_or_force() {
    let fx = Fixture::new();
    // main takes one mode, so merge names it; the group takes two, so the
    // flag becomes a choice the menu has to show.
    write_rules(
        &fx,
        "[main]\nchildren = group:feat\nmerge-mode = ff\n\n\
         [group:feat]\nchildren = group:sub\nname-allow = feature/*\nmerge-mode = squash, no-ff\n\n\
         [group:sub]\nname-allow = sub/*\n",
    );
    assert_ok(&run_wt(&fx.repo, &["new", "feature/a"]));
    let parent = default_dest(&fx, "feature/a");
    assert_ok(&run_wt(&parent, &["new", "sub/x"]));
    let child = default_dest(&fx, "sub/x");

    assert!(
        out(&run_wt(&parent, &[])).contains("merge --ff"),
        "one mode is named"
    );
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
fn merge_mode_none_refuses_and_stays_out_of_the_menu() {
    let fx = Fixture::new();
    write_rules(
        &fx,
        "[main]\nchildren = group:feat\nmerge-mode = none\n\n\
         [group:feat]\nname-allow = feature/*\n",
    );
    assert_ok(&run_wt(&fx.repo, &["new", "feature/a"]));
    let wt = default_dest(&fx, "feature/a");
    fx.commit(&wt, "work");

    let e = err(&run_wt(&wt, &["merge", "-m", "up"]));
    assert!(e.contains("'main': accepts no merges"), "{e}");
    assert!(e.contains("rule: merge-mode = none"), "{e}");
    let e = err(&run_wt(&wt, &["land", "-m", "up"]));
    assert!(e.contains("'main': accepts no merges"), "{e}");

    // the menu offers neither merge nor land, but sync stays — merge-mode
    // rules what main takes in, not what this branch pulls down
    let menu = out(&run_wt(&wt, &[]));
    assert!(
        !menu
            .lines()
            .any(|l| l.trim_start().starts_with("merge") || l.trim_start().starts_with("land")),
        "{menu}"
    );
    assert!(menu.contains("sync"), "{menu}");

    let info = out(&run_wt(&wt, &["info"]));
    assert!(
        info.contains("merge to 'main': none — accepts no merges"),
        "{info}"
    );
}

#[test]
fn an_unmanaged_worktree_offers_only_the_way_back() {
    let fx = Fixture::new();
    write_rules(&fx, GROUP_CFG);
    fx.git(&fx.repo, &["branch", "stray", "main"]);
    assert_ok(&run_wt(&fx.repo, &["open", "stray"]));
    let wt = default_dest(&fx, "stray");
    let menu = out(&run_wt(&wt, &[]));
    assert!(menu.starts_with("stray (unmanaged)"), "{menu}");
    assert!(
        menu.contains("adopt (--group G | --free) --parent P"),
        "{menu}"
    );
    assert_hidden_verbs_refuse(&wt, &menu);
}

#[test]
fn open_and_new_answer_a_missing_argument_with_what_they_accept() {
    let fx = Fixture::new();
    write_rules(
        &fx,
        "[main]\nchildren = group:feat, group:any, dev\n\n\
         [group:feat]\nname-allow = feature/*\nname-deny = feature/tmp-*\n\n[group:any]\n\n[dev]\n",
    );
    fx.git(&fx.repo, &["branch", "loose", "main"]);

    let o = run_wt(&fx.repo, &["new"]);
    assert_eq!(
        o.status.code(),
        Some(2),
        "a missing name is still a usage error"
    );
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
    assert!(
        e.contains("loose") && e.contains("unmanaged until adopted"),
        "{e}"
    );
    assert!(!e.contains("\n  main"), "main is checked out here: {e}");

    // Everything opened: the screen says so rather than showing an empty list.
    assert_ok(&run_wt(&fx.repo, &["open", "loose"]));
    let e = err(&run_wt(&fx.repo, &["open"]));
    assert!(e.contains("no branch is waiting for a worktree"), "{e}");
}

#[test]
fn the_manual_needs_neither_a_repo_nor_a_readable_rules() {
    let fx = Fixture::new();
    // Broken beyond loading: the moment a user most needs to look a verb up.
    write_rules(&fx, "[main]\nchildren = group:ghost\nbogus-key = 1\n");
    let o = run_wt(&fx.repo, &["help", "--all"]);
    assert_ok(&o);
    let stdout = out(&o);
    for verb in [
        "new", "open", "close", "merge", "sync", "land", "destroy", "adopt", "init",
    ] {
        assert!(
            stdout.contains(&format!("wtree {verb}")),
            "'{verb}' missing:\n{stdout}"
        );
    }
    // ... and outside a git repo entirely
    let o = run_wt(&fx.tmp.0, &["help", "--all"]);
    assert_ok(&o);
    assert!(out(&o).contains("wtree merge"));

    // The contextual menu cannot be built from that rules, and says so.
    assert_fail(&run_wt(&fx.repo, &[]));
}

/// The briefing is static and reads nothing, so it answers anywhere — even
/// outside a repo. Both entry screens point at it, since an agent arrives
/// through one of them.
#[test]
fn llm_prints_the_briefing_anywhere_and_both_screens_point_at_it() {
    let fx = Fixture::new();
    let o = run_wt(&fx.tmp.0, &["llm"]);
    assert_ok(&o);
    let text = out(&o);
    assert!(text.contains("a briefing for coding agents"), "{text}");
    assert!(text.contains("wtree info"), "{text}");

    // The one topic: the rules-file reference, pointing back at `wtree rule`.
    let o = run_wt(&fx.tmp.0, &["llm", "rule"]);
    assert_ok(&o);
    let text = out(&o);
    assert!(text.contains("the rules file reference"), "{text}");
    assert!(text.contains("merge-mode"), "{text}");

    // A wrong topic is answered with the registry, not a silent briefing.
    let o = run_wt(&fx.tmp.0, &["llm", "extra"]);
    assert_eq!(o.status.code(), Some(2), "{}", err(&o));
    assert!(err(&o).contains("wtree llm [rule]"), "{}", err(&o));
    let o = run_wt(&fx.tmp.0, &["llm", "rule", "extra"]);
    assert_eq!(o.status.code(), Some(2), "{}", err(&o));

    // ... and past a rules file that cannot load, like the manual.
    write_rules(&fx, "[main]\nchildren = group:ghost\nbogus-key = 1\n");
    assert_ok(&run_wt(&fx.repo, &["llm"]));
    assert_ok(&run_wt(&fx.repo, &["llm", "rule"]));

    write_rules(&fx, GROUP_CFG);
    let menu = out(&run_wt(&fx.repo, &[]));
    assert!(menu.contains("wtree llm"), "{menu}");
    let manual = out(&run_wt(&fx.repo, &["-h"]));
    assert!(manual.contains("wtree llm"), "{manual}");
}

/// `--help` used to be read as an argument: verbs that parse their flags called
/// it unknown, and the ones that ignore theirs (`list`, `info`) just ran. Both
/// are wrong — a user reaching for `--help` gets help, and nothing else happens.
#[test]
fn help_anywhere_on_the_line_beats_the_verb() {
    let fx = Fixture::new();

    // The verb that would have written files does not write them.
    let o = run_wt(&fx.repo, &["init", "--help"]);
    assert_ok(&o);
    assert!(out(&o).contains("wtree init [--new"), "{}", out(&o));
    assert!(
        !fx.repo.join(".git/wtree").exists(),
        "init --help must not init"
    );

    // ... including behind flags the verb does accept, and on a verb that
    // ignores its arguments entirely.
    assert_ok(&run_wt(&fx.repo, &["init", "--new", "--force", "--help"]));
    assert!(
        !fx.repo.join(".git/wtree").exists(),
        "still nothing written"
    );
    let o = run_wt(&fx.repo, &["list", "--help"]);
    assert_ok(&o);
    assert!(out(&o).contains("worktrees in this repo"), "{}", out(&o));

    // No row of its own falls back to the manual rather than to nothing: a
    // typo'd verb, and `--help` with no verb at all.
    for a in [vec!["bogus", "--help"], vec!["--help"]] {
        let o = run_wt(&fx.repo, &a);
        assert_ok(&o);
        assert!(
            out(&o).contains("usage: wtree <verb>"),
            "{a:?}: {}",
            out(&o)
        );
    }

    // `-h` is the same rule, not a second one: same reach, same answer.
    for (short, long) in [
        (vec!["-h"], vec!["--help"]),
        (vec!["list", "-h"], vec!["list", "--help"]),
        (vec!["init", "--new", "-h"], vec!["init", "--new", "--help"]),
    ] {
        let s = run_wt(&fx.repo, &short);
        assert_ok(&s);
        assert_eq!(out(&s), out(&run_wt(&fx.repo, &long)), "{short:?}");
    }
    assert!(!fx.repo.join(".git/wtree").exists(), "-h must not init");

    // Broken rules are when a verb's usage is most needed, so it reads none.
    write_rules(&fx, "[main]\nbogus-key = 1\n");
    assert_fail(&run_wt(&fx.repo, &[]));
    let o = run_wt(&fx.repo, &["merge", "--help"]);
    assert_ok(&o);
    assert!(out(&o).contains("merge into the parent"), "{}", out(&o));
}

/// `help` answers but is never offered: it is the word a lost user reaches for
/// first, and refusing it to teach a shorter spelling costs them a try. Nothing
/// on screen names it, so each of the two screens has one advertised way in.
#[test]
fn the_help_verb_answers_without_being_advertised() {
    let fx = Fixture::new();
    write_rules(&fx, "[main]\nchildren = group:feat\n\n[group:feat]\n");

    assert_eq!(
        out(&run_wt(&fx.repo, &["help"])),
        out(&run_wt(&fx.repo, &[]))
    );
    assert_eq!(
        out(&run_wt(&fx.repo, &["help", "--all"])),
        out(&run_wt(&fx.repo, &["-h"]))
    );

    for a in [vec![], vec!["-h"], vec!["bogus"]] {
        let o = run_wt(&fx.repo, &a);
        let all = format!("{}{}", out(&o), String::from_utf8_lossy(&o.stderr));
        assert!(!all.contains("wtree help"), "{a:?} points at it:\n{all}");
    }
}

/// The command is `wtree` and the crate is `gitwtree`, so the version line names
/// both: what you typed, and what you would reinstall.
#[test]
fn version_prints_the_command_and_the_crate() {
    let fx = Fixture::new();
    let expected = format!("wtree {} (gitwtree)", env!("CARGO_PKG_VERSION"));
    for a in [vec!["-v"], vec!["--version"]] {
        let o = run_wt(&fx.repo, &a);
        assert_ok(&o);
        assert_eq!(out(&o).trim(), expected, "{a:?}");
    }
    // It reads no repo: outside one is exactly where you check what you installed.
    let o = run_wt(fx.repo.parent().unwrap(), &["--version"]);
    assert_ok(&o);
    assert_eq!(out(&o).trim(), expected);
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
    assert!(err(&o).starts_with("error:"), "{}", err(&o));
}

/// Three words carry the diagnostics — `refusal:` for what the policy
/// declines, `stopped:` for a verb that halted between its steps with part of
/// its work standing, and `error:` for everything else that went wrong — and
/// the exit code says separately whether the line or the run was at fault.
#[test]
fn every_diagnostic_wears_one_of_the_three_words() {
    let fx = Fixture::new();
    write_rules(&fx, GROUP_CFG);

    let o = run_wt(&fx.repo, &["frobnicate"]);
    assert_eq!(o.status.code(), Some(2), "stderr:\n{}", err(&o));
    assert!(err(&o).starts_with("error: unknown verb"), "{}", err(&o));
    // The two lines under it are the menu, not diagnostics, and stay bare.
    assert!(err(&o).contains("\nwtree      verbs"), "{}", err(&o));

    // An unknown flag is the same mistake one word further in, and always said
    // so; the verb slot now matches it.
    let o = run_wt(&fx.repo, &["new", "--frobnicate"]);
    assert!(err(&o).starts_with("error: unknown flag"), "{}", err(&o));

    // A layer under the verbs raises a bare sentence; main is the last hand on
    // it, so the word is added there rather than at each of those layers.
    let o = run_wt(&fx.tmp.0, &["list"]);
    assert!(err(&o).starts_with("error:"), "{}", err(&o));

    // A refusal already carries its word and must not collect a second.
    let o = run_wt(&fx.repo, &["new", "junk/x"]);
    assert!(err(&o).starts_with("refusal: new"), "{}", err(&o));
}

/// The two failures every reader meets — no repository, no rules — are wtree's
/// to answer. Which plumbing command asked, and where the file would have
/// sat, are answers to a question nobody standing there has yet.
#[test]
fn the_expected_failures_are_answered_without_git_or_paths() {
    let fx = Fixture::new();

    let o = run_wt(&fx.tmp.0, &["list"]);
    assert_fail(&o);
    assert_eq!(err(&o).trim(), "error: not inside a git repository");

    // In a repository, with no rules written yet.
    let o = run_wt(&fx.repo, &["list"]);
    assert_fail(&o);
    assert_eq!(
        err(&o).trim(),
        "error: no policy rules in this repository — run `wtree init` first"
    );
    assert_ok(&run_wt(&fx.repo, &["init", "--new"]));
    assert_ok(&run_wt(&fx.repo, &["list"]));
}

#[test]
fn an_uninitialized_repo_is_pointed_at_init() {
    let fx = Fixture::new();
    let o = run_wt(&fx.repo, &[]);
    assert_ok(&o);
    let stdout = out(&o);
    assert!(stdout.contains("no wtree policy yet"), "{stdout}");
    assert!(stdout.contains("init"), "{stdout}");
    // The briefing explains setup, so this screen must point at it too.
    assert!(stdout.contains("wtree llm"), "{stdout}");
}
