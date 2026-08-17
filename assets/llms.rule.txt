# wtree rule — the rules file reference

Rules are defined in `.git/wtree/rules`. Edit it as text; the next wtree run picks it up. `wtree rule` shows the rules currently in effect.

## Example

A main-dev shape:

```ini
[main]
children = dev
destroyable = false
merge-mode = ff, no-ff

[dev]
children = group:work
merge-mode = squash

[group:work]
name-allow = feat/*, fix/*
```

It works as follows.
- From main, `wtree new` can create the dev branch.
- From dev, `wtree new` can create [group:work] branches. Names outside `feat/*`, `fix/*` are refused.
- The dev branch merges to main with `wtree merge`. The method is a choice of ff or no-ff.
- A [group:work] branch merges to dev with `wtree merge`. The method is fixed to squash.
- main cannot be removed with `wtree destroy`.

## Writing it

### Sections

`[X]` declares one fixed branch. `[group:X]` declares a set of work branches sharing one policy. `children` sets the parent-child relation. A branch declared as a child takes that section's branch as its parent.

### Keys

- children (`[X]` `[group:X]`): what may take this section as its parent. A comma-separated list of `group:X`, declared fixed branch names, or `*` (free branches). A declared fixed branch name may be listed only under `[X]`.
- destroyable (`[X]`): `false` refuses destroy unconditionally. Default `true`.
- name-allow / name-deny (`[group:X]`): glob patterns for branch names. Only `*` and `?` are special.
- ephemeral (`[group:X]`): with `true`, children that pass the safety checks are deleted along with a destroyed parent. Default `false`.
- merge-mode (`[X]` `[group:X]`): the merge methods this branch accepts. A comma-separated list of squash, rebase, no-ff, ff. The default accepts every method. With a single method the option can be omitted from `wtree merge`. none stands alone and accepts no merges.
- copy (`[X]` `[group:X]`): untracked files a new worktree takes from its parent's worktree. Patterns match entries at the worktree root; a directory ends with `/`.
- description (`[X]` `[group:X]`): one line on what the branch is for. `wtree` and `wtree info` print it.

### How each merge-mode works

`no-ff`
- Same as `git merge --no-ff` on the parent. Unlike plain git, the branch also advances to the merge commit.
- `-m` (a commit message) is required.

`ff`
- Same as `git merge --ff-only` on the parent.

`rebase`
- Two steps.
    1. `git rebase parent`
    2. `git merge --ff-only child` on the parent

`squash`
- Four steps.
    1. soft reset of the commits since the merge base
    2. commit them as one change
    3. `git rebase parent`
    4. `git merge --ff-only` on the parent
- So unlike a plain squash, the branch remains usable afterwards.
- `-m` (a commit message) is required.

`none`
- Accepts no merges at all.
