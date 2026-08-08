# wtree

[한국어](README.ko.md)

wtree is a policy-based git worktree manager, designed around working with AI agents.

## Idea

`git worktree` is a good way to work on several branches at once, and it gives each of them a space of its own — which is exactly what running AI agents like Claude Code or Codex in parallel needs.

But you have to pick the path yourself, and once you are done, removing the worktree and deleting the branch are two separate chores.

With agents, where a branch is cut from and where it merges back rests entirely on what you told the agent. Agents break those rules often, and nothing tells them they did.

wtree takes parentage, naming and merge method (ff, rebase, squash and so on) into a policy up front, and allows only what falls inside it.

## Install

```bash
cargo install gitwtree
```

The command is `wtree`. Unix-like systems only.

## Getting started

```bash
wtree init --new
```

This writes `.git/wtree/rules`. Open it and declare the structure. The smallest form that puts work branches under main:

```ini
[main]
children = group:work
destroyable = false
merge-mode = squash

[group:work]
name-allow = feat/*, fix/*
ephemeral = true
```

Now create a branch.

```bash
wtree new feat/login
```

It prints a `cd` command for the new worktree. Work there, commit, then:

```bash
wtree land -m "feat: add something"
```

That squash-merges into main and cleans up the worktree and the branch. To merge and keep the worktree, use `wtree merge`. `squash` and `no-ff` create a new commit, so they need `-m`.

`wtree` on its own lists only the verbs that would get past the policy where you are standing. `wtree help --all` is the full manual.

## Verbs

| verb | |
|---|---|
| `new <name>` | create a branch and its worktree |
| `open <branch>` | give an existing branch a worktree |
| `close` | remove the worktree, keep the branch |
| `merge` | merge into its parent |
| `sync` | merge the parent into this branch |
| `land` | merge, then destroy |
| `destroy` | remove the branch and its worktree |
| `adopt` | bring an existing branch under the policy |
| `list` / `info` | what exists, and what is allowed here |
| `init --new` | write the starter files |
| `init --load [path]` | take the rules from a `.wtree/` instead |
| `save [path]` | copy the rules out to a `.wtree/` you can commit |

`wtree init` with neither flag asks which of the two you meant, and refuses when there is no terminal to ask on.

## Config

| file | |
|---|---|
| `rules` | branch policy |
| `settings` | where worktrees are created, and other settings |
| `hooks/post-create` | runs right after `wtree new` |

In `rules`, `[X]` is a fixed branch and `[group:X]` is a group of work branches sharing one policy. `children` declares what may take this section as its parent.

| key | sections | meaning |
|---|---|---|
| `children` | `[X]` `[group:X]` | `group:X`, `*` (free branches), or a declared fixed branch name |
| `destroyable` | `[X]` | `false` refuses destroy unconditionally. Default `true` |
| `name-allow` / `name-deny` | `[group:X]` | glob patterns for branch names (only `*` and `?` are special) |
| `ephemeral` | `[group:X]` | collected along with the parent on destroy, if the safety checks pass. Default `false` |
| `merge-mode` | `[X]` `[group:X]` | merge methods this branch accepts. `squash`, `rebase`, `no-ff`, `ff`, comma-separated |
| `copy` | `[X]` `[group:X]` | untracked files a new worktree takes from its parent's |
| `description` | `[X]` `[group:X]` | one line on what the branch is for, printed by `wtree` and `info` |

## License

MIT
