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

`wtree` on its own lists only the verbs that would get past the policy where you are standing. `wtree -h` is the full manual, and `wtree <verb> -h` is one verb's line.

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
| `hooks/` | scripts run around `new`, `merge` and `destroy` |

In `rules`, `[X]` is a fixed branch and `[group:X]` is a group of work branches sharing one policy. `children` declares what may take this section as its parent.

| key | sections | meaning |
|---|---|---|
| `children` | `[X]` `[group:X]` | `group:X`, `*` (free branches), or a declared fixed branch name |
| `destroyable` | `[X]` | `false` refuses destroy unconditionally. Default `true` |
| `name-allow` / `name-deny` | `[group:X]` | glob patterns for branch names (only `*` and `?` are special) |
| `ephemeral` | `[group:X]` | collected along with the parent on destroy, if the safety checks pass. Default `false` |
| `merge-mode` | `[X]` `[group:X]` | merge methods this branch accepts. `squash`, `rebase`, `no-ff`, `ff`, comma-separated. `none` (alone) accepts no merges |
| `copy` | `[X]` `[group:X]` | untracked files a new worktree takes from its parent's |
| `description` | `[X]` `[group:X]` | one line on what the branch is for, printed by `wtree` and `info` |

What each `merge-mode` leaves in the parent:

| mode | the parent gets | the branch's commits |
|---|---|---|
| `ff` | the branch's commits as they are | kept |
| `rebase` | the same commits, replayed on its tip | kept, rewritten if the parent moved |
| `squash` | one commit | folded into it |
| `no-ff` | one merge commit | kept, on its second parent |

`no-ff` reads as a squash without discarding anything. `git log --first-parent` shows one line per branch, and plain `git log` still has every commit.

## Hooks

Executables in `hooks/`, named after the moment they run at. `init` writes a `post-create.sample` documenting the whole contract; rename it to enable it, or link it under several names and branch on `$WTREE_HOOK`.

| hook | runs |
|---|---|
| `pre-create` / `post-create` | around `wtree new` and `wtree open` |
| `pre-merge` / `post-merge` | around `wtree merge`, and the merge half of `land` |
| `pre-destroy` / `post-destroy` | around `wtree destroy`, and the destroy half of `land` |

A `pre-` hook is a gate: a non-zero exit aborts the verb before anything has been touched. A `post-` hook only reports, so a non-zero exit is a warning and what the verb did stands. `sync` and `close` run no hooks. Under `land` both gates run before the merge, so either can still abort the whole verb.

A hook must leave the working tree as it found it. New files a hook leaves there make `land` stop (`stopped:`, naming the files and the hooks that ran) rather than force-delete them; `wtree destroy` then finishes the job once they are dealt with.

Each hook gets `WTREE_HOOK`, `WTREE_REPO` and `WTREE_INTERACTIVE`, plus `WTREE_PATH` and `WTREE_BRANCH` for the worktree it concerns. `WTREE_VERB` names the verb that was typed, which is how the create pair tells `new` from `open` and the other two tell a bare verb from `land`'s. The merge pair adds `WTREE_TARGET`, `WTREE_MODE`, `WTREE_MESSAGE` and `WTREE_DIRTY`, and `WTREE_TIP` for `post-merge`. The sample lists all of them.

On `new` and `open`, everything after `--` reaches the create pair as `"$@"` — word boundaries intact, nothing expanded — so a hook can start whatever the worktree was made for:

```sh
wtree new feat/login -- claude 'fix GH #322'
```

`--no-hooks` skips every hook for one run, the `pre-` ones included. It takes the place of disabling a hook file and forgetting to put it back:

```sh
wtree merge --squash -m 'fix the thing' --no-hooks
```

## License

MIT
