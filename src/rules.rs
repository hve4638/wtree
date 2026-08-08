//! Policy rules (`.git/wtree/rules`) parser and strict validation.
//!
//! Syntax: INI-like. `[<branch>]` / `[group:<name>]` headers, `key = value`
//! entries where a value may be a comma-separated list. `#` starts a comment,
//! both full-line and inline. No quoting/escaping — a literal `#` cannot appear
//! in a value (conservative rule; revisit only if a real need shows up).
//!
//! All load and validation problems are collected and reported together;
//! nothing stops at the first failure. Messages cite `(label:line)` so later
//! verbs can quote rules as `(.git/wtree/rules:14)`.

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SectionKind {
    Branch,
    Group,
}

impl SectionKind {
    /// The section's header as written in the rules — the spelling every
    /// message quotes, so that a citation can be pasted back into the file.
    pub fn header(self, name: &str) -> String {
        match self {
            SectionKind::Branch => format!("[{name}]"),
            SectionKind::Group => format!("[group:{name}]"),
        }
    }
}

/// One token of a `children` list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Child {
    /// `group:X` — reference to a declared `[group:X]`.
    GroupRef(String),
    /// Bare name — reference to a declared `[X]` (valid in branch sections only).
    Bare(String),
    /// `*` — free-branch fallback.
    Star,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MergeMode {
    Squash,
    Rebase,
    NoFf,
    Ff,
}

impl MergeMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "squash" => Some(MergeMode::Squash),
            "rebase" => Some(MergeMode::Rebase),
            "no-ff" => Some(MergeMode::NoFf),
            "ff" => Some(MergeMode::Ff),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            MergeMode::Squash => "squash",
            MergeMode::Rebase => "rebase",
            MergeMode::NoFf => "no-ff",
            MergeMode::Ff => "ff",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub key: String,
    pub value: String,
    pub line: usize,
}

#[derive(Debug, Clone)]
pub struct Section {
    pub kind: SectionKind,
    pub name: String,
    pub line: usize,
    pub entries: Vec<Entry>,
}

impl Section {
    pub fn entry(&self, key: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.key == key)
    }

    pub fn header(&self) -> String {
        self.kind.header(&self.name)
    }
}

#[derive(Debug, Default)]
pub struct Rules {
    pub sections: Vec<Section>,
}

