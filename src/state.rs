//! Worktree state file: `<private git dir>/WT_HEAD`. Named after git's own
//! per-worktree pointers (`HEAD`, `ORIG_HEAD`) — it records which branch this
//! worktree was set up for, and is compared against HEAD to detect drift.
//!
//! Records what `wtree new`/`wtree adopt` decided at creation time: the worktree's
//! own branch (integrity check against HEAD), its identity (group member or
//! free) and its recorded parent. Fixed branches never have a state file.
//!
//! Reading is fail-closed: a missing file is `Missing` (unmanaged), and
//! anything that does not parse to exactly the known fields is `Invalid` with
//! a reason. Writing goes through a temp file in the same directory + rename,
//! so a reader sees either the whole previous record or the whole new one,
//! never a half-written file. Nothing is fsynced: a record lost to a system
//! crash reads as unmanaged, which fails closed and `wtree adopt` rebuilds.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const STATE_FILE: &str = "WT_HEAD";
pub const VERSION: &str = "1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Group(String),
    Free,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        if s == "free" {
            return Some(Kind::Free);
        }
        match s.strip_prefix("group:") {
            Some(g) if !g.is_empty() => Some(Kind::Group(g.to_string())),
            _ => None,
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Kind::Free => f.write_str("free"),
            Kind::Group(g) => write!(f, "group:{g}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    /// Short branch name (e.g. `feature/a`) — compared against HEAD by the
    /// judgment core; a mismatch means raw switch/rename happened.
    pub branch: String,
    pub kind: Kind,
    pub parent: String,
}

impl State {
    /// One-line summary shown when an existing record is about to be replaced
    /// (re-adopt / mismatch adopt) — never a silent overwrite.
    pub fn summary(&self) -> String {
        format!(
            "branch={}, kind={}, parent={}",
            self.branch, self.kind, self.parent
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateRead {
    /// No state file — not managed by wtree (fail closed).
    Missing,
    /// File exists but is corrupt: missing field, unknown version, bad syntax.
    Invalid {
        reason: String,
    },
    Valid(State),
}

pub fn state_path(private_git_dir: &Path) -> PathBuf {
    private_git_dir.join(STATE_FILE)
}

pub fn read(private_git_dir: &Path) -> StateRead {
    let path = state_path(private_git_dir);
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return StateRead::Missing,
        Err(e) => {
            return StateRead::Invalid {
                reason: format!("cannot read state file: {e}"),
            };
        }
    };
    parse(&text)
}

fn invalid(reason: impl Into<String>) -> StateRead {
    StateRead::Invalid {
        reason: reason.into(),
    }
}

pub fn parse(text: &str) -> StateRead {
    let mut version = None;
    let mut branch = None;
    let mut kind = None;
    let mut parent = None;
    for (idx, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            return invalid(format!(
                "line {}: expected 'key = value', got '{line}'",
                idx + 1
            ));
        };
        let (k, v) = (k.trim(), v.trim());
        if v.is_empty() {
            return invalid(format!("line {}: empty value for '{k}'", idx + 1));
        }
        let slot = match k {
            "version" => &mut version,
            "branch" => &mut branch,
            "kind" => &mut kind,
            "parent" => &mut parent,
            other => return invalid(format!("line {}: unknown key '{other}'", idx + 1)),
        };
        if slot.is_some() {
            return invalid(format!("line {}: duplicate key '{k}'", idx + 1));
        }
        *slot = Some(v.to_string());
    }
    let Some(version) = version else {
        return invalid("missing field 'version'");
    };
    if version != VERSION {
        return invalid(format!("unknown version '{version}' (expected {VERSION})"));
    }
    let Some(branch) = branch else {
        return invalid("missing field 'branch'");
    };
    let Some(kind_raw) = kind else {
        return invalid("missing field 'kind'");
    };
    let Some(parent) = parent else {
        return invalid("missing field 'parent'");
    };
    let Some(kind) = Kind::parse(&kind_raw) else {
        return invalid(format!(
            "invalid kind '{kind_raw}' (expected 'group:X' or 'free')"
        ));
    };
    StateRead::Valid(State {
        branch,
        kind,
        parent,
    })
}

pub fn serialize(state: &State) -> String {
    format!(
        "version = {VERSION}\nbranch = {}\nkind = {}\nparent = {}\n",
        state.branch, state.kind, state.parent
    )
}

/// Atomic write: temp file in the same directory, then rename over the target.
/// The pid in the temp name keeps concurrent `wtree` processes off each other.
pub fn write(private_git_dir: &Path, state: &State) -> io::Result<()> {
    let tmp = private_git_dir.join(format!("{STATE_FILE}.{}.tmp", std::process::id()));
    fs::write(&tmp, serialize(state))?;
    fs::rename(&tmp, state_path(private_git_dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn sample() -> State {
        State {
            branch: "feature/a".into(),
            kind: Kind::Group("develop".into()),
            parent: "dev".into(),
        }
    }

    fn reason_of(r: StateRead) -> String {
        match r {
            StateRead::Invalid { reason } => reason,
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new();
        let s = sample();
        write(&dir.0, &s).unwrap();
        assert_eq!(read(&dir.0), StateRead::Valid(s));
        // free kind too
        let f = State {
            branch: "x".into(),
            kind: Kind::Free,
            parent: "main".into(),
        };
        write(&dir.0, &f).unwrap();
        assert_eq!(read(&dir.0), StateRead::Valid(f));
    }

    #[test]
    fn missing_file_is_missing() {
        let dir = TempDir::new();
        assert_eq!(read(&dir.0), StateRead::Missing);
    }

    #[test]
    fn write_is_atomic_rename() {
        let dir = TempDir::new();
        write(&dir.0, &sample()).unwrap();
        // overwrite an existing record
        let s2 = State {
            branch: "b".into(),
            kind: Kind::Free,
            parent: "main".into(),
        };
        write(&dir.0, &s2).unwrap();
        assert_eq!(read(&dir.0), StateRead::Valid(s2));
        // no temp residue left behind, whatever the temp file was named
        let residue: Vec<String> = fs::read_dir(&dir.0)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != STATE_FILE)
            .collect();
        assert!(residue.is_empty(), "left behind: {residue:?}");
    }

    #[test]
    fn corrupt_variants_are_invalid_with_reason() {
        let cases: &[(&str, &str)] = &[
            (
                "version = 1\nbranch = a\nkind = free\n",
                "missing field 'parent'",
            ),
            (
                "branch = a\nkind = free\nparent = main\n",
                "missing field 'version'",
            ),
            (
                "version = 2\nbranch = a\nkind = free\nparent = main\n",
                "unknown version '2'",
            ),
            (
                "version = 1\nbranch = a\nkind = boss\nparent = main\n",
                "invalid kind 'boss'",
            ),
            (
                "version = 1\nbranch = a\nkind = group:\nparent = main\n",
                "invalid kind 'group:'",
            ),
            (
                "version = 1\nbranch = a\nkind = free\nparent = main\ncolor = red\n",
                "unknown key 'color'",
            ),
            (
                "version = 1\nbranch = a\nbranch = b\nkind = free\nparent = main\n",
                "duplicate key 'branch'",
            ),
            (
                "version = 1\nbranch =\nkind = free\nparent = main\n",
                "empty value",
            ),
            ("garbage line\n", "expected 'key = value'"),
        ];
        for (text, needle) in cases {
            let reason = reason_of(parse(text));
            assert!(
                reason.contains(needle),
                "for {text:?}: expected '{needle}' in '{reason}'"
            );
        }
    }

    #[test]
    fn summary_mentions_all_fields() {
        let s = sample().summary();
        assert!(s.contains("feature/a") && s.contains("group:develop") && s.contains("dev"));
    }
}
