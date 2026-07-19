# Why chat still retries when the other person is online

A short, plain-language note for product and debugging. Networking jargon kept light.

## The offline case (easy)

If their Wire app is closed or unreachable, we cannot hand them the message yet.
We save it locally, show **queued**, and only probe slowly until they come back.
That part is obvious.

## The confusing case: they are online

“Online” only means: **their app is running and the network can sometimes reach
them.**

It does **not** mean: **every kind of connection from you to them always works.**

So the UI can look wrong: they are in a call with you, or messaging works one
way, and your bubble still sits on “sending / retrying” for a moment.

## What “send” actually means here

Hitting Send is not one atomic “delivered to their phone” step. Roughly:

1. **Your app saves the message** — instant and durable on your machine.
2. **Their app has to actually receive it.**
3. **Their app has to tell you “got it”** — only then does your UI treat it as
   fully delivered.

Retries are about steps 2 and 3, not about re-typing or distrusting the text.

## Two different doors

Wire uses more than one way for two machines to talk:

1. **Call / video door** — once open, both sides can usually use that session
   both ways. That is why voice and screen share often felt fine even when chat
   felt slow one direction.
2. **Chat sync door (older main path)** — chat historically leaned more on
   “**I** open a sync channel **to them** and push/pull the shared message log.”

So you can be mid-call (door 1 open) while **your** attempt to open the chat
sync door (door 2) still fails, times out, or takes seconds.

**Presence is not the same as every channel working.**

## “Can hear them but can’t cleanly open a channel”

Home routers and the public internet often make **inbound and outbound** paths
unequal:

- **They dial you** → your app accepts → works.
- **You dial them** → their network blocks, drops, or only half-opens the path
  → fails or stalls.

That matches what we saw in real sessions:

- Their messages arrive on you quickly (they opened the door).
- Your messages lag until **they** pull, or until a short wake/ping gets through
  and they open things from their side.

A useful mental picture: two people in a noisy room. You can hear them shout,
but when **you** try to start a careful back-and-forth, the door is sticky,
one-way, or only opens if **they** push first.

## What a retry means when they are online

Usually one of:

1. **Your first knock did not fully connect** — timeout, flaky path, sticky NAT.
2. **They may already have the body, but their “got it” confirmation has not
   come back yet.**
3. **A brief blip** — Wi‑Fi hiccup, sleep, app busy — the next try works.

Not: “the app is unsure what you typed.”  
Yes: “I still need a clean round-trip with their machine.”

## What we changed so this hurts less

- **Chat ALPN fast path** — hand the message (and receipts) on the short chat
  wake that already tends to work, instead of waiting only on the sticky docs
  sync dial.
- **Offline queue** — if wakes keep failing, stop aggressive “delivery retry N”
  spam; show queued and probe slowly.
- **UI** — small status icons next to the message instead of debug strings
  under every bubble.

Docs sync remains the durable multi-device history path. Live DM delivery no
longer has to depend on the worst door alone.

## Related

- `docs/chat-delivery-asymmetry.md` — evidence and NAT/path notes
- `docs/chat-offline-queue.md` — queued vs pending vs retrying
- `docs/chat-keepalive-sessions.md` — keeping the chat wake path warm