/// Result of loading rules text: the parsed structure plus every collected
/// load/validation error and warning. `rules` is usable even with errors
/// (best-effort parse) but callers must treat any error as load failure.
#[derive(Debug)]
pub struct Loaded {
    pub rules: Rules,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

pub fn load_str(text: &str, label: &str) -> Loaded {
    let (rules, mut errors) = parse(text, label);
    let (verrors, warnings) = rules.validate(label);
    errors.extend(verrors);
    Loaded {
        rules,
        errors,
        warnings,
    }
}

fn split_list(value: &str) -> impl Iterator<Item = &str> {
    value.split(',').map(str::trim).filter(|t| !t.is_empty())
}

fn parse_child(token: &str) -> Child {
    if token == "*" {
        Child::Star
    } else if let Some(g) = token.strip_prefix("group:") {
        Child::GroupRef(g.to_string())
    } else {
        Child::Bare(token.to_string())
    }
}

/// fnmatch-style matcher: only `*` and `?` are special, `*` matches any
/// sequence including `/` (no path-component semantics).
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0usize, 0usize);
    let mut backtrack: Option<(usize, usize)> = None; // (pattern idx after '*', name idx to retry)
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            backtrack = Some((pi + 1, ni));
            pi += 1;
        } else if let Some((bp, bn)) = backtrack {
            backtrack = Some((bp, bn + 1));
            pi = bp;
            ni = bn + 1;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Tracks where entries of the current section should go while parsing.
enum Cursor {
    /// Before any section header.
    None,
    /// Index into `Rules::sections`.
    At(usize),
    /// Header was invalid (malformed, bad name, duplicate) — the header
    /// error already covers the whole section, so its entries are swallowed
    /// instead of producing a noisy secondary error per line.
    Skip,
}

pub fn parse(text: &str, label: &str) -> (Rules, Vec<String>) {
    let mut cfg = Rules::default();
    let mut errors = Vec::new();
    let mut cursor = Cursor::None;

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

        if line.starts_with('[') {
            cursor = Cursor::Skip;
            let Some(inner) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
                errors.push(format!(
                    "malformed section header '{line}' ({label}:{lineno})"
                ));
                continue;
            };
            let inner = inner.trim();
            // The pre-2026-08-06 spelling would otherwise fail as "whitespace in
            // a branch name", which does not say what to type instead.
            let old_syntax = inner
                .strip_prefix("branch ")
                .map(|n| format!("[{}]", n.trim()))
                .or_else(|| {
                    inner
                        .strip_prefix("group ")
                        .map(|n| format!("[group:{}]", n.trim()))
                });
            if let Some(new) = old_syntax {
                errors.push(format!(
                    "old section syntax '{line}' — use '{new}' ({label}:{lineno})"
                ));
                continue;
            }
            let (kind, name) = match inner.strip_prefix("group:") {
                Some(g) => (SectionKind::Group, g),
                None => (SectionKind::Branch, inner),
            };
            if name.is_empty() {
                errors.push(format!("empty section name '{line}' ({label}:{lineno})"));
                continue;
            }
            // git forbids both in a ref name, so neither can be part of a
            // legitimate branch or group name.
            if name.contains(char::is_whitespace)
                || (kind == SectionKind::Group && name.contains(':'))
            {
                errors.push(format!(
                    "invalid section name '{name}' in '{line}' ({label}:{lineno})"
                ));
                continue;
            }
            if cfg.section(kind, name).is_some() {
                errors.push(format!(
                    "duplicate section {} ({label}:{lineno})",
                    kind.header(name)
                ));
                continue;
            }
            cfg.sections.push(Section {
                kind,
                name: name.to_string(),
                line: lineno,
                entries: Vec::new(),
            });
            cursor = Cursor::At(cfg.sections.len() - 1);
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            errors.push(format!(
                "expected 'key = value', got '{line}' ({label}:{lineno})"
            ));
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() {
            errors.push(format!("empty key ({label}:{lineno})"));
            continue;
        }
        match cursor {
            Cursor::None => {
                errors.push(format!(
                    "'{key}' appears outside of any section ({label}:{lineno})"
                ));
            }
            Cursor::Skip => {}
            Cursor::At(i) => {
                let section = &mut cfg.sections[i];
                if value.is_empty() {
                    errors.push(format!(
                        "empty value for '{key}' in {} ({label}:{lineno})",
                        section.header()
                    ));
                    continue;
                }
                if section.entry(key).is_some() {
                    errors.push(format!(
                        "duplicate key '{key}' in {} ({label}:{lineno})",
                        section.header()
                    ));
                    continue;
                }
                section.entries.push(Entry {
                    key: key.to_string(),
                    value: value.to_string(),
                    line: lineno,
                });
            }
        }
    }
    (cfg, errors)
}

// `description` is free text, printed and never acted on — the one key with no
// valid set to validate against.
const BRANCH_KEYS: &[&str] = &[
    "children",
    "destroyable",
    "merge-mode",
    "copy",
    "description",
];
const GROUP_KEYS: &[&str] = &[
    "children",
    "name-allow",
    "name-deny",
    "ephemeral",
    "merge-mode",
    "copy",
    "description",
];

/// One entry of a `copy` list. A trailing `/` marks it as matching directories
/// only (the gitignore convention), so a bare `node_modules` cannot drag in a
/// tree by accident — pulling one in has to be spelled out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyPattern {
    pub glob: String,
    pub dir_only: bool,
}

