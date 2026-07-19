# Chat offline / unreachable queue (planned → implementing)

## Problem

Today, own messages are written to the local Iroh doc immediately (durable), but
the UI stays on **syncing / delivery retry** and the client keeps waking peers
on a backoff forever—even when the friend never has Wire open.

That is noisy, misleading, and wastes connects.

## Desired behavior

1. Message still **commits locally** (no data loss; peer will get it later via
   doc sync when they come online).
2. If **no conversation member is reachable** on the delivery wake path, show
   the bubble **grayed** with label **`queued`** (not endless retry).
3. **Stop aggressive retries** while queued.
4. When a peer becomes reachable again (they dial us, send us traffic, or a
   probe succeeds), **resume**: wake + mark pending until receipt → delivered.

## Reachability signal (v1)

Practical, no separate presence protocol:

| Event | Meaning |
|-------|---------|
| `SyncRequest` / invite **connect succeeds** | Peer reachable → active delivery |
| Connect **fails** for all other members | Unreachable → **queued** |
| Inbound chat protocol from peer | Peer alive → resume queued for shared conversations |
| Remote doc activity on conversation | Someone is syncing → resume queued |

Do **not** treat local `InsertLocal` alone as “peer online.”

## States

| State | UI | Retries |
|-------|----|---------|
| `Pending` | syncing… | normal backoff after a successful wake |
| `Retrying` | retrying… | peer was reachable; still waiting on receipt |
| `Queued` | queued | slow background probes only (15–60s); UI stays queued |
| `Delivered` | (none) | — |
| `Failed` | failed | local commit error only |

Online receipt waits keep a fast sub-second→3s schedule. Offline (after several
failed wakes) switches to long backoff, does not stack concurrent dials, and
does not overwrite the UI with “delivery retry N”.

## Edge cases

- **Groups:** queued if *no* other member reachable; later may refine to
  partial delivery.
- **App restart:** restore undelivered own messages; one probe per conversation
  → queued or pending.
- **History clear:** drop pending/queued for that conversation with the doc.

## Status

**Implemented** in `wire-app` (`DeliveryState::Queued`, wake probe on send,
resume on inbound chat protocol / remote doc insert / sync finished). Keep-alive
sessions remain separate (`chat-keepalive-sessions.md`).
