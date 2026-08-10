//! Machine-local settings: `<git common dir>/wtree/settings`.
//!
//! Kept separate from the policy rules so the policy file can be copied to
//! teammates without dragging machine paths along. `key = value` lines, `#`
//! comments. Unknown keys are load errors (same strictness as the policy
//! rules — a typo must not silently fall back to defaults).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const SETTINGS_LABEL: &str = ".git/wtree/settings";

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Settings {
    /// `worktree-dir` — base directory under which `wtree new` places new
    /// worktrees. Relative paths resolve against the primary worktree root.
    /// Unset = `<repo parent>/<repo name>.worktrees`.
    pub worktree_dir: Option<PathBuf>,
}

/// Missing file = defaults. Any parse problem is an error (never a default).
pub fn load(path: &Path) -> Result<Settings, String> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Settings::default()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    parse(&text, SETTINGS_LABEL)
}

pub fn parse(text: &str, label: &str) -> Result<Settings, String> {
    let mut settings = Settings::default();
    let mut errors = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        let lineno = idx + 1;
        let line = match raw.find('#') {
            Some(i) => &raw[..i],
            None => raw,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            errors.push(format!(
                "expected 'key = value', got '{line}' ({label}:{lineno})"
            ));
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "worktree-dir" => {
                if value.is_empty() {
                    errors.push(format!("empty value for 'worktree-dir' ({label}:{lineno})"));
                } else if settings.worktree_dir.is_some() {
                    errors.push(format!("duplicate key 'worktree-dir' ({label}:{lineno})"));
                } else {
                    settings.worktree_dir = Some(PathBuf::from(value));
                }
            }
            other => errors.push(format!("unknown key '{other}' ({label}:{lineno})")),
        }
    }
    if errors.is_empty() {
        Ok(settings)
    } else {
        Err(errors.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn missing_file_is_defaults() {
        let dir = TempDir::new();
        assert_eq!(load(&dir.0.join("settings")).unwrap(), Settings::default());
    }

    #[test]
    fn worktree_dir_parsed_with_comments() {
        let s = parse("# machine settings\nworktree-dir = /x/wts  # inline\n", "L").unwrap();
        assert_eq!(s.worktree_dir, Some(PathBuf::from("/x/wts")));
    }

    #[test]
    fn strict_errors() {
        for (text, needle) in [
            ("worktree-dir = a\nworktree-dir = b\n", "duplicate key"),
            ("worktree-dir =\n", "empty value"),
            ("nonsense-key = 1\n", "unknown key 'nonsense-key'"),
            ("garbage\n", "expected 'key = value'"),
        ] {
            let err = parse(text, "L").unwrap_err();
            assert!(err.contains(needle), "for {text:?}: {err}");
            assert!(err.contains("(L:"), "for {text:?}: {err}");
        }
    }
}
