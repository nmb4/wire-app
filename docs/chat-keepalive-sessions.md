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

**Implemented** (chat ALPN session pool):

- `ChatSessionPool` holds up to 8 peer QUIC connections
- Idle drop after **60s** without streams; connection is **explicitly closed**
- Outbound `SyncRequest` / invite reuse pooled connections (`open_bi`)
- Inbound accept loops on multiple bi-streams until idle or `connection.closed()`
- **400ms reuse timeout** then fresh dial (was a hard ~3s floor per send on
  dead pooled sessions); **3s stream / 4s connect** on the fresh path
- Send path **does not block** on wake — background probe + `WakeFinished`
- Failed/timed-out streams invalidate + close the pool entry and redial once

Still open: piggyback docs/blob traffic on the same session; measure burst RTT
in the wild after the offline-queue work.
