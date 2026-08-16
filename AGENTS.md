# AGENTS.md

## Foresight

Work iteratively. The goal is not to one-shot tasks; the goal is to make them right.

Before changing code, spend a small amount of time connecting the task to the surrounding system. Think ahead far enough to catch likely interactions, common states, and user/developer friction, but do not turn every task into a heavyweight specification exercise.

### Before implementation

For any non-trivial task:

1. Understand the requested change and the code paths it touches.
2. Identify the closest connected behavior, state, or subsystem.
3. Ask what common real-world situations become possible because of this change.
4. Decide where it makes sense to stop for a checkpoint or manual test.
5. Then implement.

Keep this lightweight. A short markdown plan or task note is enough when writing the reasoning down helps. Do not create large specs unless the task genuinely requires one.

### Think in connections, not tunnels

Do not treat the requested feature as an isolated island.

When adding or changing behavior, inspect the immediate neighboring cases. In particular, consider:

- What can happen immediately before this state?
- What can happen while this state is active?
- What other common action can occur at the same time?
- What happens when the action is repeated, interrupted, cancelled, replaced, or superseded?
- Which existing state machines, resources, UI states, network flows, or lifecycle events interact with it?
- Could the new behavior leave the application in a state that is difficult to recover from?

Prioritize **common connected cases** over exhaustive edge-case hunting. A feature that works in isolation but breaks during an ordinary adjacent action is not complete.

Do not spend disproportionate effort on extremely rare cases during the first pass. Refinement can happen later.

### Foresight over hindsight

Prefer preventing the next likely problem over preserving every historical behavior.

This codebase may move quickly. Do not add compatibility layers, migrations, deprecated paths, or preservation logic merely because old behavior existed. Maintain backwards compatibility only when the project actually needs it or the task explicitly requires it.

Use previous mistakes as information, not as a reason to accumulate defensive complexity.

### Iterate deliberately

Implementation should be a loop:

**understand → foresee → implement → verify → checkpoint → refine**

A checkpoint is useful when the next meaningful information can only come from running or manually using the software. Do not keep expanding the implementation based on increasingly speculative assumptions when a quick real-world check would provide better information.

Likewise, do not stop after the narrow requested code compiles if one more closely related check can prevent an obvious follow-up bug.

### User experience and developer experience both matter

Evaluate changes from both sides.

For the user:

- Does the feature behave sensibly in normal use?
- Are transitions and conflicting actions handled?
- Can the user recover from interruption or failure?
- Does the application expose confusing, impossible, or stale states?

For the developer:

- Is the behavior understandable from the code?
- Are important states visible through useful logs or diagnostics?
- Will the next change require fighting unnecessary complexity?
- Can likely failures be reproduced and reasoned about?

A technically correct implementation that creates a poor normal-use experience is incomplete.

### Verification

Use automated tests where they give strong signal cheaply, especially for deterministic state transitions, parsing, data transformations, protocol behavior, and regressions.

Do not assume automated tests cover application experience. Some problems only become apparent during manual use, especially UI, audio/video, networking, timing, multi-client, and cross-platform behavior.

Before considering a non-trivial feature complete, think through the manual scenarios a person is likely to try first. When useful, tell the developer exactly what should be manually checked at the next checkpoint.

### Expensive feedback loops

Be conscious of expensive build, compile, deployment, or reproduction cycles.

For projects with slow feedback loops—such as large Rust builds—spending additional time reasoning through closely related behavior before compiling is often cheaper than discovering one predictable issue per build.

This is not an excuse for speculative overengineering. Think further when the likely cost of another feedback cycle is higher than the cost of a short foresight pass.

### Scope discipline

Foresight does not mean expanding every task.

When you notice a related issue:

- Fix it now if it is a direct, common consequence of the requested change and the fix is small.
- Include it in the current plan if the requested feature would otherwise be unsafe or obviously incomplete.
- Record or mention it for later if it is useful but separable.
- Ignore it if it is remote, speculative, or would derail the task.

