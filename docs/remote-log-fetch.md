# Remote log fetch (`wire-cli fetch-logs`)

## Purpose

Pull the latest GUI log file from a running Wire client by node id, with **no
UI confirmation** on the client. Used for debugging asymmetric delivery /
connectivity between peers.

## Protocol

- ALPN: `wire/logs/1`
- Client (`wire-cli`) dials the target node and opens one bi-stream.
- Request JSON (`LogsRequest`): `{ version: 1, kind: "fetch-latest", max_bytes? }`
- Response: JSON meta (`LogsMeta`) then `u64` body length + raw log bytes.
- Large files are **tail-truncated** (default max 8 MiB, hard cap 32 MiB).

## Client (GUI)

- Registers `LogsProtocol` on the main router at startup.
- Serves the current process log path when set via
  `wire::remote_logs::set_current_log_path`, else the newest
  `%LOCALAPPDATA%/wire/wire-app-*.log` by mtime.
- No prompt, no friends check (debug tooling; anyone who can dial the node can
  fetch). Tighten auth later if needed.

## CLI

```bash
wire-cli fetch-logs <NODE_ID>
wire-cli fetch-logs <NODE_ID> -o friend.log --max-bytes 16777216
```

Default output: `wire-remote-<short-id>.log` in the current directory.

## Notes

- Target must be online and reachable (same discovery/NAT constraints as chat).
- Both sides should run a build that includes this ALPN.