impl CopyPattern {
    fn parse(tok: &str) -> Self {
        match tok.strip_suffix('/') {
            Some(g) => CopyPattern {
                glob: g.to_string(),
                dir_only: true,
            },
            None => CopyPattern {
                glob: tok.to_string(),
                dir_only: false,
            },
        }
    }

    /// Entries are matched by their name alone, so the kind has to agree
    /// exactly: without the slash a pattern never takes a directory, and with
    /// it never takes a file.
    pub fn matches(&self, name: &str, is_dir: bool) -> bool {
        self.dir_only == is_dir && glob_match(&self.glob, name)
    }
}

impl Rules {
    pub fn section(&self, kind: SectionKind, name: &str) -> Option<&Section> {
        self.sections
            .iter()
            .find(|s| s.kind == kind && s.name == name)
    }

    pub fn get(&self, kind: SectionKind, name: &str, key: &str) -> Option<&str> {
        self.section(kind, name)?
            .entry(key)
            .map(|e| e.value.as_str())
    }

    pub fn line_of(&self, kind: SectionKind, name: &str, key: &str) -> Option<usize> {
        self.section(kind, name)?.entry(key).map(|e| e.line)
    }

    pub fn branch_names(&self) -> impl Iterator<Item = &str> {
        self.sections
            .iter()
            .filter(|s| s.kind == SectionKind::Branch)
            .map(|s| s.name.as_str())
    }

    pub fn group_names(&self) -> impl Iterator<Item = &str> {
        self.sections
            .iter()
            .filter(|s| s.kind == SectionKind::Group)
            .map(|s| s.name.as_str())
    }

    /// Parsed `children` of a section. Missing key or section = empty (fail closed).
    pub fn children_of(&self, kind: SectionKind, name: &str) -> Vec<Child> {
        self.get(kind, name, "children")
            .map(|v| split_list(v).map(parse_child).collect())
            .unwrap_or_default()
    }

    /// `[branch]` sections that list `branch` bare in their `children` — the
    /// parent(s) of a fixed branch. Validated rules have at most one.
    pub fn bare_parent_sections(&self, branch: &str) -> Vec<&Section> {
        self.sections
            .iter()
            .filter(|s| {
                s.kind == SectionKind::Branch
                    && self
                        .children_of(SectionKind::Branch, &s.name)
                        .iter()
                        .any(|c| matches!(c, Child::Bare(b) if b == branch))
            })
            .collect()
    }

    /// Default: true. Only valid on `[branch]`.
    pub fn destroyable(&self, branch: &str) -> bool {
        self.get(SectionKind::Branch, branch, "destroyable") != Some("false")
    }

    /// Default: false. Only valid on `[group]`.
    pub fn ephemeral(&self, group: &str) -> bool {
        self.get(SectionKind::Group, group, "ephemeral") == Some("true")
    }

    /// `None` = key absent = every mode allowed. Invalid tokens are dropped
    /// here; validation reports them as errors.
    pub fn merge_modes(&self, kind: SectionKind, name: &str) -> Option<Vec<MergeMode>> {
        self.get(kind, name, "merge-mode")
            .map(|v| split_list(v).filter_map(MergeMode::parse).collect())
    }

    /// Untracked files a new worktree receives from its parent's. Missing key
    /// or section = empty, so nothing crosses unless a rule says it may.
    pub fn copy_list(&self, kind: SectionKind, name: &str) -> Vec<CopyPattern> {
        self.get(kind, name, "copy")
            .map(|v| split_list(v).map(CopyPattern::parse).collect())
            .unwrap_or_default()
    }

    pub fn name_allow(&self, group: &str) -> Vec<String> {
        self.pattern_list(group, "name-allow")
    }

    pub fn name_deny(&self, group: &str) -> Vec<String> {
        self.pattern_list(group, "name-deny")
    }

