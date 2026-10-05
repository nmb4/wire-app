# GitButler: cleaning up stale unapplied branches

Notes from a 2026-10-05 session spent removing branches that had already landed
on `main`. Written down because the obvious approach does not work and the
failure modes are confusing.

## `but branch delete` only works on APPLIED branches

The obvious command fails on any branch that is not currently applied:

```
$ but branch delete some-branch
Error: Could not find branch: 'some-branch'
Hint: Run but status for applicable targets.
```

This is not a name-resolution problem. The branch exists and `but branch list`
shows it. `but branch delete` operates on the workspace snapshot only, and
unapplied branches are not in that snapshot.

`but clean` is not a substitute. It only removes branches with **no local
commits**:

```
$ but clean --dry-run
No empty branches found.
```

Every branch worth deleting has commits, so `but clean` will never touch them.

## The working sequence is apply, then delete

```bash
but apply <branch>          # now it is in the workspace snapshot
but branch delete <branch>  # works
```

`but apply` works on unapplied branches, and also on remote-only branches — it
applies `origin/<branch>` and prints:

```
Applied remote branch 'origin/strdv-239-floating-panes' to workspace
```

Remote-only branches are therefore fully manageable through `but` alone, as
long as you are willing to apply them first.

## `but apply` refuses to run with uncommitted changes

Any uncommitted file that the incoming branch also touches blocks the apply:

```
Error: Uncommitted files would be overwritten by checkout: ".gitignore", "AGENTS.md"
```

GitButler has no stash. Park the work instead, using the normal commit path:

```bash
but diff                                              # get file IDs
but commit -b wip/park-<reason> -m "wip: ..." <id> <id>
```

Then apply and delete the stale branches, and afterwards:

```bash
but unapply wip/park-<reason>
```

`unapply` moves the commit's changes back into uncommitted status. You end up
where you started, with the branch ref still around until you delete it
yourself.

## Parking a branch can break `but apply` for everything else

This is the trap that cost the most time. While the wip branch was applied,
every other `but apply` failed with a misleading message:

```
Failed to apply branch: 'bb/wir-3-...' conflicts with existing stack: wip/park-branch-cleanup
```

The reported "conflict" had nothing to do with the files involved — the wip
commit touched `.gitignore`, `AGENTS.md`, docs and scripts, and the branch
being applied touched `wire-app/src/chat.rs`. The real problem was simply that
a second stack was applied in the workspace.

So: **park the work, then `but unapply` the parking branch, then apply and
delete the stale branches, then re-apply the parking branch** if you still want
it. Sequence matters more than it should.

## `but branch delete` does not delete git refs

After a successful delete, git refs survive:

```
$ but branch delete fix-windows-tray-icon-registration
Discarded branch 'fix-windows-tray-icon-registration'

$ git rev-parse --verify -q origin/fix-windows-tray-icon-registration
fbc75ceefb60134c4a807b6b77670d24eefb9aea      # still there
```

`but branch delete` removes the branch from the **workspace**, nothing more.
The local ref is gone for branches that had one, but:

- remote-only branches keep their `origin/*` ref, so they are still listed by
  `but branch list` afterwards
- actually removing them from the remote needs `git push origin --delete`

Do not assume "deleted" from GitButler means gone from the repo.

## Branches checked out in another worktree cannot be deleted

`git branch -D` refuses for a branch held by a linked worktree:

```
$ git worktree list
/Users/noah/dev/wire-app                          ... [gitbutler/workspace]
/Users/noah/.bb/worktrees/env_adf4izchjc/wire-app ... [bb/wir-1-import-issues-from-kanbn-thr_q478ks4twc]
/Users/noah/.bb/worktrees/env_iuh6r4rjq5/wire-app ... [bb/wir-3-image-not-sending-thr_uz8xx7r4at]
```

`but branch delete` will discard it from the workspace anyway and report
success, while the git ref lives on. Any `bb/*` branch spawned by another agent
runs in `~/.bb/worktrees/`, so expect this for those. Pruning the worktree is a
`git worktree remove` on someone else's sandbox — ask first.

## Detecting whether a branch is already landed

Patch-id comparison against `main` answers "is this work already merged?"
without trusting commit SHAs, which differ after any rebase or cherry-pick:

```bash
git log --no-merges -p --pretty=format:'commit %H' origin/main \
  | git patch-id --stable > /tmp/main_patchids.txt

p=$(git show <commit> | git patch-id --stable | cut -d' ' -f1)
awk -v p="$p" '$1==p {print $2}' /tmp/main_patchids.txt
```

Empty output means the commit is unique and still needs review. A hit means the
branch is safe to discard no matter how old it looks.

Caveat: patch-id equality means the same change landed, not that the same
branch is redundant. A branch can be "already landed" and still be the only
place some follow-up work exists. Check every commit on the branch, not just
the tip.

## Before deleting anything

- Record the SHAs somewhere outside the repo. Commits on a discarded branch
  become unreachable and are eventually garbage collected.
- Back up `.git/hooks/pre-commit` and `.git/hooks/post-checkout`. Leaving the
  `gitbutler/workspace` branch can fire `post-checkout`, which deletes the
  GitButler-managed hooks.
- Watch for `git status` after every step; `but` can be flaky and its
  "Discarded branch" output is worth confirming against `git branch --list`.