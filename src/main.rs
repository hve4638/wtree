use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use wtree::verbs::{INIT_USAGE, NEW_USAGE, OPEN_USAGE, SAVE_USAGE};
use wtree::{prompt, rules, verbs};

/// Rust's runtime sets SIGPIPE to SIG_IGN before main, so a closed reader turns
/// every later `println!` into a panic (`head`, `less`, `grep -q` all do this).
/// Restoring the default makes the process die quietly like any other unix tool.
fn restore_sigpipe() {
    // SAFETY: `SIG_DFL` is a valid handler for `SIGPIPE`, and this runs once at
    // startup, before anything else can be looking at the disposition.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
}

fn main() -> ExitCode {
    restore_sigpipe();
    // `env::args` panics on a non-UTF-8 argument, and a git refname is only a
    // byte string — `wtree open $'\xff'` is reachable. Every name here is a
    // `String` down to the state file, so refuse it rather than act on a
    // lossily converted branch the user did not name.
    let args: Vec<String> = match env::args_os().skip(1).map(OsString::into_string).collect() {
        Ok(a) => a,
        Err(_) => {
            eprintln!("wtree: arguments must be valid UTF-8");
            return ExitCode::from(2);
        }
    };
    let cwd: PathBuf = match env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("wtree: cannot determine the current directory: {e}");
            return ExitCode::from(1);
        }
    };
    // No verb is a question, not a mistake — answer it with the menu on stdout.
    // `rest` rather than `args[1..]`, which panics on the bare invocation.
    let verb = args.first().map(String::as_str).unwrap_or("help");
    let rest: &[String] = args.get(1..).unwrap_or(&[]);
    // `--help` outranks everything else on the line, wherever it sits. Asking a
    // verb what it does and having it do the thing instead is the one answer
    // that cannot be taken back, and no verb here wants `--help` as a value.
    if args.iter().any(|a| a == "--help") {
        help_for(verb);
        return ExitCode::SUCCESS;
    }
    let result = match verb {
        "help" => {
            if rest.iter().any(|a| a == "--all") {
                manual();
                return ExitCode::SUCCESS;
            }
            verbs::help(&cwd)
        }
        "check" => return cmd_check(args.get(1).map(String::as_str)),
        "init" => match parse_init_args(rest) {
            Ok((mode, force)) => verbs::init(&cwd, mode, force),
            Err(msg) => {
                eprintln!("{msg}");
                return ExitCode::from(2);
            }
        },
        "save" => match parse_save_args(rest) {
            Ok((dest, force)) => verbs::save(&cwd, dest.as_deref(), force),
            Err(msg) => {
                eprintln!("{msg}");
                return ExitCode::from(2);
            }
        },
        "new" => match parse_new_args(rest) {
            Ok((name, group)) => verbs::new(&cwd, &name, group.as_deref()),
            Err(ArgErr::Missing) => {
                verbs::usage_new(&cwd);
                return ExitCode::from(2);
            }
            Err(ArgErr::Bad(msg)) => {
                eprintln!("{msg}");
                return ExitCode::from(2);
            }
        },
        "open" => match parse_open_args(rest) {
            Ok(branch) => verbs::open(&cwd, &branch),
            Err(ArgErr::Missing) => {
                verbs::usage_open(&cwd);
                return ExitCode::from(2);
            }
            Err(ArgErr::Bad(msg)) => {
                eprintln!("{msg}");
                return ExitCode::from(2);
            }
        },
        "close" => match parse_close_args(rest) {
            Ok(key) => verbs::close(&cwd, key.as_deref()),
            Err(msg) => {
                eprintln!("{msg}");
                return ExitCode::from(2);
            }
        },
        "merge" => match parse_merge_args("merge", rest) {
            Ok((mode, msg)) => verbs::merge(&cwd, mode, msg.as_deref()),
            Err(msg) => {
                eprintln!("{msg}");
                return ExitCode::from(2);
            }
        },
        // land inherits merge's mode flag and -m: it runs a merge, and the
        // policy that decides how a merge lands does not change because a
        // destroy follows it.
        "land" => match parse_merge_args("land", rest) {
            Ok((mode, msg)) => verbs::land(&cwd, mode, msg.as_deref()),
            Err(msg) => {
                eprintln!("{msg}");
                return ExitCode::from(2);
            }
        },
        "destroy" => match parse_destroy_args(rest) {
            Ok((force, key)) => verbs::destroy(&cwd, force, key.as_deref()),
            Err(msg) => {
                eprintln!("{msg}");
                return ExitCode::from(2);
            }
        },
        "sync" => {
            if args.len() > 1 {
                eprintln!("wtree sync: takes no arguments");
                return ExitCode::from(2);
            }
            verbs::sync(&cwd)
        }
        "adopt" => match parse_adopt_args(rest) {
            Ok((group, free, parent)) => verbs::adopt(&cwd, group.as_deref(), free, &parent),
            Err(msg) => {
                eprintln!("{msg}");
                return ExitCode::from(2);
            }
        },
        "list" => verbs::list(&cwd),
        "info" => verbs::info(&cwd),
        v => {
            eprintln!("wtree: unknown verb '{v}'");
            eprintln!("wtree           verbs available where you are");
            eprintln!("wtree help --all   every verb");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("{}", msg.trim_end());
            ExitCode::from(1)
        }
    }
}

