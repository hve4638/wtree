//! Judgment core: pure decisions over a `World` snapshot plus policy rules.
//!
//! No side effects — each `plan_*` returns either a plan describing what the
//! verb would do, or a `Refusal` listing every reason, citing rule values and
//! rules lines in the DESIGN.md error format.

use std::fmt;

use crate::repo::{World, WtFact};
use crate::rules::{Child, MergeMode, Rules, SectionKind};
use crate::state::{Kind, State, StateRead};

pub const ALL_MODES: [MergeMode; 4] = [
    MergeMode::Squash,
    MergeMode::Rebase,
    MergeMode::NoFf,
    MergeMode::Ff,
];

pub(crate) const ADOPT_HINT: &str = "recover with: wtree adopt";

/// Identity of a branch/worktree, resolved in DESIGN order:
/// (1) valid state record (only when recorded branch == HEAD),
/// (2) `[X]` declaration -> fixed, (3) unknown, fail closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    Fixed {
        branch: String,
    },
    GroupMember {
        branch: String,
        group: String,
        parent: String,
    },
    Free {
        branch: String,
        parent: String,
    },
    Unknown {
        reasons: Vec<String>,
    },
}

impl Identity {
    pub fn branch(&self) -> Option<&str> {
        match self {
            Identity::Fixed { branch }
            | Identity::GroupMember { branch, .. }
            | Identity::Free { branch, .. } => Some(branch),
            Identity::Unknown { .. } => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Refusal {
    pub verb: &'static str,
    pub subject: String,
    pub reasons: Vec<String>,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "wtree {}: refused", self.verb)?;
        writeln!(f, "  {}", self.subject)?;
        for r in &self.reasons {
            // A reason may carry extra lines (rule citations); indent them.
            let mut lines = r.lines();
            if let Some(first) = lines.next() {
                writeln!(f, "  \u{2717} {first}")?;
            }
            for cont in lines {
                writeln!(f, "      {cont}")?;
            }
        }
        Ok(())
    }
}

pub type Decision<T> = Result<T, Refusal>;

fn refuse<T>(verb: &'static str, subject: impl Into<String>, reasons: Vec<String>) -> Decision<T> {
    Err(Refusal {
        verb,
        subject: subject.into(),
        reasons,
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum NewPlan {
    /// Declared fixed branch, listed bare in the parent's children — created
    /// without a state record.
    Fixed {
        name: String,
        parent: String,
    },
    GroupMember {
        name: String,
        group: String,
        parent: String,
    },
    Free {
        name: String,
        parent: String,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub struct MergePlan {
    pub source: String,
    pub target: String,
    pub mode: MergeMode,
}

#[derive(Debug, PartialEq, Eq)]
pub struct SyncPlan {
    pub branch: String,
    pub parent: String,
}

#[derive(Debug, PartialEq, Eq)]
pub struct DestroyPlan {
    pub branch: String,
    /// Ephemeral descendants collected together with `branch`, leaf first.
    pub cascade: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct OpenPlan {
    pub branch: String,
    /// A declared `[branch]`: its identity comes from the rules, so the new
    /// worktree needs no state record and the branch is managed at once.
    /// Anything else stays unknown until it is adopted from there.
    pub fixed: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ClosePlan {
    /// `None` when HEAD is detached — there is no branch to keep.
    pub branch: Option<String>,
    /// Uncommitted work the confirmation key cleared; `git worktree remove`
    /// needs `--force` to carry that out.
    pub dirty: bool,
    /// Whether the state record goes with the worktree, leaving the branch
    /// unmanaged (group/free) or not (fixed, declared in the rules).
    pub drops_record: bool,
}

/// Naming rules of one group a parent may create into.
#[derive(Debug, PartialEq, Eq)]
pub struct GroupRule {
    pub group: String,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

/// What `new` may create here, judged without a name: the identity checks and
/// the parent's `children`. Every name-dependent check (already exists, name
/// reservation, group resolution) stays in `plan_new`.
#[derive(Debug, PartialEq, Eq)]
pub struct NewGate {
    pub parent: String,
    pub sec_kind: SectionKind,
    pub sec_name: String,
    pub groups: Vec<GroupRule>,
    /// Fixed names listed bare in `children`, existing or not.
    pub bares: Vec<String>,
    pub star: bool,
}

/// Everything `merge` settles before it looks at the mode flag.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct MergeGate {
    pub source: String,
    pub target: String,
    /// Modes the target accepts — the flag, if any, has to be one of these.
    pub modes: Vec<MergeMode>,
    pub cite: Option<String>,
}

/// `destroy` up to the point where its flags start to matter. The policy layer
/// and the child scan are decided here; the last two fields are facts, not
/// verdicts, because `--force` and `--key` are what settle them.
#[derive(Debug, PartialEq, Eq)]
pub struct DestroyGate {
    pub branch: String,
    pub cascade: Vec<String>,
    /// Why the parent link is broken, when it is. `--force` passes it.
    pub missing_parent: Option<String>,
    /// Work would be lost, so `--key` is required.
    pub needs_key: bool,
}

/// A branch `open` would accept right now.
#[derive(Debug, PartialEq, Eq)]
pub struct OpenCandidate {
    pub branch: String,
    /// Declared `[branch]`: managed the moment it is opened. Anything else
    /// lands unmanaged and needs `adopt` (see `OpenPlan::fixed`).
    pub fixed: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct AdoptPlan {
    pub branch: String,
    pub kind: Kind,
    pub parent: String,
    /// Summary of the record being replaced (re-adopt / mismatch recovery);
    /// `None` when there was no record. Never replaced silently.
    pub previous: Option<String>,
}

/// One verb the contextual menu may offer, with what the renderer needs to
/// spell its invocation. Built from the gates alone, so nothing here can
/// advertise what the verb would refuse on policy. `list` and `info` are
/// absent: they are always available and the renderer adds them unconditionally.
#[derive(Debug)]
pub enum Affordance {
    New(NewGate),
    Open(Vec<OpenCandidate>),
    Merge(MergeGate),
    Sync(SyncPlan),
    /// Carries the merge half's modes — `land` takes the same mode flags.
    Land(MergeGate),
    Close(ClosePlan),
    Destroy(DestroyGate),
    Adopt,
}

/// Verbs usable on an unknown (unmanaged) worktree. Everything else is
/// refused: merge/sync/land/destroy need a judged identity, and `new` is
/// refused separately because an unknown branch cannot act as a parent.
///
/// `close` is allowed because an unmanaged worktree has no record to lose, and
/// it is the way back out of an `open` that left one. `open` judges the branch
/// it is given, not the worktree it is typed in, so this worktree's identity
/// never enters into it.
pub fn verb_allowed_when_unknown(verb: &str) -> bool {
    matches!(
        verb,
        "adopt" | "list" | "info" | "init" | "save" | "open" | "close"
    )
}

/// Refusal reasons for a parent that is no longer managed. Its rules cannot be
/// read, and an unreadable rule set is not an absent one: reading it as "no
/// constraint" would quietly loosen exactly the policy the parent carried, so
/// the verb stops here (fail closed) and says how to reconnect the link.
fn unmanaged_parent(parent: &str, reasons: &[String]) -> Vec<String> {
    let mut rs = vec![format!(
        "parent '{parent}' is unmanaged — its rules cannot be read (fail closed)"
    )];
    rs.extend(reasons.iter().cloned());
    rs.push(format!(
        "restore it with: wtree open {parent}, then wtree adopt there"
    ));
    rs.push(
        "or re-parent this branch: wtree adopt --group <G> --parent <managed branch>".to_string(),
    );
    rs
}

/// Refusal reasons for a verb that would have to remove the primary worktree.
/// git will not: it is the repo checkout itself, not one of the worktrees
/// hanging off it. Judged here rather than left to git so the verb stops before
/// it has done half its work — `destroy` would otherwise delete nothing but
/// still have to report the branch it could not remove.
fn primary_worktree(verb: &'static str) -> Vec<String> {
    vec![
        "this is the primary worktree — git cannot remove it".to_string(),
        format!("{verb} removes a linked worktree; the primary one is the repo checkout itself"),
    ]
}

fn mode_list(modes: &[MergeMode]) -> String {
    modes
        .iter()
        .map(|m| m.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

pub struct Ctx<'a> {
    pub world: &'a World,
    pub cfg: &'a Rules,
    /// Label used when citing rules, e.g. ".git/wtree/rules".
    pub label: &'a str,
}

impl<'a> Ctx<'a> {
    fn current(&self) -> &WtFact {
        self.world.current()
    }

    fn is_primary(&self) -> bool {
        self.world.primary == Some(self.world.current)
    }

    fn current_head_or_detached(&self) -> String {
        self.current()
            .head
            .clone()
            .unwrap_or_else(|| "(detached)".to_string())
    }

    /// `rule: key = value    (.git/wtree/rules:N)` citation for refusal reasons.
    fn rule(&self, kind: SectionKind, name: &str, key: &str) -> String {
        match (
            self.cfg.get(kind, name, key),
            self.cfg.line_of(kind, name, key),
        ) {
            (Some(v), Some(l)) => format!("rule: {key} = {v}    ({}:{l})", self.label),
            _ => format!("rule: {key} unset in {}", kind.header(name)),
        }
    }

    pub fn identity_of(&self, wt: &WtFact) -> Identity {
        match &wt.state {
            StateRead::Invalid { reason } => Identity::Unknown {
                reasons: vec![
                    format!("state record is corrupt: {reason}"),
                    ADOPT_HINT.to_string(),
                ],
            },
            StateRead::Valid(s) => {
                let Some(head) = &wt.head else {
                    return Identity::Unknown {
                        reasons: vec![
                            format!("HEAD is detached but state records branch '{}'", s.branch),
                            ADOPT_HINT.to_string(),
                        ],
                    };
                };
                if head != &s.branch {
                    return Identity::Unknown {
                        reasons: vec![
                            format!(
                                "recorded branch '{}' != HEAD '{head}' — trace of raw switch/rename",
                                s.branch
                            ),
                            ADOPT_HINT.to_string(),
                        ],
                    };
                }
                match &s.kind {
                    Kind::Free => Identity::Free {
                        branch: s.branch.clone(),
                        parent: s.parent.clone(),
                    },
                    Kind::Group(g) => {
                        if self.cfg.section(SectionKind::Group, g).is_none() {
                            Identity::Unknown {
                                reasons: vec![
                                    format!(
                                        "recorded group '{g}' is no longer declared in the rules"
                                    ),
                                    ADOPT_HINT.to_string(),
                                ],
                            }
                        } else {
                            Identity::GroupMember {
                                branch: s.branch.clone(),
                                group: g.clone(),
                                parent: s.parent.clone(),
                            }
                        }
                    }
                }
            }
            StateRead::Missing => match &wt.head {
                None => Identity::Unknown {
                    reasons: vec!["HEAD is detached and no state record exists".to_string()],
                },
                Some(h) if self.cfg.section(SectionKind::Branch, h).is_some() => {
                    Identity::Fixed { branch: h.clone() }
                }
                Some(h) => Identity::Unknown {
                    reasons: vec![
                        format!("no state record and '{h}' is not a declared [branch]"),
                        ADOPT_HINT.to_string(),
                    ],
                },
            },
        }
    }

    pub fn current_identity(&self) -> Identity {
        self.identity_of(self.current())
    }

    /// Identity of an arbitrary branch (e.g. a merge target): a worktree whose
    /// valid record claims this branch wins (state over declaration —
    /// grandfather rule), then `[branch]` declaration, then unknown.
    pub fn branch_identity(&self, branch: &str) -> Identity {
        for wt in &self.world.facts {
            if wt.head.as_deref() == Some(branch) {
                if let StateRead::Valid(s) = &wt.state {
                    if s.branch == branch {
                        return self.identity_of(wt);
                    }
                }
            }
        }
        if self.cfg.section(SectionKind::Branch, branch).is_some() {
            return Identity::Fixed {
                branch: branch.to_string(),
            };
        }
        Identity::Unknown {
            reasons: vec![format!(
                "'{branch}' has no state record and no [branch] declaration"
            )],
        }
    }

    /// Parent derivation: group/free = recorded parent; fixed = the unique
    /// `[branch]` section listing it bare (no lister = root, no parent).
    pub fn parent_of(&self, id: &Identity) -> Option<(String, &'static str)> {
        match id {
            Identity::GroupMember { parent, .. } | Identity::Free { parent, .. } => {
                Some((parent.clone(), "recorded"))
            }
            Identity::Fixed { branch } => self
                .cfg
                .bare_parent_sections(branch)
                .first()
                .map(|s| (s.name.clone(), "rules-derived")),
            Identity::Unknown { .. } => None,
        }
    }

    /// Naming constraint check for a group; `Err((why, key))` cites the
    /// violated key (`name-allow` or `name-deny`).
    fn naming_ok(&self, group: &str, name: &str) -> Result<(), (String, &'static str)> {
        let allow = self.cfg.name_allow(group);
        if !allow.is_empty() && !allow.iter().any(|p| crate::rules::glob_match(p, name)) {
            return Err((
                format!("'{name}' does not match name-allow of group '{group}'"),
                "name-allow",
            ));
        }
        for p in self.cfg.name_deny(group) {
            if crate::rules::glob_match(&p, name) {
                return Err((
                    format!("'{name}' matches name-deny pattern '{p}' of group '{group}'"),
                    "name-deny",
                ));
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------- plans ----

    /// `plan_new` minus everything that needs the name. Split out so the two
    /// callers cannot drift: `plan_new` runs it first, and `help` asks it
    /// whether `new` is possible at all.
    pub fn gate_new(&self) -> Decision<NewGate> {
        let id = self.current_identity();
        let (sec_kind, sec_name, parent) = match &id {
            Identity::Unknown { reasons } => {
                let mut rs = vec![
                    "current worktree is unmanaged — cannot be a parent (fail closed)".to_string(),
                ];
                rs.extend(reasons.clone());
                return refuse("new", "new".to_string(), rs);
            }
            Identity::Free { branch, .. } => {
                return refuse(
                    "new",
                    format!("new from '{branch}'"),
                    vec![format!(
                        "'{branch}' is a free branch — free branches cannot have children (fail closed)"
                    )],
                );
            }
            Identity::Fixed { branch } => (SectionKind::Branch, branch.clone(), branch.clone()),
            Identity::GroupMember { branch, group, .. } => {
                (SectionKind::Group, group.clone(), branch.clone())
            }
        };
        let children = self.cfg.children_of(sec_kind, &sec_name);
        if children.is_empty() {
            return refuse(
                "new",
                format!("new from '{parent}'"),
                vec![format!(
                    "{} declares no children — nothing may be created here (fail closed)",
                    sec_kind.header(&sec_name)
                )],
            );
        }
        Ok(NewGate {
            parent,
            sec_kind,
            sec_name,
            groups: children
                .iter()
                .filter_map(|c| match c {
                    Child::GroupRef(g) => Some(GroupRule {
                        group: g.clone(),
                        allow: self.cfg.name_allow(g),
                        deny: self.cfg.name_deny(g),
                    }),
                    _ => None,
                })
                .collect(),
            bares: children
                .iter()
                .filter_map(|c| match c {
                    Child::Bare(b) => Some(b.clone()),
                    _ => None,
                })
                .collect(),
            star: children.iter().any(|c| matches!(c, Child::Star)),
        })
    }

    pub fn plan_new(&self, name: &str, group_opt: Option<&str>) -> Decision<NewPlan> {
        // The gate's subject has no name in it (it judges without one); put it
        // back so the refusal reads as it always has.
        let g = self.gate_new().map_err(|mut r| {
            r.subject = r.subject.replacen("new", &format!("new '{name}'"), 1);
            r
        })?;
        let (parent, sec_kind, sec_name, star) =
            (g.parent.clone(), g.sec_kind, g.sec_name.clone(), g.star);
        let subject = format!("new '{name}' from '{parent}'");
        if self.world.branches.contains(name) {
            return refuse(
                "new",
                subject,
                vec![
                    format!("branch '{name}' already exists"),
                    format!("to give it a worktree instead: wtree open {name}"),
                ],
            );
        }
        let bares: Vec<&str> = g.bares.iter().map(String::as_str).collect();
        let groups: Vec<&str> = g.groups.iter().map(|r| r.group.as_str()).collect();

        // A declared fixed name may only be created when listed bare; neither
        // a group nor '*' may produce it (name reservation).
        if self.cfg.section(SectionKind::Branch, name).is_some() {
            if bares.contains(&name) {
                return Ok(NewPlan::Fixed {
                    name: name.to_string(),
                    parent,
                });
            }
            let mut rs = vec![format!(
                "'{name}' is a declared fixed branch name — cannot be created as a group/free branch (name reservation)"
            )];
            if star {
                rs.push("'*' excludes declared fixed branch names".to_string());
            }
            rs.push(format!(
                "to allow: list '{name}' bare in children of {}",
                sec_kind.header(&sec_name)
            ));
            rs.push(self.rule(sec_kind, &sec_name, "children"));
            return refuse("new", subject, rs);
        }

        // Group resolution, DESIGN's three ambiguity rules. '*' is a last
        // fallback, only when there are zero candidates and no --group.
        let mut candidates = Vec::new();
        let mut rejected = Vec::new();
        for g in &groups {
            match self.naming_ok(g, name) {
                Ok(()) => candidates.push(*g),
                Err(why) => rejected.push((*g, why)),
            }
        }
        if let Some(want) = group_opt {
            if !groups.contains(&want) {
                return refuse(
                    "new",
                    subject,
                    vec![
                        format!(
                            "--group {want}: not in children of {}",
                            sec_kind.header(&sec_name)
                        ),
                        self.rule(sec_kind, &sec_name, "children"),
                    ],
                );
            }
            if !candidates.contains(&want) {
                let (why, key) = rejected
                    .iter()
                    .find(|(g, _)| *g == want)
                    .map(|(_, w)| w.clone())
                    .expect("a listed group is either candidate or rejected");
                return refuse(
                    "new",
                    subject,
                    vec![
                        format!(
                            "--group narrows candidates but does not override naming rules: {why}"
                        ),
                        self.rule(SectionKind::Group, want, key),
                    ],
                );
            }
            return Ok(NewPlan::GroupMember {
                name: name.to_string(),
                group: want.to_string(),
                parent,
            });
        }
        match candidates.len() {
            1 => Ok(NewPlan::GroupMember {
                name: name.to_string(),
                group: candidates[0].to_string(),
                parent,
            }),
            0 if star => Ok(NewPlan::Free {
                name: name.to_string(),
                parent,
            }),
            0 => {
                let mut rs: Vec<String> = rejected
                    .iter()
                    .map(|(g, (why, key))| {
                        format!(
                            "group:{g} — {why}\n{}",
                            self.rule(SectionKind::Group, g, key)
                        )
                    })
                    .collect();
                if rs.is_empty() {
                    rs.push("no candidate groups in children".to_string());
                }
                rs.push(self.rule(sec_kind, &sec_name, "children"));
                refuse("new", subject, rs)
            }
            _ => refuse(
                "new",
                subject,
                vec![
                    format!(
                        "ambiguous: name matches multiple candidate groups: {}",
                        candidates.join(", ")
                    ),
                    "narrow with --group or split the groups by naming rules".to_string(),
                ],
            ),
        }
    }

    /// `open` gives an existing branch a worktree. It claims no relationship —
    /// the branch keeps whatever identity it already has and no record is
    /// written — so the only questions are whether the branch exists and
    /// whether it is free to check out.
    pub fn plan_open(&self, branch: &str) -> Decision<OpenPlan> {
        let subject = format!("open '{branch}'");
        if !self.world.branches.contains(branch) {
            return refuse(
                "open",
                subject,
                vec![
                    format!("branch '{branch}' does not exist"),
                    format!("to create it here: wtree new {branch}"),
                ],
            );
        }
        if let Some(wt) = self
            .world
            .facts
            .iter()
            .find(|f| f.head.as_deref() == Some(branch))
        {
            return refuse(
                "open",
                subject,
                vec![format!(
                    "'{branch}' is already checked out at {}",
                    wt.path.display()
                )],
            );
        }
        Ok(OpenPlan {
            branch: branch.to_string(),
            fixed: matches!(self.branch_identity(branch), Identity::Fixed { .. }),
        })
    }

    /// `close` removes this worktree and keeps the branch, so nothing is
    /// destroyed and `destroyable` does not apply. The one question it asks is
    /// whether the node leaves the tree along with the worktree: a fixed
    /// branch's identity is its rules declaration and outlives any checkout,
    /// while a group/free branch's identity is the record inside this
    /// worktree — losing it would leave that branch's children with an
    /// unmanaged parent, which is where policy silently stops applying.
    /// The verbs that would get past policy here, in the order the menu shows
    /// them. Each one is decided by the same gate its verb runs first, so the
    /// menu cannot drift from the verbs — see DESIGN, "help".
    pub fn affordances(&self) -> Vec<Affordance> {
        let mut out = Vec::new();
        if let Ok(g) = self.gate_new() {
            out.push(Affordance::New(g));
        }
        let candidates = self.open_candidates();
        if !candidates.is_empty() {
            out.push(Affordance::Open(candidates));
        }
        let merge = self.gate_merge().ok();
        if let Some(g) = merge.clone() {
            out.push(Affordance::Merge(g));
        }
        if let Ok(p) = self.plan_sync() {
            out.push(Affordance::Sync(p));
        }
        let destroy = self.gate_destroy().ok();
        // land needs both halves, and unlike destroy it cannot be talked into
        // the remaining two: a broken parent link would need --force, and
        // uncommitted work is refused outright (land would have to leave it).
        if let (Some(m), Some(d)) = (merge, &destroy)
            && d.missing_parent.is_none()
            && !self.current().dirty
        {
            out.push(Affordance::Land(m));
        }
        if let Ok(p) = self.gate_close() {
            out.push(Affordance::Close(p));
        }
        if let Some(d) = destroy {
            out.push(Affordance::Destroy(d));
        }
        if self.gate_adopt().is_ok() {
            out.push(Affordance::Adopt);
        }
        out
    }

    /// Every branch `open` would take right now. `open` asks "what can be
    /// opened?" rather than "may I open this?", so instead of splitting a gate
    /// out of `plan_open` this runs the real thing over each branch — there is
    /// no second copy of the rule to fall out of step.
    pub fn open_candidates(&self) -> Vec<OpenCandidate> {
        self.world
            .branches
            .iter()
            .filter_map(|b| {
                self.plan_open(b).ok().map(|p| OpenCandidate {
                    branch: p.branch,
                    fixed: p.fixed,
                })
            })
            .collect()
    }

    /// `plan_close` minus the confirmation key. The returned plan's `dirty` is
    /// what the key would have to clear.
    pub fn gate_close(&self) -> Decision<ClosePlan> {
        let cur = self.current();
        let branch = cur.head.clone();
        let subject = match &branch {
            Some(b) => format!("'{b}'"),
            None => "(detached HEAD)".to_string(),
        };
        if self.is_primary() {
            return refuse("close", subject, primary_worktree("close"));
        }

        let id = self.current_identity();
        let drops_record = matches!(id, Identity::GroupMember { .. } | Identity::Free { .. });
        if drops_record {
            let branch = branch.clone().expect("a live record matches HEAD");
            // Children whose own record has drifted are already unmanaged, so
            // closing takes nothing further from them.
            let orphaned: Vec<String> = self
                .children_records(&branch)
                .into_iter()
                .filter(|(wt, s)| wt.head.as_deref() == Some(s.branch.as_str()))
                .map(|(_, s)| format!("'{}'", s.branch))
                .collect();
            if !orphaned.is_empty() {
                return refuse(
                    "close",
                    subject,
                    vec![
                        format!(
                            "'{branch}' is recorded in this worktree only, so closing it drops '{branch}' out of the tree and orphans its children:\n{}",
                            orphaned.join("\n")
                        ),
                        "land/destroy them first, or re-parent them with: wtree adopt".to_string(),
                    ],
                );
            }
        }

        Ok(ClosePlan {
            branch,
            dirty: cur.dirty,
            drops_record,
        })
    }

    pub fn plan_close(&self, key: Option<&str>) -> Decision<ClosePlan> {
        let plan = self.gate_close()?;
        let cur = self.current();
        let subject = match &plan.branch {
            Some(b) => format!("'{b}'"),
            None => "(detached HEAD)".to_string(),
        };
        // Work loss: the branch survives, so unreflected commits are not lost
        // and only uncommitted work is at stake — under destroy's key.
        if cur.dirty {
            let expected = cur.confirmation_key.as_deref();
            if expected.is_none() || key != expected {
                let mut rs = vec![
                    "uncommitted changes go with the worktree (the branch keeps its commits)"
                        .to_string(),
                ];
                match expected {
                    Some(k) => rs.push(format!("confirmation key required: wtree close --key {k}")),
                    None => rs.push("confirmation key could not be computed".to_string()),
                }
                return refuse("close", subject, rs);
            }
        }
        Ok(plan)
    }

    /// `plan_merge` minus the mode flag: who the target is, whether it is still
    /// there and still managed, and what it accepts.
    pub fn gate_merge(&self) -> Decision<MergeGate> {
        let id = self.current_identity();
        let branch = match &id {
            Identity::Unknown { reasons } => {
                return refuse(
                    "merge",
                    format!(
                        "'{}' is unmanaged (fail closed)",
                        self.current_head_or_detached()
                    ),
                    reasons.clone(),
                );
            }
            other => other
                .branch()
                .expect("known identity has a branch")
                .to_string(),
        };
        let Some((target, how)) = self.parent_of(&id) else {
            return refuse(
                "merge",
                format!("'{branch}'"),
                vec![
                    "no merge target: not listed bare in any [branch] children (root branch?)"
                        .to_string(),
                ],
            );
        };
        let subject = format!("'{branch}' -> '{target}'");
        if !self.world.branches.contains(&target) {
            return refuse(
                "merge",
                subject,
                vec![
                    format!("{how} parent '{target}' no longer exists — it was destroyed"),
                    "re-adopt onto a live parent with: wtree adopt".to_string(),
                ],
            );
        }
        if let Identity::Unknown { reasons } = self.branch_identity(&target) {
            return refuse("merge", subject, unmanaged_parent(&target, &reasons));
        }
        let (modes, cite) = self.target_merge_modes(&target);
        Ok(MergeGate {
            source: branch,
            target,
            modes,
            cite,
        })
    }

    pub fn plan_merge(&self, mode_flag: Option<MergeMode>) -> Decision<MergePlan> {
        let g = self.gate_merge()?;
        let MergeGate {
            source,
            target,
            modes: allowed,
            cite,
        } = g;
        let subject = format!("'{source}' -> '{target}'");
        let mode = match mode_flag {
            Some(m) if allowed.contains(&m) => m,
            Some(m) => {
                let mut rs = vec![format!(
                    "'{target}': accepts {} merges only (requested: --{})",
                    mode_list(&allowed),
                    m.as_str()
                )];
                rs.extend(cite);
                return refuse("merge", subject, rs);
            }
            None if allowed.len() == 1 => allowed[0],
            None => {
                let mut rs = vec![format!(
                    "'{target}' allows multiple merge modes ({}) — pick one: --squash | --rebase | --no-ff | --ff",
                    mode_list(&allowed)
                )];
                rs.extend(cite);
                return refuse("merge", subject, rs);
            }
        };
        Ok(MergePlan {
            source,
            target,
            mode,
        })
    }

    /// Merge-mode set of a target branch: fixed -> its `[branch]` section,
    /// group member -> its `[group]` section, free/undeclared -> every mode.
    pub fn target_merge_modes(&self, target: &str) -> (Vec<MergeMode>, Option<String>) {
        let (kind, name) = match self.branch_identity(target) {
            Identity::Fixed { branch } => (SectionKind::Branch, branch),
            Identity::GroupMember { group, .. } => (SectionKind::Group, group),
            _ => return (ALL_MODES.to_vec(), None),
        };
        match self.cfg.merge_modes(kind, &name) {
            Some(modes) if !modes.is_empty() => {
                let cite = self.rule(kind, &name, "merge-mode");
                (modes, Some(cite))
            }
            _ => (ALL_MODES.to_vec(), None),
        }
    }

    pub fn plan_sync(&self) -> Decision<SyncPlan> {
        let id = self.current_identity();
        let branch = match &id {
            Identity::Unknown { reasons } => {
                return refuse(
                    "sync",
                    format!(
                        "'{}' is unmanaged (fail closed)",
                        self.current_head_or_detached()
                    ),
                    reasons.clone(),
                );
            }
            other => other
                .branch()
                .expect("known identity has a branch")
                .to_string(),
        };
        let Some((parent, _how)) = self.parent_of(&id) else {
            return refuse(
                "sync",
                format!("'{branch}'"),
                vec!["no parent to sync from (root branch?)".to_string()],
            );
        };
        if !self.world.branches.contains(&parent) {
            return refuse(
                "sync",
                format!("'{branch}'"),
                vec![
                    format!("parent '{parent}' no longer exists — it was destroyed"),
                    "re-adopt onto a live parent with: wtree adopt".to_string(),
                ],
            );
        }
        if let Identity::Unknown { reasons } = self.branch_identity(&parent) {
            return refuse(
                "sync",
                format!("'{branch}' <- '{parent}'"),
                unmanaged_parent(&parent, &reasons),
            );
        }
        Ok(SyncPlan { branch, parent })
    }

    /// `plan_destroy` up to the point where the flags decide. The policy layer
    /// and the child scan refuse outright here; the broken parent link and the
    /// work-loss risk come back as facts, because `--force` and `--key` are
    /// what settle those.
    pub fn gate_destroy(&self) -> Decision<DestroyGate> {
        if self.is_primary() {
            return refuse(
                "destroy",
                format!("'{}'", self.current_head_or_detached()),
                primary_worktree("destroy"),
            );
        }
        let id = self.current_identity();
        let branch = match &id {
            Identity::Unknown { reasons } => {
                return refuse(
                    "destroy",
                    format!(
                        "'{}' is unmanaged (fail closed)",
                        self.current_head_or_detached()
                    ),
                    reasons.clone(),
                );
            }
            other => other
                .branch()
                .expect("known identity has a branch")
                .to_string(),
        };
        let subject = format!("'{branch}'");

        // Layer 1 — policy. `destroyable` applies to fixed branches only;
        // groups and free branches have no policy layer.
        if matches!(id, Identity::Fixed { .. }) && !self.cfg.destroyable(&branch) {
            return refuse(
                "destroy",
                subject,
                vec![
                    format!(
                        "'{branch}': destroyable = false — refused unconditionally, --force cannot override"
                    ),
                    self.rule(SectionKind::Branch, &branch, "destroyable"),
                ],
            );
        }

        // Recursive child scan + ephemeral cascade. A live child always
        // blocks; --force cannot override (prevents orphan subtrees).
        let mut visited = vec![branch.clone()];
        let (blockers, cascade) = self.subtree_closable(&branch, &mut visited);
        if !blockers.is_empty() {
            return refuse(
                "destroy",
                format!("'{branch}' — live children (refused as a whole, --force cannot override)"),
                blockers,
            );
        }

        // Layer 2 — relation. Missing parent blocks, --force passes (the
        // upstream may be managed outside wtree).
        let missing_parent = match self.parent_of(&id) {
            None => Some("no recorded/derivable parent".to_string()),
            Some((p, how)) if !self.world.branches.contains(&p) => {
                Some(format!("{how} parent '{p}' no longer exists"))
            }
            Some(_) => None,
        };
        let cur = self.current();
        Ok(DestroyGate {
            branch,
            cascade,
            missing_parent,
            needs_key: cur.dirty || cur.unreflected,
        })
    }

    pub fn plan_destroy(&self, force: bool, key: Option<&str>) -> Decision<DestroyPlan> {
        let g = self.gate_destroy()?;
        let branch = g.branch;
        let subject = format!("'{branch}'");
        if let Some(why) = g.missing_parent {
            if !force {
                return refuse(
                    "destroy",
                    subject,
                    vec![
                        why,
                        "the upstream may be managed outside wtree — verify, then pass --force"
                            .to_string(),
                    ],
                );
            }
        }

        // Layer 3 — work loss. --force does not apply here: "destroy without
        // a parent" never implies "throw the work away".
        let cur = self.current();
        if g.needs_key {
            let expected = cur.confirmation_key.as_deref();
            if expected.is_none() || key != expected {
                let mut why = Vec::new();
                if cur.dirty {
                    why.push("uncommitted changes");
                }
                if cur.unreflected {
                    why.push("commits not reflected in parent");
                }
                let mut rs = vec![format!("work-loss risk: {}", why.join(", "))];
                match expected {
                    Some(k) => rs.push(format!(
                        "confirmation key required: wtree destroy --key {k}   (--force cannot override)"
                    )),
                    None => rs.push("confirmation key could not be computed".to_string()),
                }
                return refuse("destroy", subject, rs);
            }
        }

        Ok(DestroyPlan {
            branch,
            cascade: g.cascade,
        })
    }

    /// Worktrees whose state record names `branch` as its parent — the
    /// group/free children, found by enumeration because no child links are
    /// stored (DESIGN). A corrupt record names no parent, so it is not a child
    /// of anything.
    fn children_records<'s>(&'s self, branch: &str) -> Vec<(&'s WtFact, &'s State)> {
        self.world
            .facts
            .iter()
            .filter_map(|wt| match &wt.state {
                StateRead::Valid(s) if s.parent == branch => Some((wt, s)),
                _ => None,
            })
            .collect()
    }

    /// Recursive child scan for destroy/land: returns (blockers, leaf-first
    /// cascade order). Managed children are worktree records whose parent is
    /// `branch`, plus declared fixed branches whose rules parent is `branch`
    /// and which exist in git.
    fn subtree_closable(
        &self,
        branch: &str,
        visited: &mut Vec<String>,
    ) -> (Vec<String>, Vec<String>) {
        let mut blockers = Vec::new();
        let mut order = Vec::new();
        // Fixed children: rules x branch existence.
        for b in self.cfg.branch_names() {
            if b == branch || !self.world.branches.contains(b) {
                continue;
            }
            let parents = self.cfg.bare_parent_sections(b);
            if parents.len() == 1 && parents[0].name == branch {
                blockers.push(format!(
                    "'{b}' — fixed child branch exists; land/destroy it first (fixed branches never cascade)"
                ));
            }
        }
        // Group/free children: worktree enumeration + their records.
        for (wt, s) in self.children_records(branch) {
            if wt.head.as_deref() != Some(s.branch.as_str()) {
                blockers.push(format!(
                    "'{}' — child record does not match its worktree HEAD (unmanaged); resolve with adopt first",
                    s.branch
                ));
                continue;
            }
            match &s.kind {
                Kind::Free => blockers.push(format!(
                    "'{}' — free child; land/destroy it first (free branches never cascade)",
                    s.branch
                )),
                Kind::Group(g) if !self.cfg.ephemeral(g) => blockers.push(format!(
                    "'{}' — group:{g} is not ephemeral; land/destroy it first",
                    s.branch
                )),
                Kind::Group(_) => {
                    if visited.contains(&s.branch) {
                        blockers.push(format!(
                            "'{}' — cycle in recorded parent links (corrupt state)",
                            s.branch
                        ));
                        continue;
                    }
                    visited.push(s.branch.clone());
                    let (sub_blockers, sub_order) = self.subtree_closable(&s.branch, visited);
                    blockers.extend(sub_blockers);
                    order.extend(sub_order);
                    if wt.dirty {
                        blockers.push(format!("'{}' — uncommitted changes (dirty)", s.branch));
                    }
                    if wt.unreflected {
                        blockers.push(format!(
                            "'{}' — commits not reflected in its parent",
                            s.branch
                        ));
                    }
                    order.push(s.branch.clone());
                }
            }
        }
        (blockers, order)
    }

    /// What `adopt` can settle before it sees `--group`/`--free`/`--parent`:
    /// only whether this worktree could ever carry a record. Everything else
    /// depends on the parent it is given. Returns the branch to adopt.
    ///
    /// Note this stays Ok for an already-managed worktree — re-adopt is the
    /// way to correct a wrong group or parent, so `help` must keep offering it.
    pub fn gate_adopt(&self) -> Decision<String> {
        let Some(branch) = self.current().head.clone() else {
            return refuse(
                "adopt",
                "(detached HEAD)".to_string(),
                vec![
                    "cannot adopt a detached HEAD — check out the branch to adopt first"
                        .to_string(),
                ],
            );
        };
        // Name reservation applies to --group and --free alike: a declared
        // fixed name never carries a state record.
        if self.cfg.section(SectionKind::Branch, &branch).is_some() {
            return refuse(
                "adopt",
                format!("'{branch}'"),
                vec![format!(
                    "'{branch}' is a declared fixed branch name (name reservation) — fixed branches carry no state and are never adopted"
                )],
            );
        }
        Ok(branch)
    }

    pub fn plan_adopt(
        &self,
        group_opt: Option<&str>,
        free: bool,
        parent: &str,
    ) -> Decision<AdoptPlan> {
        let branch = self.gate_adopt().map_err(|mut r| {
            // The gate judges without a parent; the refusal names it as ever.
            if r.subject.starts_with('\'') {
                r.subject = format!("{} (parent '{parent}')", r.subject);
            }
            r
        })?;
        let cur = self.current();
        let subject = format!("'{branch}' (parent '{parent}')");
        let group_req = match (group_opt, free) {
            (Some(_), true) => {
                return refuse(
                    "adopt",
                    subject,
                    vec!["--group and --free are mutually exclusive".to_string()],
                );
            }
            (None, false) => {
                return refuse(
                    "adopt",
                    subject,
                    vec!["one of --group <X> or --free is required".to_string()],
                );
            }
            (g, _) => g,
        };
        // Adoptable targets: no record, mismatched record, or a valid record
        // (re-adopt). A replaced record is surfaced in the plan, never dropped
        // silently.
        let previous = match &cur.state {
            StateRead::Valid(s) => Some(s.summary()),
            StateRead::Invalid { reason } => Some(format!("(corrupt record: {reason})")),
            StateRead::Missing => None,
        };
        if !self.world.branches.contains(parent) {
            return refuse(
                "adopt",
                subject,
                vec![format!("parent branch '{parent}' does not exist")],
            );
        }
        if parent == branch {
            return refuse(
                "adopt",
                subject,
                vec![format!("'{branch}' cannot be its own parent")],
            );
        }
        let pid = self.branch_identity(parent);
        let (psec_kind, psec_name) = match &pid {
            Identity::Unknown { reasons } => {
                let mut rs = vec![format!(
                    "parent '{parent}' is unmanaged — cannot be a parent (fail closed)"
                )];
                rs.extend(reasons.clone());
                return refuse("adopt", subject, rs);
            }
            Identity::Free { .. } => {
                return refuse(
                    "adopt",
                    subject,
                    vec![format!(
                        "'{parent}' is a free branch — free branches cannot have children (fail closed)"
                    )],
                );
            }
            Identity::Fixed { branch } => (SectionKind::Branch, branch.clone()),
            Identity::GroupMember { group, .. } => (SectionKind::Group, group.clone()),
        };
        let children = self.cfg.children_of(psec_kind, &psec_name);
        let kind = match group_req {
            None => {
                if !children.iter().any(|c| matches!(c, Child::Star)) {
                    return refuse(
                        "adopt",
                        subject,
                        vec![
                            format!(
                                "--free: children of {} contains no '*'",
                                psec_kind.header(&psec_name)
                            ),
                            self.rule(psec_kind, &psec_name, "children"),
                        ],
                    );
                }
                Kind::Free
            }
            Some(g) => {
                if !children
                    .iter()
                    .any(|c| matches!(c, Child::GroupRef(x) if x == g))
                {
                    return refuse(
                        "adopt",
                        subject,
                        vec![
                            format!(
                                "--group {g}: not in children of {}",
                                psec_kind.header(&psec_name)
                            ),
                            self.rule(psec_kind, &psec_name, "children"),
                        ],
                    );
                }
                if let Err((why, key)) = self.naming_ok(g, &branch) {
                    return refuse(
                        "adopt",
                        subject,
                        vec![why, self.rule(SectionKind::Group, g, key)],
                    );
                }
                Kind::Group(g.to_string())
            }
        };
        // Orphan-history gate: without a merge-base every later merge would
        // fail, so refuse at adoption time.
        if !self.world.shares_base_with_head.contains(parent) {
            return refuse(
                "adopt",
                subject,
                vec![format!(
                    "no common ancestor (merge-base) with parent '{parent}' — an orphan history could never merge back"
                )],
            );
        }
        Ok(AdoptPlan {
            branch,
            kind,
            parent: parent.to_string(),
            previous,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo;
    use crate::rules;
    use crate::testutil::Fixture;
    use std::path::Path;

    fn cfg(text: &str) -> Rules {
        let l = rules::load_str(text, ".git/wtree/rules");
        assert!(l.errors.is_empty(), "rules errors: {:?}", l.errors);
        l.rules
    }

    fn world(cwd: &Path, cfg: &Rules) -> World {
        repo::gather(cwd, cfg).unwrap()
    }

    /// Assert refusal and require `needles` to appear in its rendering.
    fn refused<T: fmt::Debug>(d: Decision<T>, needles: &[&str]) -> Refusal {
        let e = match d {
            Err(e) => e,
            Ok(v) => panic!("expected refusal, got {v:?}"),
        };
        let text = e.to_string();
        for n in needles {
            assert!(text.contains(n), "missing '{n}' in:\n{text}");
        }
        e
    }

    fn member(fx: &Fixture, branch: &str, group: &str, parent: &str) -> std::path::PathBuf {
        let p = fx.add_worktree(branch, parent);
        fx.write_state(&p, branch, &format!("group:{group}"), parent);
        p
    }

    // ------------------------------------------------ identity + parents ----

    #[test]
    fn raw_switch_and_rename_make_identity_unknown() {
        let fx = Fixture::new();
        let c = cfg("[main]\nchildren = group:feat\n\n[group:feat]\nname-allow = feature/*\n");
        let wt = member(&fx, "feature/a", "feat", "main");

        // sanity: managed before the raw intervention
        let w = world(&wt, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        assert!(matches!(
            ctx.current_identity(),
            Identity::GroupMember { .. }
        ));

        // raw switch: HEAD moves, record stays
        fx.git(&wt, &["switch", "-q", "-c", "oops"]);
        let w = world(&wt, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        assert!(matches!(ctx.current_identity(), Identity::Unknown { .. }));
        refused(
            ctx.plan_merge(None),
            &["unmanaged", "raw switch/rename", "adopt"],
        );
        refused(ctx.plan_new("feature/b", None), &["cannot be a parent"]);
        fx.git(&wt, &["switch", "-q", "feature/a"]);

        // raw rename: record still says the old name
        let wt2 = member(&fx, "feature/b", "feat", "main");
        fx.git(&wt2, &["branch", "-m", "feature/b2"]);
        let w = world(&wt2, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        refused(
            ctx.plan_sync(),
            &["recorded branch 'feature/b'", "HEAD 'feature/b2'"],
        );
    }

    #[test]
    fn corrupt_state_and_vanished_group_are_unknown() {
        let fx = Fixture::new();
        let wt = fx.add_worktree("feature/a", "main");
        let private = repo::private_git_dir(&wt).unwrap();
        std::fs::write(private.join(crate::state::STATE_FILE), "version = 9\n").unwrap();
        let c = cfg("[main]\nchildren = group:feat\n\n[group:feat]\n");
        let w = world(&wt, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        refused(ctx.plan_sync(), &["corrupt", "unknown version '9'"]);

        // recorded group no longer declared in the rules
        fx.write_state(&wt, "feature/a", "group:gone", "main");
        let w = world(&wt, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        refused(
            ctx.plan_sync(),
            &["recorded group 'gone' is no longer declared"],
        );
    }

    #[test]
    fn parent_derivation_recorded_rules_and_root() {
        let fx = Fixture::new();
        let c = cfg(
            "[main]\nchildren = dev, group:feat\nmerge-mode = squash\n\n[dev]\n\n[group:feat]\nname-allow = feature/*\n",
        );
        // recorded parent (group member)
        let m = member(&fx, "feature/a", "feat", "main");
        let w = world(&m, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        assert_eq!(
            ctx.plan_sync().unwrap(),
            SyncPlan {
                branch: "feature/a".into(),
                parent: "main".into()
            }
        );
        // rules-derived parent (fixed branch, no state record)
        let d = fx.add_worktree("dev", "main");
        let w = world(&d, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        assert_eq!(
            ctx.plan_sync().unwrap(),
            SyncPlan {
                branch: "dev".into(),
                parent: "main".into()
            }
        );
        assert_eq!(
            ctx.plan_merge(None).unwrap(),
            MergePlan {
                source: "dev".into(),
                target: "main".into(),
                mode: MergeMode::Squash
            }
        );
        // root: fixed branch listed bare nowhere
        let w = world(&fx.repo, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        refused(ctx.plan_sync(), &["no parent to sync from (root branch?)"]);
        refused(ctx.plan_merge(None), &["no merge target"]);
    }

    #[test]
    fn state_record_wins_over_branch_declaration() {
        // Grandfather rule: a group-recorded branch keeps its group identity
        // even when a [branch] of the same name is later declared.
        let fx = Fixture::new();
        let c = cfg("[main]\nchildren = dev, group:feat\n\n[dev]\n\n[group:feat]\n");
        let wt = fx.add_worktree("dev", "main");
        fx.write_state(&wt, "dev", "group:feat", "main");
        let w = world(&wt, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        assert!(matches!(
            ctx.current_identity(),
            Identity::GroupMember { ref group, .. } if group == "feat"
        ));
        assert_eq!(ctx.plan_sync().unwrap().parent, "main");
    }

    // ---------------------------------------------------------- plan_new ----

    #[test]
    fn new_group_resolution_three_rules() {
        let fx = Fixture::new();
        let base =
            "[group:a]\nname-allow = feature/*\n\n[group:b]\nname-allow = feature/*, bug/*\n";
        let star_cfg = cfg(&format!("[main]\nchildren = group:a, group:b, *\n\n{base}"));
        let w = world(&fx.repo, &star_cfg);
        let ctx = Ctx {
            world: &w,
            cfg: &star_cfg,
            label: ".git/wtree/rules",
        };
        // exactly one candidate -> that group ('*' ignored)
        assert_eq!(
            ctx.plan_new("bug/x", None).unwrap(),
            NewPlan::GroupMember {
                name: "bug/x".into(),
                group: "b".into(),
                parent: "main".into()
            }
        );
        // two candidates -> ambiguous, refuse ('*' must not swallow it)
        refused(ctx.plan_new("feature/x", None), &["ambiguous", "a, b"]);
        // --group narrows the ambiguity
        assert_eq!(
            ctx.plan_new("feature/x", Some("a")).unwrap(),
            NewPlan::GroupMember {
                name: "feature/x".into(),
                group: "a".into(),
                parent: "main".into()
            }
        );
        // zero candidates + '*' -> free branch
        assert_eq!(
            ctx.plan_new("other/x", None).unwrap(),
            NewPlan::Free {
                name: "other/x".into(),
                parent: "main".into()
            }
        );
        // --group does not override naming rules, and suppresses the '*' fallback
        refused(
            ctx.plan_new("other/x", Some("a")),
            &["does not override naming rules", "(.git/wtree/rules:"],
        );
        // --group of a group not listed in children
        refused(
            ctx.plan_new("feature/x", Some("ghost")),
            &["--group ghost: not in children"],
        );

        // zero candidates, no '*' -> refuse with per-group reasons
        let nostar_cfg = cfg(&format!("[main]\nchildren = group:a, group:b\n\n{base}"));
        let w = world(&fx.repo, &nostar_cfg);
        let ctx = Ctx {
            world: &w,
            cfg: &nostar_cfg,
            label: ".git/wtree/rules",
        };
        refused(
            ctx.plan_new("other/x", None),
            &["group:a", "does not match name-allow", "rule: name-allow"],
        );
    }

    #[test]
    fn new_fixed_name_reservation_and_bare_creation() {
        let fx = Fixture::new();
        // declared but not listed bare: reserved, neither group nor '*'
        let c = cfg("[main]\nchildren = group:g, *\n\n[dev]\n\n[group:g]\n");
        let w = world(&fx.repo, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        refused(
            ctx.plan_new("dev", None),
            &[
                "name reservation",
                "'*' excludes declared fixed branch names",
            ],
        );
        refused(ctx.plan_new("dev", Some("g")), &["name reservation"]);
        // listed bare: created as fixed, without a state record
        let c2 = cfg("[main]\nchildren = dev, group:g, *\n\n[dev]\n\n[group:g]\n");
        let w = world(&fx.repo, &c2);
        let ctx = Ctx {
            world: &w,
            cfg: &c2,
            label: ".git/wtree/rules",
        };
        assert_eq!(
            ctx.plan_new("dev", None).unwrap(),
            NewPlan::Fixed {
                name: "dev".into(),
                parent: "main".into()
            }
        );
        // an existing branch name is always refused
        refused(ctx.plan_new("main", None), &["already exists"]);
    }

    #[test]
    fn new_refused_without_children_and_from_free() {
        let fx = Fixture::new();
        let c = cfg("[main]\nchildren = *\n");
        // fail closed: no children declared
        let c0 = cfg("[main]\n");
        let w = world(&fx.repo, &c0);
        let ctx = Ctx {
            world: &w,
            cfg: &c0,
            label: ".git/wtree/rules",
        };
        refused(
            ctx.plan_new("x", None),
            &["declares no children", "fail closed"],
        );
        // free branches have no children
        let f = fx.add_worktree("loose", "main");
        fx.write_state(&f, "loose", "free", "main");
        let w = world(&f, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        refused(
            ctx.plan_new("x", None),
            &["free branches cannot have children"],
        );
    }

    // -------------------------------------------------------- plan_merge ----

    #[test]
    fn merge_mode_set_rules() {
        let fx = Fixture::new();
        let d = fx.add_worktree("dev", "main");
        // two allowed modes: flag is mandatory
        let c = cfg("[main]\nchildren = dev\nmerge-mode = squash, no-ff\n\n[dev]\n");
        let w = world(&d, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        refused(
            ctx.plan_merge(None),
            &[
                "multiple merge modes",
                "squash, no-ff",
                "(.git/wtree/rules:3)",
            ],
        );
        assert_eq!(
            ctx.plan_merge(Some(MergeMode::NoFf)).unwrap().mode,
            MergeMode::NoFf
        );
        // flag outside the set
        refused(
            ctx.plan_merge(Some(MergeMode::Rebase)),
            &[
                "accepts squash, no-ff merges only (requested: --rebase)",
                "rule: merge-mode",
            ],
        );
        // single allowed mode: flag optional
        let c1 = cfg("[main]\nchildren = dev\nmerge-mode = squash\n\n[dev]\n");
        let w = world(&d, &c1);
        let ctx = Ctx {
            world: &w,
            cfg: &c1,
            label: ".git/wtree/rules",
        };
        assert_eq!(ctx.plan_merge(None).unwrap().mode, MergeMode::Squash);
        refused(
            ctx.plan_merge(Some(MergeMode::Ff)),
            &["accepts squash merges only"],
        );
        // unset: every mode allowed, so the flag is mandatory
        let c2 = cfg("[main]\nchildren = dev\n\n[dev]\n");
        let w = world(&d, &c2);
        let ctx = Ctx {
            world: &w,
            cfg: &c2,
            label: ".git/wtree/rules",
        };
        refused(ctx.plan_merge(None), &["squash, rebase, no-ff, ff"]);
        assert_eq!(
            ctx.plan_merge(Some(MergeMode::Ff)).unwrap().mode,
            MergeMode::Ff
        );
    }

    #[test]
    fn merge_into_group_member_uses_group_merge_mode() {
        let fx = Fixture::new();
        let c = cfg(
            "[main]\nchildren = group:mid\n\n[group:mid]\nchildren = group:leaf\nmerge-mode = rebase\n\n[group:leaf]\n",
        );
        let m = member(&fx, "m1", "mid", "main");
        let l = member(&fx, "l1", "leaf", "m1");
        let _ = m;
        let w = world(&l, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        assert_eq!(
            ctx.plan_merge(None).unwrap(),
            MergePlan {
                source: "l1".into(),
                target: "m1".into(),
                mode: MergeMode::Rebase
            }
        );
        refused(
            ctx.plan_merge(Some(MergeMode::Squash)),
            &[
                "'m1': accepts rebase merges only",
                "rule: merge-mode = rebase",
            ],
        );
    }

    #[test]
    fn merge_refused_when_parent_vanished() {
        let fx = Fixture::new();
        let c = cfg("[main]\nchildren = group:feat\n\n[group:feat]\n");
        let wt = fx.add_worktree("feature/a", "main");
        fx.write_state(&wt, "feature/a", "group:feat", "ghost");
        let w = world(&wt, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        refused(ctx.plan_merge(None), &["'ghost' no longer exists", "adopt"]);
        refused(ctx.plan_sync(), &["'ghost' no longer exists"]);
    }

    #[test]
    fn drain_existing_members_merge_new_creation_refused() {
        let fx = Fixture::new();
        let before = cfg(
            "[main]\nchildren = group:feat\nmerge-mode = squash\n\n[group:feat]\nname-allow = feature/*\n",
        );
        let m = member(&fx, "feature/a", "feat", "main");
        // rules change: group removed from children (still declared)
        let after = cfg("[main]\nmerge-mode = squash\n\n[group:feat]\nname-allow = feature/*\n");
        let _ = before;
        // existing member still merges via its recorded parent
        let w = world(&m, &after);
        let ctx = Ctx {
            world: &w,
            cfg: &after,
            label: ".git/wtree/rules",
        };
        assert_eq!(ctx.plan_merge(None).unwrap().target, "main");
        // but nothing new can be created there
        let w = world(&fx.repo, &after);
        let ctx = Ctx {
            world: &w,
            cfg: &after,
            label: ".git/wtree/rules",
        };
        refused(ctx.plan_new("feature/b", None), &["declares no children"]);
    }

    // ------------------------------------------------------ plan_destroy ----

    #[test]
    fn destroy_policy_layer_is_absolute() {
        let fx = Fixture::new();
        // Judged from a linked worktree throughout: the primary one is refused
        // before any of this, so it cannot show the layer under test.
        let c = cfg("[main]\nchildren = mid\n\n[mid]\ndestroyable = false\n");
        let wt = fx.add_worktree("mid", "main");
        let w = world(&wt, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        refused(
            ctx.plan_destroy(true, None),
            &[
                "destroyable = false",
                "--force cannot override",
                "(.git/wtree/rules:5)",
            ],
        );
    }

    #[test]
    fn destroy_refuses_the_primary_worktree() {
        let fx = Fixture::new();
        let c = cfg("[main]\nchildren = group:feat\n\n[group:feat]\n");
        let w = world(&fx.repo, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        // Nothing about 'main' forbids it — it is the checkout git will not
        // remove, exactly as for close.
        refused(
            ctx.plan_destroy(true, None),
            &["primary worktree", "git cannot remove it"],
        );
        refused(ctx.plan_close(None), &["primary worktree"]);
    }

    #[test]
    fn destroy_blocked_by_non_ephemeral_and_fixed_children() {
        let fx = Fixture::new();
        // 'mid' is the branch under test, in a linked worktree; its children
        // hang off it.
        let mid = fx.add_worktree("mid", "main");
        // non-ephemeral group child
        let c = cfg("[main]\nchildren = mid\n\n[mid]\nchildren = group:feat\n\n[group:feat]\n");
        member(&fx, "wip", "feat", "mid");
        let w = world(&mid, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        refused(ctx.plan_destroy(true, None), &["'wip'", "not ephemeral"]);
        // fixed child (rules x branch existence, no worktree needed)
        let c2 = cfg("[main]\nchildren = mid\n\n[mid]\nchildren = dev\n\n[dev]\n");
        fx.git(&fx.repo, &["branch", "dev", "main"]);
        let w = world(&mid, &c2);
        let ctx = Ctx {
            world: &w,
            cfg: &c2,
            label: ".git/wtree/rules",
        };
        refused(ctx.plan_destroy(true, None), &["'dev'", "fixed child"]);
        // free child
        let c3 = cfg("[main]\nchildren = mid\n\n[mid]\nchildren = *\n");
        let f = fx.add_worktree("loose", "main");
        fx.write_state(&f, "loose", "free", "mid");
        let w = world(&mid, &c3);
        let ctx = Ctx {
            world: &w,
            cfg: &c3,
            label: ".git/wtree/rules",
        };
        refused(ctx.plan_destroy(true, None), &["'loose'", "free child"]);
    }

    #[test]
    fn destroy_ephemeral_cascade_leaf_first() {
        let fx = Fixture::new();
        let c = cfg("[main]\nchildren = mid\n\n[mid]\nchildren = group:eph\n\n\
             [group:eph]\nchildren = group:eph\nephemeral = true\n");
        let mid = fx.add_worktree("mid", "main");
        member(&fx, "wa", "eph", "mid");
        member(&fx, "wb", "eph", "wa");
        let wc = member(&fx, "wc", "eph", "wb");
        // all clean: collected leaf-first together with 'mid'
        let w = world(&mid, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        let plan = ctx.plan_destroy(true, None).unwrap();
        assert_eq!(plan.branch, "mid");
        assert_eq!(
            plan.cascade,
            vec!["wc".to_string(), "wb".into(), "wa".into()]
        );
        // one dirty leaf + one unreflected middle: whole refusal, per-branch reasons
        fx.make_dirty(&wc);
        let wb_path = fx.tmp.0.join("wtree-wb");
        fx.commit(&wb_path, "unreflected work");
        let w = world(&mid, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        refused(
            ctx.plan_destroy(true, None),
            &[
                "'wc' — uncommitted changes (dirty)",
                "'wb' — commits not reflected in its parent",
            ],
        );
    }

    #[test]
    fn destroy_confirmation_key_flow() {
        let fx = Fixture::new();
        let c = cfg("[main]\nchildren = group:feat\n\n[group:feat]\nname-allow = feature/*\n");
        let m = member(&fx, "feature/a", "feat", "main");
        fx.commit(&m, "work"); // commits not reflected in main
        let w = world(&m, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        let key = w.current().confirmation_key.clone().unwrap();
        refused(
            ctx.plan_destroy(false, None),
            &[
                "work-loss risk",
                "commits not reflected in parent",
                &format!("--key {key}"),
            ],
        );
        refused(
            ctx.plan_destroy(false, Some("zzzzz")),
            &["confirmation key required"],
        );
        // --force never substitutes for the key
        refused(ctx.plan_destroy(true, None), &["--force cannot override"]);
        // the right key passes
        assert_eq!(
            ctx.plan_destroy(false, Some(&key)).unwrap(),
            DestroyPlan {
                branch: "feature/a".into(),
                cascade: vec![]
            }
        );
        // the key is stale once the worktree changes
        fx.make_dirty(&m);
        let w = world(&m, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        refused(
            ctx.plan_destroy(false, Some(&key)),
            &["uncommitted changes"],
        );
    }

    #[test]
    fn destroy_relation_layer_passable_with_force() {
        let fx = Fixture::new();
        let c = cfg("[main]\nchildren = group:feat\n\n[group:feat]\n");
        // recorded parent vanished
        let wt = fx.add_worktree("orphaned", "main");
        fx.write_state(&wt, "orphaned", "group:feat", "ghost");
        let w = world(&wt, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        refused(
            ctx.plan_destroy(false, None),
            &["'ghost' no longer exists", "--force"],
        );
        assert_eq!(ctx.plan_destroy(true, None).unwrap().branch, "orphaned");
        // declared but listed in nobody's children: no derivable parent
        let c2 = cfg("[main]\nchildren = group:feat\n\n[group:feat]\n\n[solo]\n");
        let solo = fx.add_worktree("solo", "main");
        let w = world(&solo, &c2);
        let ctx = Ctx {
            world: &w,
            cfg: &c2,
            label: ".git/wtree/rules",
        };
        refused(
            ctx.plan_destroy(false, None),
            &["no recorded/derivable parent"],
        );
        assert_eq!(ctx.plan_destroy(true, None).unwrap().branch, "solo");
    }

    // ------------------------------------------------ plan_open/plan_close ----

    #[test]
    fn open_needs_an_existing_branch_without_a_worktree() {
        let fx = Fixture::new();
        let c = cfg("[main]\nchildren = dev\n\n[dev]\n");
        let w = world(&fx.repo, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        refused(ctx.plan_open("dev"), &["does not exist", "wtree new dev"]);
        refused(ctx.plan_open("main"), &["already checked out at"]);
        // a declared name is fixed on sight; anything else stays unknown until
        // it is adopted from the worktree open creates
        fx.git(&fx.repo, &["branch", "dev", "main"]);
        fx.git(&fx.repo, &["branch", "junk", "main"]);
        let w = world(&fx.repo, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        assert_eq!(
            ctx.plan_open("dev").unwrap(),
            OpenPlan {
                branch: "dev".into(),
                fixed: true
            }
        );
        assert_eq!(
            ctx.plan_open("junk").unwrap(),
            OpenPlan {
                branch: "junk".into(),
                fixed: false
            }
        );
    }

    #[test]
    fn close_refuses_only_what_would_leave_the_tree() {
        let fx = Fixture::new();
        let c = cfg(
            "[main]\nchildren = dev, group:feat\n\n[dev]\nchildren = group:feat\ndestroyable = false\n\n[group:feat]\nname-allow = feature/*\nchildren = group:feat\n",
        );
        // the primary worktree is git's, not wtree's, to remove
        let w = world(&fx.repo, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        refused(ctx.plan_close(None), &["primary worktree"]);

        // a protected fixed branch closes, live child and all: [dev] keeps it
        // in the tree with no worktree at all
        let dev = fx.add_worktree("dev", "main");
        let a = member(&fx, "feature/a", "feat", "dev");
        let w = world(&dev, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        assert_eq!(
            ctx.plan_close(None).unwrap(),
            ClosePlan {
                branch: Some("dev".into()),
                dirty: false,
                drops_record: false
            }
        );

        // a group member with a live child does not: the record inside this
        // worktree is the only thing holding it in the tree
        member(&fx, "feature/b", "feat", "feature/a");
        let w = world(&a, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        refused(
            ctx.plan_close(None),
            &["orphans its children", "'feature/b'"],
        );

        // an unmanaged worktree has no record to lose — close is its way out
        let junk = fx.add_worktree("junk", "main");
        let w = world(&junk, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        assert_eq!(
            ctx.plan_close(None).unwrap(),
            ClosePlan {
                branch: Some("junk".into()),
                dirty: false,
                drops_record: false
            }
        );

        // detached HEAD likewise, with no branch left behind to speak of
        let det = fx.add_worktree_detached("det", "main");
        let w = world(&det, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        assert_eq!(ctx.plan_close(None).unwrap().branch, None);
    }

    // -------------------------------------------------------- plan_adopt ----

    #[test]
    fn adopt_readopt_returns_previous_record() {
        let fx = Fixture::new();
        let c = cfg(
            "[main]\nchildren = group:feat, group:feat2\n\n[group:feat]\nname-allow = feature/*\n\n[group:feat2]\nname-allow = feature/*\n",
        );
        let m = member(&fx, "feature/a", "feat", "main");
        let w = world(&m, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        let plan = ctx.plan_adopt(Some("feat2"), false, "main").unwrap();
        assert_eq!(plan.kind, Kind::Group("feat2".into()));
        assert_eq!(plan.parent, "main");
        let prev = plan.previous.expect("re-adopt must surface the old record");
        assert!(prev.contains("group:feat"), "{prev}");
    }

    #[test]
    fn adopt_refuses_self_parent() {
        // Reachable only on re-adopt: a valid record makes the branch its own
        // parent candidate when its group lists itself in children.
        let fx = Fixture::new();
        let c = cfg(
            "[main]\nchildren = group:feat\n\n[group:feat]\nname-allow = feature/*\nchildren = group:feat\n",
        );
        let m = member(&fx, "feature/a", "feat", "main");
        let w = world(&m, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        refused(
            ctx.plan_adopt(Some("feat"), false, "feature/a"),
            &["cannot be its own parent"],
        );
    }

    #[test]
    fn adopt_after_raw_switch_carries_old_record() {
        let fx = Fixture::new();
        let c = cfg("[main]\nchildren = group:feat, *\n\n[group:feat]\nname-allow = feature/*\n");
        let m = member(&fx, "feature/a", "feat", "main");
        fx.git(&m, &["switch", "-q", "-c", "oops"]);
        let w = world(&m, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        let plan = ctx.plan_adopt(None, true, "main").unwrap();
        assert_eq!(plan.branch, "oops");
        assert_eq!(plan.kind, Kind::Free);
        assert!(plan.previous.unwrap().contains("feature/a"));
    }

    #[test]
    fn adopt_name_reservation_applies_to_group_and_free() {
        let fx = Fixture::new();
        let c = cfg("[main]\nchildren = group:feat, *\n\n[dev]\n\n[group:feat]\n");
        let wt = fx.add_worktree("dev", "main"); // raw-created declared name
        let w = world(&wt, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        refused(ctx.plan_adopt(None, true, "main"), &["name reservation"]);
        refused(
            ctx.plan_adopt(Some("feat"), false, "main"),
            &["name reservation"],
        );
    }

    #[test]
    fn adopt_validates_like_new() {
        let fx = Fixture::new();
        let c = cfg("[main]\nchildren = group:feat\n\n[group:feat]\nname-allow = feature/*\n");
        let wt = fx.add_worktree("junk/x", "main");
        let w = world(&wt, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        // naming constraints are not bypassed
        refused(
            ctx.plan_adopt(Some("feat"), false, "main"),
            &["does not match name-allow", "rule: name-allow"],
        );
        // no '*' in children: --free refused
        refused(ctx.plan_adopt(None, true, "main"), &["contains no '*'"]);
        // group not in children
        refused(
            ctx.plan_adopt(Some("ghost"), false, "main"),
            &["--group ghost: not in children"],
        );
        // exactly one of --group/--free
        refused(
            ctx.plan_adopt(None, false, "main"),
            &["--group <X> or --free is required"],
        );
        refused(
            ctx.plan_adopt(Some("feat"), true, "main"),
            &["mutually exclusive"],
        );
        // nonexistent parent
        refused(
            ctx.plan_adopt(Some("feat"), false, "nope"),
            &["does not exist"],
        );
        // happy path
        let wt2 = fx.add_worktree("feature/ok", "main");
        let w = world(&wt2, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        let plan = ctx.plan_adopt(Some("feat"), false, "main").unwrap();
        assert_eq!(plan.kind, Kind::Group("feat".into()));
        assert_eq!(plan.previous, None);
    }

    #[test]
    fn adopt_refuses_orphan_history() {
        let fx = Fixture::new();
        let c = cfg("[main]\nchildren = *\n");
        let wt = fx.add_worktree_detached("orph", "main");
        fx.git(&wt, &["checkout", "-q", "--orphan", "orphan-branch"]);
        fx.commit(&wt, "disconnected root");
        let w = world(&wt, &c);
        let ctx = Ctx {
            world: &w,
            cfg: &c,
            label: ".git/wtree/rules",
        };
        refused(
            ctx.plan_adopt(None, true, "main"),
            &["no common ancestor (merge-base) with parent 'main'"],
        );
    }

    // ------------------------------------------------- unknown-verb table ----

    #[test]
    fn unknown_identity_verb_table() {
        for verb in ["merge", "sync", "land", "destroy", "new"] {
            assert!(
                !verb_allowed_when_unknown(verb),
                "{verb} must be refused when unknown"
            );
        }
        for verb in ["adopt", "list", "info", "init", "save", "open", "close"] {
            assert!(
                verb_allowed_when_unknown(verb),
                "{verb} must be allowed when unknown"
            );
        }
    }
}