The purpose of foresight is to reduce avoidable follow-up work, not to create endless scope.

### Completion standard

Before finishing, ask:

- Did I solve the requested problem rather than only its easiest interpretation?
- Did I inspect the most closely connected common cases?
- Did I create or preserve sensible behavior when states overlap?
- Did I avoid unnecessary backwards-compatibility baggage?
- Did I verify what can be verified automatically?
- Did I identify what still needs manual verification?
- Is this a good experience for both the user and the developer?

If the answer to an important question is no, either address it or make the limitation explicit.

## Current project infrastructure status

- GitHub CI is not currently used for this project. Treat the workflow files as
  dormant scaffolding, not as evidence that a change has been or will be
  verified remotely. Run the relevant checks locally and report their results.
- The Android workflow and packaging configuration were inherited from the
  `callme` example repository that Wire was originally based on. Android is not
  currently built, tested, released, or otherwise supported by the project.
- An Android version remains a possible future direction, but the current egui
  experience on Android is considered the main usability blocker. Do not expand
  Android-specific code or workflow maintenance as part of unrelated work;
  treat Android enablement as separately scoped product and platform work.

## Wire Kanban Board (kan.bn)

The project's tasks live on a Kan board named **Wire**.

- **Board public ID:** `mx87hw9x3zf3`
- **Workspace:** General (`0w1w9dpim929`, card prefix `GEN`)
- **CLI:** `scripts/kanbn.lua` (API key read from `KANBN_API_KEY` in `.env`)

### Lists

| Index | List | Purpose |
|-------|------|---------|
| 0 | Bugs | Reported defects |
| 1 | Features | Planned/accepted feature work |
| 2 | Ideas | Unscheduled proposals and research |
| 3 | In Progress | Actively being worked on |
| 4 | In Testing | Implemented, under verification |
| 5 | Done | Completed and verified |
| 6 | Dropped | Abandoned or superseded |

Cards are numbered with the `GEN-` prefix (e.g. `GEN-77`).

### CLI quick reference (`scripts/kanbn.lua`)

The Kan API only accepts 12-char **card public IDs** (e.g. `es3x2wr9cp4u`), never
`GEN-N`. The CLI resolves `GEN-N` for you in most commands, so prefer it.

```bash
# Set the key once (quotes in .env are auto-stripped by the CLI):
export KANBN_API_KEY="$(grep KANBN_API_KEY .env | cut -d= -f2 | tr -d '"')"

# Resolve a card number -> publicId (use this when you only have GEN-N):
lua scripts/kanbn.lua card-by-number 0w1w9dpim929 GEN-19

# Read a card (accepts GEN-N or publicId):
lua scripts/kanbn.lua card GEN-19

# Update a card field (key=value pairs, e.g. description as HTML):
lua scripts/kanbn.lua card update GEN-19 description="<p>...</p>"

# Add a checklist to a card (returns checklist publicId):
lua scripts/kanbn.lua checklist add GEN-19 "Requirements"

# Add an item to a checklist (returns item publicId):
lua scripts/kanbn.lua checklist-item add <checklistPublicId> "Do the thing"

# Explore the whole board (lists + cards + short descriptions):
lua scripts/kanbn.lua explore-board mx87hw9x3zf3
```

Notes:
- `checklist add` and `card` accept either a `GEN-N` reference (resolved via the
  workspace, default `0w1w9dpim929`) or a raw public ID.
- Prefer the high-level `card update` / `checklist` / `checklist-item` commands
  over raw `request` — raw JSON args are fragile on Windows (cmd.exe strips
  quotes). Use `request ... --body-file FILE` if you must send hand-written JSON.

### When to hit the API

Only call the Kan API when **explicitly asked** to (e.g. "read the board", "add a
card", "update GEN-42") or when the user **references a card ID** like `GEN-77`.
Do not query or modify the board proactively.
