## Memory

Your memory is OptMem, installed for this repository only (project-local;
nothing lives in `~/.optmem` and no PATH/env changes were made):
- The tool is `scripts/memo` (bash: macOS, Linux, Windows Git Bash) or
  `scripts\memo.cmd` (cmd.exe, PowerShell)
- Your memories are in `.optmem/memory`

OptMem outlives every session, compaction, model and vendor change.
Without it you do not know who you are, or what was decided and tried.
This repo is public on GitHub and the memories are committed to it: never
record secrets, credentials, or private personal data.

### At startup: activating OptMem (mandatory)

Run `scripts/memo wake` before any other tool call, in every session, and
then do exactly what it prints, to the end of its output. If wake reports
that there is no memory, run `scripts/memo init` once, then wake again.

### While working: register memories (mandatory)

Call `scripts/memo note "<1 line, max 280 bytes>"` whenever you learn
something new, or something worth keeping happens. That covers a task
worth real effort, a fact or insight the user teaches you, anything you
learn about their life (even indirectly), any event of lasting effect.
Do not register redundant memories.

If `scripts/memo note` asks a compression: do it before your next action.

Never edit or delete anything under `.optmem/memory`: the tool manages it.
The one exception is a git conflict under `memory/`; resolve it per the
instructions in `.optmem/README.md`.

### When you need an old memory: search, or navigate

`scripts/memo recall <regex>` searches every memory, word for word.
Your memories also form a binary tree: #0-1, #2-3 ... exist as one-line
summaries, pairs of those as #0-3, and so on -- every `#a-b` line wake
prints is one node of it. `scripts/memo zoom <a-b>` opens a node into its
two halves, down to the raw memories.

### If you're a subagent: skip everything above

Parallel sessions on this machine are all you, and may all write memories.
A subagent is not: it must never run `memo`, because it cannot judge what
is already known, and its notes would arrive duplicated and incorrectly.
When you spawn one, write: `You are a subagent. Don't run memo.`

## Version control

This repository uses standard Git. Do not use GitButler, the `but` CLI, or
GitButler skills for this repository. Do not run `but setup` or re-enable
GitButler workspace management. Use normal Git commands for branches,
commits, merges, pushes, and other version-control operations.

## UI work

Before changing the egui UI, read `docs/ui-improvement-guide.md`. Capture
before/after screenshots with `scripts/ui-capture.sh <out-dir> [filter]`
(renders fixture scenes headlessly; needs the gitignored `wire-app/fonts/`
for accurate typography).
