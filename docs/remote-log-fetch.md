# Remote log fetch (`wire-cli fetch-logs`)

## Purpose

Pull GUI log files from a running Wire client by node id, with **no UI
confirmation** on the client. Used for debugging asymmetric delivery /
connectivity between peers, and for retrieving the log of a session that has
already ended (for example one that froze).

## Protocol

- ALPN: `wire/logs/1`
- Client (`wire-cli`) dials the target node and opens one bi-stream.
- Request JSON (`LogsRequest`): `{ version: 1, kind, max_bytes?, file_name? }`
- Response: JSON meta (`LogsMeta`) then `u64` body length + raw log bytes.
- Large files are **tail-truncated** (default max 8 MiB, hard cap 32 MiB).

### Request kinds

| `kind` | Extra fields | Result |
|---|---|---|
| `fetch-latest` | – | Newest log (current process's own log if known, else newest by mtime) |
| `fetch-named` | `file_name` (required) | One specific log, by bare file name |
| `list` | – | `LogsMeta.files`, newest first, empty body |

`fetch-named` exists because `fetch-latest` alone cannot serve the log you
usually want after an incident: once a frozen session is restarted, the newest
log is the *new* session's, and the interesting file is only reachable by name.
Log files are never rotated away, so an old session's log stays on disk.

`list` lets the CLI show what is available (name, size, mtime, PID, whether it
is the running process's log) before choosing a file to pull.

## Client (GUI)

- Registers `LogsProtocol` on the main router at startup
  (`wire-app/src/runtime/worker.rs`).
- Serves every `wire-app*.log` file in `%LOCALAPPDATA%/wire`, newest-first for
  `list`, plus this process's log when set via
  `wire::remote_logs::set_current_log_path` (which may live elsewhere).
- No prompt, no friends check (debug tooling; anyone who can dial the node can
  fetch). Tighten auth later if needed — note this now exposes a client's whole
  log history rather than just the newest file.
- A frozen client cannot answer: the handler runs on the same runtime that is
  wedged. Recover, then fetch the dead session's log with `fetch-named`.

## CLI

```bash
wire-cli fetch-logs <NODE_ID>
wire-cli fetch-logs <NODE_ID> -o friend.log --max-bytes 16777216

# See what the peer has, then pull a specific (e.g. previously frozen) session:
wire-cli fetch-logs <NODE_ID> --list
wire-cli fetch-logs <NODE_ID> --name wire-app-1234.log
```

Defaults: `fetch-latest` writes `wire-remote-<short-id>.log`; `fetch-named`
writes the remote file's own name, so pulling several sessions does not
overwrite them. `--list` and `--name` are mutually exclusive.

## Safety

`file_name` is always treated as a **bare file name, never a path**. A name is
accepted only if it is a single path component, contains no separator, drive
letter, or NUL, and matches `wire-app*.log`. Anything else is rejected before
any filesystem access, so `fetch-named` cannot be pointed outside the log
directory. `list` is capped at 512 entries, and the cap is applied *after*
sorting so the newest logs are the ones retained.

## Notes

- Target must be online and reachable (same discovery/NAT constraints as chat).
  A fresh CLI endpoint needs working relay or direct connectivity; it does not
  reuse a path an already-running GUI client happens to hold.
- Both sides should run a build that includes this ALPN and the `fetch-named` /
  `list` kinds.