fn parse_new_args(rest: &[String]) -> Result<(String, Option<String>), ArgErr> {
    let mut name = None;
    let mut group = None;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--group" => match it.next() {
                Some(_) if group.is_some() => {
                    return Err(ArgErr::Bad("wtree new: --group given twice".into()));
                }
                Some(g) => group = Some(g.clone()),
                None => return Err(ArgErr::Bad("wtree new: --group requires a value".into())),
            },
            s if s.starts_with('-') => {
                return Err(ArgErr::Bad(format!("wtree new: unknown flag '{s}'")));
            }
            s if name.is_some() => {
                return Err(ArgErr::Bad(format!(
                    "wtree new: unexpected extra argument '{s}'"
                )));
            }
            s => name = Some(s.to_string()),
        }
    }
    match name {
        Some(n) => Ok((n, group)),
        None => Err(ArgErr::Missing),
    }
}

/// A missing argument is not the same kind of mistake as a wrong one: `new` and
/// `open` answer it by showing what they would have accepted, which needs the
/// repo. Both still exit 2 — nothing was created either way.
#[derive(Debug)]
enum ArgErr {
    Missing,
    Bad(String),
}

/// One branch name, no flags: open takes its target as an argument rather than
/// from cwd, because the branch it attaches a worktree to has none.
fn parse_open_args(rest: &[String]) -> Result<String, ArgErr> {
    let mut branch: Option<String> = None;
    for a in rest {
        match a.as_str() {
            s if s.starts_with('-') => {
                return Err(ArgErr::Bad(format!(
                    "wtree open: unknown flag '{s}'\n{OPEN_USAGE}"
                )));
            }
            s if branch.is_some() => {
                return Err(ArgErr::Bad(format!(
                    "wtree open: unexpected extra argument '{s}'\n{OPEN_USAGE}"
                )));
            }
            s => branch = Some(s.to_string()),
        }
    }
    branch.ok_or(ArgErr::Missing)
}

const CLOSE_USAGE: &str = "usage: wtree close [--key <key>]";

/// `--key` as in destroy, and for the same reason: the refusal that issues one
/// spells the re-run, so a bare word is a typo rather than a key. There is no
/// `--force` — close has no relation layer for one to pass.
fn parse_close_args(rest: &[String]) -> Result<Option<String>, String> {
    let mut key: Option<String> = None;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--key" => match it.next() {
                Some(_) if key.is_some() => return Err("wtree close: --key given twice".into()),
                Some(k) => key = Some(k.clone()),
                None => return Err("wtree close: --key requires a value".into()),
            },
            s => {
                return Err(format!(
                    "wtree close: unknown argument '{s}'\n{CLOSE_USAGE}"
                ));
            }
        }
    }
    Ok(key)
}

const ADOPT_USAGE: &str = "usage: wtree adopt (--group G | --free) --parent P";

