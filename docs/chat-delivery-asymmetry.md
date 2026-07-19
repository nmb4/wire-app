# Chat delivery asymmetry (NAT / path notes)

Observed 2026-07-19 on a live DM while both peers were on a working Discord
voice + screen-share call (low perceived Discord RTT). Wire voice/screen also
worked fine in both directions. Text chat did not: **A → B messages arrived
~2–3s late on B’s UI; B → A messages arrived on A in ~40–80ms.**

This is a real product issue, not a misread of the “syncing…” delivery label.

## Evidence (sender log `wire-app-17212.log`)

Outbound (commit → remote receipt = peer already processed the message):

| Commit (local) | Receipt back | Gap |
|----------------|--------------|-----|
| 19:14:22.074   | 19:14:25.203 | ~3.1s |
| 19:14:36.258   | 19:14:39.234 | ~3.0s |
| 19:14:50.085   | 19:14:52.797 | ~2.7s |

Inbound (friend’s message appears locally):

- `source=remote` → `content-ready` → ack in **~40–80ms**
- Example: local commit at 19:14:36.258, friend’s message at 19:14:36.276 (**18ms**)

Same session, same peer, opposite directions.

## What failed on the slow side

On the slow sender (`me=ec4fd48232`, peer `cc35c21b0c`):

```
gossip dial failed: failed connecting to remote endpoint peer=cc35c21b0c…
sync failed origin=Connect(DirectJoin) err=Failed to establish connection
```

Repeated at startup for the DM (and other conversations).

Also:

```
iroh_quinn_udp sendmsg error: Os { code: 10040, … len: 1452 }
destination: [2001:9e8:…]:63399    # peer IPv6
destination: 87.123.246.129:7555   # relay-ish path
```

Windows `WSAEMSGSIZE` (10040) on ~1452-byte UDP payloads — path MTU /
fragmentation trouble on the path this node uses to transmit.

Meanwhile the same log is full of successful **inbound** chat connections:

```
accepting chat invitation peer=cc35c21b0c
```

So: **Accept works; Connect(DirectJoin) often does not.** Classic
asymmetric reachability / NAT behavior.

## Why calls and screen share still work both ways

Calls use a different stack than chat:

| Path | ALPN / stack | How traffic flows |
|------|----------------|-------------------|
| Voice / screen | `RtcProtocol` (iroh-roq / media) | One side dials (or both); once the QUIC session is up, **media is bidirectional on that session** |
| Chat messages | `iroh-docs` + `iroh-gossip` + `iroh-blobs` | Live sync often needs **this node to Connect outbound** (`DirectJoin`) to push/pull doc updates |
| Chat wake / invite | `wire/chat-invite/1` | Separate short-lived connect per wake |

So “I can send audio to them” does **not** imply “my docs engine can dial them.”

Likely call setup succeeded because:

1. They dialed us (or hole-punch completed for the RTC ALPN), and
2. After connect, A/V rides the established connection both ways.

Chat write path still tries docs/gossip **Connect** from the writer. When that
fails, the peer only learns about the new entry when **they** dial us and pull
(e.g. after a `SyncRequest` wake, or when they send something). That matches the
~2–3s delay seen on screen share.

## Role of blobs

Message bodies are blob-backed (`doc.set_bytes` → content hash). After the peer
sees the new doc entry they must fetch the blob.

Blob fetch is usually **pull**: the peer that lacks content connects to a
provider that has it. If the peer can dial us (Accept on our side), blob
download is fine **once they know the hash**.

So blobs are probably **not** the primary asymmetry. The slow step is **docs
gossip/sync telling them a new entry exists** when our outbound `Connect` fails.
Receipts are the same shape (small blob + doc entry) and only return after the
peer has already loaded the message — which is why receipt RTT tracked the
visible delay on their screen.

## App-side aggravators (fixable without fixing NAT)

Historical send path made the slow direction worse:

1. **Wake after heavy work** — `sync_and_wake` ran full `open_and_publish` /
   timeline reload **before** `SyncRequest`, delaying the only reliable nudge
   when outbound docs Connect fails.
2. **Invite spam on every wake** — full ticket invites every send; peer logged
   endless `ignored chat invitation for a non-canonical replica`, burning
   connects while docs sync was already struggling.
3. **SyncRequest handler re-invited** — every wake triggered a return invite
   storm.

Fixes in code: wake (`SyncRequest`) immediately after local insert; reserve
invites for create/clear/initialize; don’t invite on every delivery wake;
avoid re-`start_sync` + full publish before waking peers.

## Keep-alive footgun (fixed 2026-07-19)

Session pooling introduced a worse failure mode than slow dials:

1. Inbound accept and outbound dial both tried to own one pooled connection per
   peer.
2. The “losing” connection was **force-closed** (`chat-replaced` / `chat-dup-dial`).
3. Logs: `chat session stream failed: closed by peer: chat-replaced`.
4. SyncRequest could still ack on a short-lived path, so the UI kept probing,
   but **docs/gossip never got a stable peer address** and stayed on
   `Connect(DirectJoin) … timed out`.
5. Symptom: first session “no connection / waiting for peer”; after restart a
   few messages work; continuous chat only completes after many delivery
   retries.

Mitigations in code:

- Never close a connection just because another session for that peer exists.
- On every successful chat connect/accept, `endpoint.add_node_addr` with the
  connection’s `remote_address()` so docs/gossip can dial the same path.
- Pool forget on idle/failure without force-close.

## What still needs a real networking fix

- Reliable docs/gossip when even addressed Connect fails (relay preference,
  reuse RTC path).
- Windows UDP 10040 / 1452-byte sends (PMTU, IPv6 vs IPv4, relay).
- True multipath: piggyback docs on an open chat/RTC QUIC connection.

Until then, expect some NAT asymmetry even when calls look perfect — but chat
should no longer *kill* its own working sessions.
