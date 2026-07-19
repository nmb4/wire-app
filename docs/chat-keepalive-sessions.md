# Chat keep-alive sessions (planned)

## Problem

Each outbound chat wake opens a short-lived `wire/chat-invite/1` connection
(`SyncRequest` / invite). On paths where docs `Connect(DirectJoin)` is flaky
(see `chat-delivery-asymmetry.md`), delivery often depends on that wake + the
peer pulling. Rapid back-and-forth then pays dial cost repeatedly (~seconds),
while voice/screen feel instant because they already hold a QUIC session.

## Idea

After successful chat contact with a peer (we connected, or they connected to
us), **keep a chat-plane session warm for ~60–120s of idle time**.

- Pool per `NodeId` (or per conversation): open QUIC + optional docs sync interest
- Reuse for `SyncRequest`, invites, and ideally docs/blob traffic if the stack allows
- Idle timer resets on send/recv; drop after 1–2 minutes quiet
- Cap concurrent pooled peers (e.g. recent DMs only)

## Expected benefit

Bursty chatter after the first message should approach call-like latency when
the path works at all. Does **not** replace NAT/relay fixes for cold start to
an unreachable peer.

## Non-goals (this doc)

- Full presence system
- Replacing iroh-docs gossip
- Keeping sessions forever (battery / fd pressure)

## Status

**Not implemented.** Do after offline/queued delivery (`chat-offline-queue.md`).
Suggested order: queue UX → measure → keep-alive pool on chat ALPN → explore
docs session reuse.
