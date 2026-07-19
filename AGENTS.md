# AGENTS.md

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

### Model restrictions

**Grok models are restricted to UI-related work only.** A Grok model may work on
frontend presentation and interaction, including UI components, styling, layout,
responsive behavior, and visual polish. It must not perform or assist with any
non-UI work in this project, including backend code, APIs, data models, business
logic, infrastructure, configuration, tests, documentation, project management,
or general codebase changes. If a task is not clearly UI-related, route it to a
non-Grok model.

**Grok models are banned from touching the board.** Do not let any Grok model
(xAI) read, modify, or otherwise interact with the Wire Kanban board via the
`kanbn.lua` CLI or the Kan API. The user finds Grok's wording/communication
style too hard to follow, so its board changes are not welcome. Route any
board-related work to other models instead.