/// Exactly one of --group/--free, plus a mandatory --parent. The exclusivity
/// is decided here rather than in the judge so a misspelled invocation fails
/// as a usage error (exit 2), before any repo or rules are touched.
fn parse_adopt_args(rest: &[String]) -> Result<(Option<String>, bool, String), String> {
    let mut group: Option<String> = None;
    let mut free = false;
    let mut parent: Option<String> = None;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--group" => match it.next() {
                Some(_) if group.is_some() => return Err("wtree adopt: --group given twice".into()),
                Some(g) => group = Some(g.clone()),
                None => return Err("wtree adopt: --group requires a value".into()),
            },
            "--free" => free = true,
            "--parent" => match it.next() {
                Some(_) if parent.is_some() => {
                    return Err("wtree adopt: --parent given twice".into());
                }
                Some(p) => parent = Some(p.clone()),
                None => return Err("wtree adopt: --parent requires a value".into()),
            },
            s => {
                return Err(format!(
                    "wtree adopt: unknown argument '{s}'\n{ADOPT_USAGE}"
                ));
            }
        }
    }
    match (&group, free) {
        (Some(_), true) => {
            return Err(format!(
                "wtree adopt: --group and --free are mutually exclusive\n{ADOPT_USAGE}"
            ));
        }
        (None, false) => {
            return Err(format!(
                "wtree adopt: one of --group <X> or --free is required\n{ADOPT_USAGE}"
            ));
        }
        _ => {}
    }
    match parent {
        Some(p) => Ok((group, free, p)),
        None => Err(format!(
            "wtree adopt: --parent <branch> is required\n{ADOPT_USAGE}"
        )),
    }
}

/// Shared by `merge` and `land`; `verb` only names the one that was typed.
fn parse_merge_args(
    verb: &str,
    rest: &[String],
) -> Result<(Option<rules::MergeMode>, Option<String>), String> {
    let mut mode = None;
    let mut msg: Option<String> = None;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--squash" | "--rebase" | "--no-ff" | "--ff" => {
                let m = rules::MergeMode::parse(&a[2..]).expect("flag names mirror mode names");
                if mode.is_some() {
                    return Err(format!(
                        "wtree {verb}: pass exactly one of --squash | --rebase | --no-ff | --ff"
                    ));
                }
                mode = Some(m);
            }
            "-m" => {
                let Some(v) = it.next() else {
                    return Err(format!("wtree {verb}: -m needs a message"));
                };
                if msg.is_some() {
                    return Err(format!("wtree {verb}: -m given twice"));
                }
                // A separated value that looks like a flag is a typo, not a
                // message; -m<text> is the way to spell a dash-leading one
                // (no flag contains whitespace, so a dash-leading phrase is
                // plainly the value).
                if v.starts_with('-') && !v.contains(char::is_whitespace) {
                    return Err(format!(
                        "wtree {verb}: -m takes a value, but '{v}' reads as a flag; write it as -m{v} if it is really the message"
                    ));
                }
                msg = Some(v.clone());
            }
            s if s.starts_with("-m") => {
                if msg.is_some() {
                    return Err(format!("wtree {verb}: -m given twice"));
                }
                msg = Some(s[2..].to_string());
            }
            s => return Err(format!("wtree {verb}: unknown argument '{s}'")),
        }
    }
    Ok((mode, msg))
}

const DESTROY_USAGE: &str = "usage: wtree destroy [--force] [--key <key>]";

/// `--key` rather than wtree.sh's positional key: the refusal that issues a key
/// spells the re-run as `wtree destroy --key <key>`, and that message is the
/// only place a caller ever reads one from.
fn parse_destroy_args(rest: &[String]) -> Result<(bool, Option<String>), String> {
    let mut force = false;
    let mut key: Option<String> = None;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--force" => force = true,
            "--key" => match it.next() {
                Some(_) if key.is_some() => return Err("wtree destroy: --key given twice".into()),
                Some(k) => key = Some(k.clone()),
                None => return Err("wtree destroy: --key requires a value".into()),
            },
            s => {
                return Err(format!(
                    "wtree destroy: unknown argument '{s}'\n{DESTROY_USAGE}"
                ));
            }
        }
    }
    Ok((force, key))
}