    fn pattern_list(&self, group: &str, key: &str) -> Vec<String> {
        self.get(SectionKind::Group, group, key)
            .map(|v| split_list(v).map(str::to_string).collect())
            .unwrap_or_default()
    }

    /// Returns (errors, warnings), all collected — never stops at the first hit.
    pub fn validate(&self, label: &str) -> (Vec<String>, Vec<String>) {
        let mut errors = Vec::new();
        let mut warnings = Vec::new();
        let branches: Vec<&str> = self.branch_names().collect();
        let groups: Vec<&str> = self.group_names().collect();

        // bare child name -> (parent branch name, line of first listing)
        let mut bare_parent: HashMap<String, String> = HashMap::new();
        // bare edges child -> parent, in section order for deterministic reports
        let mut edges: Vec<(String, String)> = Vec::new();

        for s in &self.sections {
            let allowed = match s.kind {
                SectionKind::Branch => BRANCH_KEYS,
                SectionKind::Group => GROUP_KEYS,
            };
            for e in &s.entries {
                if !allowed.contains(&e.key.as_str()) {
                    errors.push(format!(
                        "unknown key '{}' in {} ({label}:{})",
                        e.key,
                        s.header(),
                        e.line
                    ));
                    continue;
                }
                match e.key.as_str() {
                    "destroyable" | "ephemeral" => {
                        if e.value != "true" && e.value != "false" {
                            errors.push(format!(
                                "invalid value '{}' for '{}' in {}: expected true or false ({label}:{})",
                                e.value, e.key, s.header(), e.line
                            ));
                        }
                    }
                    "merge-mode" => {
                        for tok in split_list(&e.value) {
                            if MergeMode::parse(tok).is_none() {
                                errors.push(format!(
                                    "invalid merge-mode '{tok}' in {}: expected squash, rebase, no-ff or ff ({label}:{})",
                                    s.header(), e.line
                                ));
                            }
                        }
                    }
                    // Entries are matched by name at the worktree root, so a
                    // pattern with a separator can never match anything. Left
                    // to load quietly it would be a policy that does nothing —
                    // the exact mistake the strict parser exists to catch.
                    "copy" => {
                        for tok in split_list(&e.value) {
                            let glob = tok.strip_suffix('/').unwrap_or(tok);
                            if glob.is_empty() || glob.contains('/') {
                                errors.push(format!(
                                    "invalid copy pattern '{tok}' in {}: patterns match entries at the worktree root, so '/' may only end one to mark a directory ({label}:{})",
                                    s.header(), e.line
                                ));
                            }
                        }
                    }
                    _ => {}
                }
            }

            if let Some(e) = s.entry("children") {
                for child in split_list(&e.value).map(parse_child) {
                    match child {
                        Child::Star => {}
                        Child::GroupRef(g) => {
                            if !groups.contains(&g.as_str()) {
                                errors.push(format!(
                                    "{} children: reference to undeclared group 'group:{g}' ({label}:{})",
                                    s.header(), e.line
                                ));
                            }
                        }
                        Child::Bare(b) => {
                            if s.kind == SectionKind::Group {
                                errors.push(format!(
                                    "[group:{}] children: bare branch name '{b}' not allowed (its parent would not be unique); only group:X and * ({label}:{})",
                                    s.name, e.line
                                ));
                                continue;
                            }
                            if !branches.contains(&b.as_str()) {
                                errors.push(format!(
                                    "[{}] children: reference to undeclared branch '{b}' ({label}:{})",
                                    s.name, e.line
                                ));
                                continue;
                            }
                            if let Some(prev) = bare_parent.get(&b) {
                                errors.push(format!(
                                    "branch '{b}' listed in children of both [{prev}] and [{}]: a fixed branch must have exactly one parent ({label}:{})",
                                    s.name, e.line
                                ));
                            } else {
                                bare_parent.insert(b.clone(), s.name.clone());
                                edges.push((b, s.name.clone()));
                            }
                        }
                    }
                }
            }
        }

        // Cycle detection over bare edges (child -> parent). Each node has at
        // most one outgoing edge, so a walk with a global visited set finds
        // every cycle exactly once.
        let edge_map: HashMap<&str, &str> = edges
            .iter()
            .map(|(c, p)| (c.as_str(), p.as_str()))
            .collect();
        let mut visited: Vec<&str> = Vec::new();
        for (start, _) in &edges {
            let mut path: Vec<&str> = Vec::new();
            let mut cur = start.as_str();
            loop {
                if let Some(pos) = path.iter().position(|&n| n == cur) {
                    let mut cyc: Vec<&str> = path[pos..].to_vec();
                    cyc.push(cur);
                    errors.push(format!("fixed-branch cycle: {}", cyc.join(" -> ")));
                    break;
                }
                if visited.contains(&cur) {
                    break;
                }
                path.push(cur);
                match edge_map.get(cur) {
                    Some(&p) => cur = p,
                    None => break,
                }
            }
            visited.extend(path);
        }

        // Declared branch name vs group name patterns: refs 'dev' and 'dev/*'
        // cannot coexist at the git level.
        for s in self
            .sections
            .iter()
            .filter(|s| s.kind == SectionKind::Group)
        {
            for key in ["name-allow", "name-deny"] {
                let Some(e) = s.entry(key) else { continue };
                for pat in split_list(&e.value) {
                    for b in &branches {
                        if pat.starts_with(&format!("{b}/")) {
                            errors.push(format!(
                                "[group:{}] pattern '{pat}' conflicts with [{b}]: git refs '{b}' and '{pat}' cannot coexist ({label}:{})",
                                s.name, e.line
                            ));
                        }
                    }
                }
            }
        }

        // Warning: an unconstrained group (no name-allow) next to a constrained
        // one in the same children list makes every name matching the
        // constrained pattern ambiguous.
        for s in &self.sections {
            let Some(e) = s.entry("children") else {
                continue;
            };
            let refs: Vec<String> = split_list(&e.value)
                .map(parse_child)
                .filter_map(|c| match c {
                    Child::GroupRef(g) => Some(g),
                    _ => None,
                })
                .collect();
            let (unconstrained, constrained): (Vec<&String>, Vec<&String>) = refs
                .iter()
                .partition(|g| self.get(SectionKind::Group, g, "name-allow").is_none());
            if !unconstrained.is_empty() && !constrained.is_empty() {
                warnings.push(format!(
                    "{} children mixes unconstrained group(s) {:?} with constrained group(s) {:?}: names matching the constrained patterns become ambiguous ({label}:{})",
                    s.header(),
                    unconstrained,
                    constrained,
                    e.line
                ));
            }
        }

        (errors, warnings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Verbatim rules sketch from DESIGN.md ("config 스케치" section).
    const SKETCH: &str = "[main]
children = dev, group:hotfix        # bare 이름 = 선언된 고정 브랜치 참조. 여기서 생성되고, 여기로만 돌아온다
destroyable = false                 # init 템플릿 기본값
merge-mode = squash                 # main으로 들어오는 merge는 squash만

[dev]
children = group:work
merge-mode = squash

[group:hotfix]
name-allow = hotfix/*               # main 아래 유일한 그룹이라 판별에는 불필요하나 관례 강제용

[group:work]
name-allow = feat/*, fix/*, refactor/*   # dev/*는 [dev]와 git 수준 충돌이라 불가
ephemeral = true                    # 로컬 전용 — 부모 destroy 시 깨끗하면 leaf부터 함께 수거
copy = .env, .env.local             # 부모 워크트리에서 딸려올 미추적 파일. 디렉터리는 끝에 `/`
";

    fn load(text: &str) -> Loaded {
        load_str(text, ".git/wtree/rules")
    }

    fn has(list: &[String], needle: &str) -> bool {
        list.iter().any(|m| m.contains(needle))
    }

    #[test]
    fn sketch_parses_clean() {
        let l = load(SKETCH);
        assert!(l.errors.is_empty(), "unexpected errors: {:?}", l.errors);
        assert!(
            l.warnings.is_empty(),
            "unexpected warnings: {:?}",
            l.warnings
        );
        assert_eq!(l.rules.sections.len(), 4);
    }

    #[test]
    fn sketch_children_parsed() {
        let l = load(SKETCH);
        assert_eq!(
            l.rules.children_of(SectionKind::Branch, "main"),
            vec![Child::Bare("dev".into()), Child::GroupRef("hotfix".into())]
        );
        assert_eq!(
            l.rules.children_of(SectionKind::Branch, "dev"),
            vec![Child::GroupRef("work".into())]
        );
    }

    #[test]
    fn defaults_applied() {
        let l = load("[main]\nchildren = group:g\n\n[group:g]\nname-allow = g/*\n");
        assert!(l.errors.is_empty(), "{:?}", l.errors);
        let c = &l.rules;
        assert!(c.destroyable("main"));
        assert!(!c.ephemeral("g"));
        assert_eq!(c.merge_modes(SectionKind::Branch, "main"), None);
        assert_eq!(c.children_of(SectionKind::Group, "g"), vec![]);
        // sketch overrides
        let s = load(SKETCH);
        assert!(!s.rules.destroyable("main"));
        assert!(s.rules.ephemeral("work"));
        assert_eq!(
            s.rules.merge_modes(SectionKind::Branch, "main"),
            Some(vec![MergeMode::Squash])
        );
    }

    #[test]
    fn inline_comment_and_comma_list() {
        let l =
            load("[main]\nchildren = dev , group:g , *   # tail comment\n\n[dev]\n\n[group:g]\n");
        assert!(l.errors.is_empty(), "{:?}", l.errors);
        assert_eq!(
            l.rules.children_of(SectionKind::Branch, "main"),
            vec![
                Child::Bare("dev".into()),
                Child::GroupRef("g".into()),
                Child::Star
            ]
        );
    }

    #[test]
    fn merge_mode_list_parsed() {
        let l = load("[main]\nmerge-mode = squash, no-ff\n");
        assert!(l.errors.is_empty(), "{:?}", l.errors);
        assert_eq!(
            l.rules.merge_modes(SectionKind::Branch, "main"),
            Some(vec![MergeMode::Squash, MergeMode::NoFf])
        );
    }

    #[test]
    fn duplicate_key_is_error() {
        let l = load("[main]\ndestroyable = false\ndestroyable = true\n");
        assert!(
            has(&l.errors, "duplicate key 'destroyable'"),
            "{:?}",
            l.errors
        );
        // first occurrence wins in the parsed structure
        assert!(!l.rules.destroyable("main"));
    }

    #[test]
    fn duplicate_section_is_error() {
        let l = load("[main]\n\n[main]\ndestroyable = false\n");
        assert!(has(&l.errors, "duplicate section [main]"), "{:?}", l.errors);
    }

    #[test]
    fn empty_value_is_error() {
        let l = load("[main]\nchildren =\n");
        assert!(
            has(&l.errors, "empty value for 'children'"),
            "{:?}",
            l.errors
        );
    }

    #[test]
    fn bad_section_name_is_error() {
        let l = load("[node main]\nchildren = x\n");
        assert!(
            has(&l.errors, "invalid section name 'node main'"),
            "{:?}",
            l.errors
        );
        // entries of the bad section are swallowed, not double-reported
        assert_eq!(l.errors.len(), 1, "{:?}", l.errors);
        assert!(has(&load("[]\n").errors, "empty section name '[]'"));
        assert!(has(
            &load("[group:]\n").errors,
            "empty section name '[group:]'"
        ));
        assert!(has(
            &load("[group:a:b]\n").errors,
            "invalid section name 'a:b'"
        ));
    }

    #[test]
    fn old_section_syntax_points_at_the_new_one() {
        let l = load("[main]\n\n[branch dev]\n\n[group develop]\n");
        assert!(
            has(
                &l.errors,
                "old section syntax '[branch dev]' — use '[dev]' (.git/wtree/rules:3)"
            ),
            "{:?}",
            l.errors
        );
        assert!(
            has(
                &l.errors,
                "old section syntax '[group develop]' — use '[group:develop]' (.git/wtree/rules:5)"
            ),
            "{:?}",
            l.errors
        );
        // a branch section may still be named 'branch' or 'group'
        assert!(load("[branch]\n\n[group]\n").errors.is_empty());
    }

    #[test]
    fn key_outside_section_is_error() {
        let l = load("children = dev\n[main]\n");
        assert!(has(&l.errors, "outside of any section"), "{:?}", l.errors);
    }

    #[test]
    fn unknown_key_is_error() {
        let l = load("[main]\nname-allow = x/*\n\n[group:g]\ndestroyable = true\n");
        assert!(
            has(&l.errors, "unknown key 'name-allow' in [main]"),
            "{:?}",
            l.errors
        );
        assert!(
            has(&l.errors, "unknown key 'destroyable' in [group:g]"),
            "{:?}",
            l.errors
        );
    }

    #[test]
    fn description_is_free_text_on_both_section_kinds() {
        let l = load(
            "[main]\ndescription = release only, never commit here\n\n[group:g]\ndescription = throwaway work\n",
        );
        assert!(l.errors.is_empty(), "{:?}", l.errors);
        assert_eq!(
            l.rules.get(SectionKind::Branch, "main", "description"),
            Some("release only, never commit here")
        );
        assert_eq!(
            l.rules.get(SectionKind::Group, "g", "description"),
            Some("throwaway work")
        );
    }

    #[test]
    fn undeclared_group_ref_is_error() {
        let l = load("[main]\nchildren = group:ghost\n");
        assert!(
            has(&l.errors, "undeclared group 'group:ghost'"),
            "{:?}",
            l.errors
        );
    }

    #[test]
    fn undeclared_bare_ref_is_error() {
        let l = load("[main]\nchildren = ghost\n");
        assert!(
            has(&l.errors, "undeclared branch 'ghost'"),
            "{:?}",
            l.errors
        );
    }

    #[test]
    fn bare_in_group_children_is_error() {
        let l = load("[main]\n\n[group:g]\nchildren = main\n");
        assert!(
            has(&l.errors, "bare branch name 'main' not allowed"),
            "{:?}",
            l.errors
        );
    }

    #[test]
    fn bare_parent_uniqueness_is_error() {
        let l = load("[main]\nchildren = dev\n\n[other]\nchildren = dev\n\n[dev]\n");
        assert!(
            has(
                &l.errors,
                "'dev' listed in children of both [main] and [other]"
            ),
            "{:?}",
            l.errors
        );
    }

    #[test]
    fn cycle_is_error() {
        let l = load("[main]\nchildren = dev\n\n[dev]\nchildren = main\n");
        assert!(
            has(&l.errors, "fixed-branch cycle: dev -> main -> dev"),
            "{:?}",
            l.errors
        );
        assert_eq!(
            l.errors.iter().filter(|e| e.contains("cycle")).count(),
            1,
            "cycle must be reported once: {:?}",
            l.errors
        );
        // self-loop
        let l2 = load("[main]\nchildren = main\n");
        assert!(
            has(&l2.errors, "fixed-branch cycle: main -> main"),
            "{:?}",
            l2.errors
        );
    }

    #[test]
    fn ref_namespace_conflict_is_error() {
        let l = load("[dev]\n\n[group:g]\nname-allow = dev/*\n\n[group:h]\nname-deny = dev/?\n");
        assert!(
            has(&l.errors, "[group:g] pattern 'dev/*' conflicts with [dev]"),
            "{:?}",
            l.errors
        );
        assert!(
            has(&l.errors, "[group:h] pattern 'dev/?' conflicts with [dev]"),
            "{:?}",
            l.errors
        );
    }

    #[test]
    fn value_domain_violations_are_errors() {
        let l = load(
            "[main]\ndestroyable = maybe\nmerge-mode = squash, octopus\n\n[group:g]\nephemeral = nah\n",
        );
        assert!(
            has(&l.errors, "invalid value 'maybe' for 'destroyable'"),
            "{:?}",
            l.errors
        );
        assert!(
            has(&l.errors, "invalid merge-mode 'octopus'"),
            "{:?}",
            l.errors
        );
        assert!(
            has(&l.errors, "invalid value 'nah' for 'ephemeral'"),
            "{:?}",
            l.errors
        );
        assert_eq!(l.errors.len(), 3, "{:?}", l.errors);
    }

    #[test]
    fn mixed_constraint_groups_warn() {
        let l = load(
            "[main]\nchildren = group:strict, group:loose\n\n[group:strict]\nname-allow = s/*\n\n[group:loose]\n",
        );
        assert!(l.errors.is_empty(), "{:?}", l.errors);
        assert!(
            has(&l.warnings, "mixes unconstrained group(s)"),
            "{:?}",
            l.warnings
        );
        assert!(has(&l.warnings, "loose"), "{:?}", l.warnings);
        assert!(has(&l.warnings, "strict"), "{:?}", l.warnings);
    }

    #[test]
    fn all_problems_collected_not_first_only() {
        let l = load("[main]\nbogus = 1\nchildren = ghost\ndestroyable = maybe\n");
        assert!(
            l.errors.len() >= 3,
            "expected all errors collected: {:?}",
            l.errors
        );
    }

    #[test]
    fn line_numbers_accurate() {
        let c = load(SKETCH).rules;
        assert_eq!(
            c.line_of(SectionKind::Branch, "main", "merge-mode"),
            Some(4)
        );
        assert_eq!(c.line_of(SectionKind::Branch, "dev", "children"), Some(7));
        assert_eq!(c.line_of(SectionKind::Group, "work", "ephemeral"), Some(15));
        assert_eq!(c.section(SectionKind::Group, "hotfix").unwrap().line, 10);
        // error messages cite label:line
        let l = load("[main]\n\nbogus = 1\n");
        assert!(has(&l.errors, "(.git/wtree/rules:3)"), "{:?}", l.errors);
    }

    #[test]
    fn bare_parent_section_helper() {
        let c = load(SKETCH).rules;
        let parents = c.bare_parent_sections("dev");
        assert_eq!(parents.len(), 1);
        assert_eq!(parents[0].name, "main");
        assert!(c.bare_parent_sections("main").is_empty());
        assert!(c.bare_parent_sections("nope").is_empty());
    }

    #[test]
    fn glob_matcher_semantics() {
        assert!(glob_match("feature/*", "feature/a"));
        assert!(glob_match("feature/*", "feature/a/b")); // '*' crosses '/'
        assert!(!glob_match("feature/*", "feature")); // literal '/' must be present
        assert!(glob_match("feature/*", "feature/")); // '*' matches empty
        assert!(glob_match("*", "anything/at/all"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(!glob_match("hotfix/*", "feature/a"));
        assert!(glob_match("*fix*", "hotfix/x"));
        assert!(glob_match("", ""));
        assert!(!glob_match("", "a"));
    }

    #[test]
    fn name_allow_deny_accessors() {
        let l = load("[group:g]\nname-allow = a/*, b/*\nname-deny = a/wip-*\n");
        assert!(l.errors.is_empty(), "{:?}", l.errors);
        assert_eq!(l.rules.name_allow("g"), vec!["a/*", "b/*"]);
        assert_eq!(l.rules.name_deny("g"), vec!["a/wip-*"]);
        assert!(l.rules.name_allow("missing").is_empty());
    }
}
