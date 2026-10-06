# OptMem (project-local install)

This repo carries its own copy of [OptMem](https://github.com/VictorTaelin/OptMem)
instead of the upstream `~/.optmem` install, so nothing touches your home
directory, PATH, or user/system environment, and the same setup works on
Windows and macOS.

- `.optmem/memo` - the tool: one Python 3 file, no dependencies (upstream
  `main`, plus the patches below).
- `.optmem/memory/` - this repo's store: `LOG.txt` (append-only, fixed
  320-byte records), `TREE/` (summary cache), `config`.
- `scripts/memo` - entry point for bash (macOS, Linux, Windows Git Bash).
- `scripts/memo.cmd` - entry point for cmd.exe and PowerShell.

Both entry points pick a Python 3 from the machine (`py -3`/`python3`/
`python`, validated before use) and pin `MEMORY_DIR` to this store. A bare
invocation of `.optmem/memo` resolves to the same store via patch 1 below.
Agent-facing instructions are in the `## Memory` block at the top of
`AGENTS.md`.

## Patches vs upstream (reapply after updating)

The vendored file deviates from upstream in exactly three places (patches
1 and 2 are marked `PATCHED (project-local install)`; patch 3 rewrites the
usage docstring's store path):

1. `memory_dir()` defaults to `<this file's dir>/memory` instead of
   `~/.optmem/memory`, so no invocation path can create a user-wide store:

   ```python
   # upstream
   return os.path.expanduser(os.environ.get("MEMORY_DIR") or "~/.optmem/memory")
   # patched
   return os.path.expanduser(os.environ.get("MEMORY_DIR")
                             or os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                             "memory"))
   ```

2. `ME` names the wrapper (forward slashes, so bash can run it too) instead
   of the raw file, so every command the tool prints runs with the right
   Python and this project's store:

   ```python
   # upstream
   ME = pretty(__file__)
   # patched
   ME = pretty(os.path.join(os.path.dirname(os.path.abspath(__file__)),
                            "..", "scripts", "memo")).replace("\\", "/")
   ```

3. The usage docstring says the store is `memory/` next to the tool, not
   `~/.optmem/memory`.

To update from upstream:

```sh
curl -fsSL https://raw.githubusercontent.com/VictorTaelin/OptMem/main/memo -o .optmem/memo
# re-apply the three patches above (their exact text is quoted here),
# then verify:
scripts/memo wake          # must read this store, not ~/.optmem
git diff .optmem/memo      # review what changed
```

`.gitattributes` marks `.optmem/**` as `-text`: the records are
position-addressed, so line-ending conversion would silently corrupt them.

## Sharing through git (public repo!)

`.optmem/memory/` is committed, so Windows and macOS machines share one
memory via normal `git pull` / `git push`. The repo is public: everything
you `memo note` is readable by anyone. Never record secrets.

The discipline that makes this safe:

- `git pull` right before `memo note`, and push soon after. Two machines
  noting without pulling in between is what creates conflicts.

### If `memory/LOG.txt` conflicts

Records are fixed width: 320 bytes = text + space padding + `\n`, and
position is identity, so a misaligned record corrupts every later one.
Never keep conflict markers, and never leave a short (unpadded) line.

Both sides appended different memories to the same base, so the merge is:
keep both sides' `#<id> <date> <text>` lines, dedupe by id (if both
machines wrote the same id, keep one line - the other machine still has
its copy), sort by id ascending (they must end up contiguous from 0),
re-pad every line to exactly 320 bytes. This script does it in place:

```sh
python - <<'EOF'
import re
p = ".optmem/memory/LOG.txt"
recs = {}
for raw in open(p, encoding="utf-8"):
    line = raw.rstrip("\n")
    if line.startswith(("<<<<<<<", "=======", ">>>>>>>")):
        continue
    m = re.match(r"^#(\d+) ", line)
    if not m:
        if line.strip():
            raise SystemExit("unexpected non-record line: %r" % line[:80])
        continue
    recs[int(m.group(1))] = line
ids = sorted(recs)
if ids != list(range(len(ids))):
    raise SystemExit("ids are not contiguous from 0: %r" % ids)
out = b"".join(recs[i].encode("utf-8").ljust(319) + b"\n" for i in ids)
open(p, "wb").write(out)
print("merged: %d records, %d bytes (multiple of 320: %s)"
      % (len(ids), len(out), len(out) % 320 == 0))
EOF
```

For `memory/TREE/*` conflicts: summaries are a rebuildable cache, so take
either side and treat them as suspect. If anything looks stale, the safe
reset is `rm .optmem/memory/TREE/*` and re-running `scripts/memo wake`,
which then asks for the `nap` compressions it needs and rebuilds them from
the merged log. `LOG.txt` is the only source of truth.

Verify afterwards: `scripts/memo recall '.'` must print every memory
without a traceback, and `scripts/memo wake` must end with `You are awake.`