/// `--new` and `--load` name the two things `init` can do, and one of them has
/// to be picked: guessing from whether a `.wtree/` happens to exist is how a
/// team policy gets silently replaced by a template. Neither flag means "ask",
/// which only works on a terminal.
fn parse_init_args(rest: &[String]) -> Result<(verbs::InitMode, bool), String> {
    let mut mode: Option<verbs::InitMode> = None;
    let mut force = false;
    let mut it = rest.iter().peekable();
    while let Some(a) = it.next() {
        let picked = match a.as_str() {
            "--new" => verbs::InitMode::New,
            "--load" => {
                // The path is optional, so a following `--force` is the next
                // flag rather than a directory called "--force".
                let p = it.next_if(|v| !v.starts_with("--"));
                verbs::InitMode::Load(p.map(PathBuf::from))
            }
            "--force" => {
                force = true;
                continue;
            }
            s => return Err(format!("wtree init: unknown argument '{s}'\n{INIT_USAGE}")),
        };
        if mode.is_some() {
            return Err(format!(
                "wtree init: --new and --load are mutually exclusive\n{INIT_USAGE}"
            ));
        }
        mode = Some(picked);
    }
    match mode {
        Some(m) => Ok((m, force)),
        // Nothing to force: the interactive path asks about overwriting to your
        // face, and there is no template-versus-load decision to pre-answer.
        None if force => Err(format!(
            "wtree init: --force only applies to --new or --load\n{INIT_USAGE}"
        )),
        // Refused here rather than deeper in: there is nothing to ask on, and a
        // missing flag is a usage error (exit 2) whether or not this is a repo.
        None if !prompt::available() => Err(format!(
            "wtree init: no terminal to ask on — say which source to use\n  \
             wtree init --new\n  wtree init --load [path]\n{INIT_USAGE}"
        )),
        None => Ok((verbs::InitMode::Ask, false)),
    }
}

fn parse_save_args(rest: &[String]) -> Result<(Option<PathBuf>, bool), String> {
    let mut dest: Option<PathBuf> = None;
    let mut force = false;
    for a in rest {
        match a.as_str() {
            "--force" => force = true,
            s if s.starts_with("--") => {
                return Err(format!("wtree save: unknown flag '{s}'\n{SAVE_USAGE}"));
            }
            s if dest.is_some() => {
                return Err(format!("wtree save: '{s}' is a second path\n{SAVE_USAGE}"));
            }
            s => dest = Some(PathBuf::from(s)),
        }
    }
    Ok((dest, force))
}

/// Every verb, in the order the manual prints them. `--help` for a single verb
/// reads one row out of the same table, so the two can never disagree.
const ROWS: &[(&str, &str, &str)] = &[
    ("new", NEW_USAGE, "create a branch and its worktree"),
    ("open", OPEN_USAGE, "give an existing branch a worktree"),
    ("close", CLOSE_USAGE, "remove a worktree, keep the branch"),
    (
        "merge",
        "usage: wtree merge [--squash|--rebase|--no-ff|--ff] [-m <msg>]",
        "merge into the parent",
    ),
    (
        "sync",
        "usage: wtree sync",
        "merge the parent into this branch",
    ),
    (
        "land",
        "usage: wtree land [--squash|--rebase|--no-ff|--ff] [-m <msg>]",
        "merge, then destroy",
    ),
    ("destroy", DESTROY_USAGE, "delete a branch and its worktree"),
    (
        "adopt",
        ADOPT_USAGE,
        "record what a branch is, and whose child",
    ),
    ("list", "usage: wtree list", "worktrees in this repo"),
    (
        "info",
        "usage: wtree info",
        "rules and previews for one worktree",
    ),
    (
        "init",
        INIT_USAGE,
        "write starter rules, or load them from a .wtree/ (asks when given neither)",
    ),
    (
        "save",
        SAVE_USAGE,
        "copy the rules out to a .wtree/ you can commit",
    ),
    (
        "check",
        "usage: wtree check <rules-path>",
        "parse and validate a rules file (dev-only)",
    ),
];

/// `wtree <verb> --help`. Whatever has no row of its own — `help`, a misspelled
/// verb, or `wtree --help` with no verb at all — gets the whole manual, which
/// is never a worse answer than the one line it was reaching for.
fn help_for(verb: &str) {
    match ROWS.iter().find(|(v, ..)| *v == verb) {
        Some((_, usage, note)) => println!("{usage}\n    {note}"),
        None => manual(),
    }
}

/// `wtree help --all` — every verb, judged against nothing. It reads no repo
/// and no rules on purpose: broken rules are when a user most needs to look
/// something up, and that is exactly when the contextual menu cannot be built.
fn manual() {
    println!("usage: wtree <verb> [args]\n");
    for (_, usage, note) in ROWS {
        println!("  {}\n      {note}", usage.trim_start_matches("usage: "));
    }
    println!("\n-m is required for --squash and --no-ff, and rejected for --rebase and --ff.");
    println!("--key and --force appear only when a verb asks for them.");
    println!("\nwtree (no verb) lists just the verbs available where you are.");
    println!("wtree <verb> --help prints that verb's line on its own.");
}

fn cmd_check(path: Option<&str>) -> ExitCode {
    let Some(path) = path else {
        eprintln!("usage: wtree check <rules-path>");
        return ExitCode::from(2);
    };
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("wtree check: cannot read '{path}': {e}");
            return ExitCode::from(2);
        }
    };
    let loaded = rules::load_str(&text, path);
    for w in &loaded.warnings {
        println!("warning: {w}");
    }
    for e in &loaded.errors {
        println!("error: {e}");
    }
    if loaded.errors.is_empty() {
        println!(
            "ok: {} section(s), {} warning(s)",
            loaded.rules.sections.len(),
            loaded.warnings.len()
        );
        ExitCode::SUCCESS
    } else {
        println!(
            "failed: {} error(s), {} warning(s)",
            loaded.errors.len(),
            loaded.warnings.len()
        );
        ExitCode::from(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wtree::rules::MergeMode;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    /// `--load` takes an optional path, so the parser has to tell a directory
    /// from the next flag. Everything else it accepts is exclusive or refused.
    #[test]
    fn init_args_read_the_optional_load_path() {
        let mode = |a: &[&str]| match parse_init_args(&args(a)) {
            Ok((verbs::InitMode::New, f)) => format!("new force={f}"),
            Ok((verbs::InitMode::Load(p), f)) => format!("load {p:?} force={f}"),
            Ok((verbs::InitMode::Ask, f)) => format!("ask force={f}"),
            Err(e) => format!("err: {}", e.lines().next().unwrap()),
        };
        assert_eq!(mode(&["--new"]), "new force=false");
        assert_eq!(mode(&["--new", "--force"]), "new force=true");
        assert_eq!(mode(&["--load"]), "load None force=false");
        assert_eq!(
            mode(&["--load", ".wtree.strategy-1"]),
            "load Some(\".wtree.strategy-1\") force=false"
        );
        // the flag after a bare --load is a flag, not a directory called --force
        assert_eq!(mode(&["--load", "--force"]), "load None force=true");
        assert_eq!(
            mode(&["--force", "--load", "x"]),
            "load Some(\"x\") force=true"
        );

        assert!(mode(&["--new", "--load"]).contains("mutually exclusive"));
        assert!(mode(&["--load", "a", "--load", "b"]).contains("mutually exclusive"));
        assert!(mode(&["--force"]).contains("--force only applies"));
        assert!(mode(&["--nope"]).contains("unknown argument"));
    }

    #[test]
    fn save_args_take_one_path_at_most() {
        assert_eq!(parse_save_args(&args(&[])).unwrap(), (None, false));
        assert_eq!(
            parse_save_args(&args(&[".wtree.b", "--force"])).unwrap(),
            (Some(PathBuf::from(".wtree.b")), true)
        );
        for (a, needle) in [
            (vec!["a", "b"], "second path"),
            (vec!["--nope"], "unknown flag"),
        ] {
            let e = parse_save_args(&args(&a)).unwrap_err();
            assert!(e.contains(needle), "{a:?}: {e}");
        }
    }

    #[test]
    fn adopt_args_require_exactly_one_identity_and_a_parent() {
        assert_eq!(
            parse_adopt_args(&args(&["--group", "feat", "--parent", "main"])).unwrap(),
            (Some("feat".into()), false, "main".into())
        );
        assert_eq!(
            parse_adopt_args(&args(&["--free", "--parent", "main"])).unwrap(),
            (None, true, "main".into())
        );
        for (a, needle) in [
            (
                vec!["--group", "g", "--free", "--parent", "main"],
                "mutually exclusive",
            ),
            (vec!["--parent", "main"], "one of --group <X> or --free"),
            (vec!["--free"], "--parent <branch> is required"),
            (vec!["--free", "--parent"], "--parent requires a value"),
            (
                vec!["--group", "a", "--group", "b", "--parent", "m"],
                "--group given twice",
            ),
            (
                vec!["--free", "--parent", "m", "extra"],
                "unknown argument 'extra'",
            ),
        ] {
            let e = parse_adopt_args(&args(&a)).unwrap_err();
            assert!(e.contains(needle), "for {a:?}: {e}");
        }
    }

    #[test]
    fn merge_args_modes_and_message() {
        assert_eq!(
            parse_merge_args("merge", &args(&["--squash", "-m", "msg"])).unwrap(),
            (Some(MergeMode::Squash), Some("msg".into()))
        );
        assert_eq!(
            parse_merge_args("merge", &args(&["-mjoined", "--no-ff"])).unwrap(),
            (Some(MergeMode::NoFf), Some("joined".into()))
        );
        assert_eq!(parse_merge_args("merge", &args(&[])).unwrap(), (None, None));
        // a dash-leading phrase is plainly a value, not a flag
        assert_eq!(
            parse_merge_args("merge", &args(&["-m", "- fix the thing"]))
                .unwrap()
                .1,
            Some("- fix the thing".into())
        );
        // land takes the same flags, and says so when it refuses them
        assert_eq!(
            parse_merge_args("land", &args(&["--rebase"])).unwrap(),
            (Some(MergeMode::Rebase), None)
        );
        let e = parse_merge_args("land", &args(&["--bogus"])).unwrap_err();
        assert!(e.starts_with("wtree land:"), "{e}");
    }

    #[test]
    fn merge_args_refusals() {
        let e = parse_merge_args("merge", &args(&["--squash", "--ff"])).unwrap_err();
        assert!(e.contains("exactly one"), "{e}");
        let e = parse_merge_args("merge", &args(&["-m"])).unwrap_err();
        assert!(e.contains("needs a message"), "{e}");
        let e = parse_merge_args("merge", &args(&["-m", "--ff"])).unwrap_err();
        assert!(e.contains("reads as a flag"), "{e}");
        let e = parse_merge_args("merge", &args(&["-m", "a", "-m", "b"])).unwrap_err();
        assert!(e.contains("given twice"), "{e}");
        let e = parse_merge_args("merge", &args(&["--bogus"])).unwrap_err();
        assert!(e.contains("unknown argument"), "{e}");
    }

    #[test]
    fn open_takes_one_branch_and_close_takes_only_a_key() {
        assert_eq!(parse_open_args(&args(&["feature/a"])).unwrap(), "feature/a");
        // No branch at all is Missing, so the caller can list the candidates.
        assert!(matches!(parse_open_args(&args(&[])), Err(ArgErr::Missing)));
        for (a, needle) in [
            (vec!["a", "b"], "unexpected extra argument 'b'"),
            (vec!["--force"], "unknown flag '--force'"),
        ] {
            let Err(ArgErr::Bad(e)) = parse_open_args(&args(&a)) else {
                panic!("for {a:?}: expected a bad-argument error");
            };
            assert!(e.contains(needle), "for {a:?}: {e}");
        }
        assert_eq!(parse_close_args(&args(&[])).unwrap(), None);
        assert_eq!(
            parse_close_args(&args(&["--key", "ab12c"])).unwrap(),
            Some("ab12c".into())
        );
        for (a, needle) in [
            (vec!["--key"], "--key requires a value"),
            (vec!["--key", "a", "--key", "b"], "--key given twice"),
            // close has no relation layer, so there is no --force to pass it
            (vec!["--force"], "unknown argument '--force'"),
        ] {
            let e = parse_close_args(&args(&a)).unwrap_err();
            assert!(e.contains(needle), "for {a:?}: {e}");
        }
    }

    #[test]
    fn destroy_args_force_and_key() {
        assert_eq!(parse_destroy_args(&args(&[])).unwrap(), (false, None));
        assert_eq!(
            parse_destroy_args(&args(&["--force", "--key", "ab12c"])).unwrap(),
            (true, Some("ab12c".into()))
        );
        for (a, needle) in [
            (vec!["--key"], "--key requires a value"),
            (vec!["--key", "a", "--key", "b"], "--key given twice"),
            // wtree.sh's positional key is not carried over: the refusal that
            // issues one spells --key, so a bare word is a typo
            (vec!["ab12c"], "unknown argument 'ab12c'"),
        ] {
            let e = parse_destroy_args(&args(&a)).unwrap_err();
            assert!(e.contains(needle), "for {a:?}: {e}");
        }
    }
}
