# Per-speaker call recording

Wire can record a call with **one audio file per participant**. The point is not
that it can record: it is that no speaker separation or diarization model is
needed, now or later, because the mesh already delivers each person's voice as a
separate stream.

## Why this is easy here

Calls are full mesh. Each participant's voice arrives on its own Opus
`MediaTrack` (`wire::rtc::MediaTrack`), and the local microphone is its own
capture stream. So a file written from one track contains exactly one person's
voice.

That is a stronger guarantee than source separation can give: no bleed, no
clustering mistakes, no model. It is also why a **plain single-speaker
transcription model** does better here than it would on a mixed mono file.

## What is recorded

Microphones only. Shared screen audio arrives as a separate track per peer and
is deliberately **not** recorded.

## The timeline contract

This is the part that makes a later transcription step possible.

Every tap emits **exactly one 20 ms frame of mono samples per audio tick** and
zero-pads anything short. Consequences:

- A muted tick, a starved decoder, or a packet-loss skip costs a gap in the
  transcript. It never shortens a file.
- Therefore every speaker file has the same sample count and the same origin as
  the call.
- Therefore a segment starting at `T` in any speaker's file started at
  `call_start + T` in the call.

Merging per-speaker transcripts is then just interleaving segments by time. No
offset estimation, no alignment pass.

`manifest.json` records `started_at`, `frame_ms`, and the per-speaker mapping so
this can be done without guessing.

A caveat worth stating: each file advances on its own 20 ms tick, paced by the
engine clock (and, for the microphone, by the capture hardware clock). Those can
drift by a few tens of milliseconds per hour. Fine for merging transcripts;
not a substitute for sample-accurate sync.

## Format

48 kHz mono 16-bit PCM WAV, canonical 44-byte header, sizes patched on close.
This is what whisper.cpp, faster-whisper, and similar tools read directly. It is
also uncompressed: **~345 MB per hour per speaker**, so a long meeting with
several people adds up.

Files are created lazily on a speaker's first frame, so someone who never speaks
leaves no empty recording behind.

## Architecture

Recording is deliberately **decoupled from playback**:

| Source | How it is tapped | Why |
| --- | --- | --- |
| Local microphone | extra `AudioSink` on the capture loop (`AudioContext::add_capture_sink`) | added on demand, independent of how many call tracks exist |
| Remote participant | a private `MediaTrackOpusDecoder` on a resubscribed track | does not steal packets from, or inherit deafen/volume from, what the user hears |

Because the remote decoder is private and the track is a `broadcast` receiver
clone, recording can be **started, stopped, and started again at any point in a
call**, including before anyone joins. It never disturbs the audio path.

Consequences worth knowing:

- The microphone is tapped **before** the mute gate, so muting never removes your
  own voice from a recording. Muting is a transmission control, not an input
  control.
- Deafening yourself and turning a participant's volume down do not affect the
  recording. A meeting is still captured when you listen on headphones.

### Threads

```
capture loop ──► RecordingTap ──bounded queue──► writer thread ──► WAV file
recorder thread ─► RecordingTap ──bounded queue─► writer thread ──► WAV file
```

- `RecordingTap` runs on an audio-facing thread and only ever `try_send`s. It
  never blocks on disk.
- Each writer owns one file and does all the I/O. On queue saturation the tap
  **drops** the frame and counts it (`dropped_frames` in the manifest) rather
  than stalling audio. The queue holds ~5 s, so this should never happen.
- Files are finalized on a stop flag. Dropping the tap closes the queue, which is
  what triggers the header patch, so the file is valid even if a recorder thread
  dies unexpectedly.

## Behaviour across a call

| Situation | What happens |
| --- | --- |
| Recording started mid-call | peers already present attach immediately from their stored tracks |
| Peer joins while recording | a file is created as their track arrives |
| Peer leaves | their file stops growing; it stays in the manifest with its duration |
| Peer drops and rejoins | the new track is swapped into the **same** file, so one person is still one file. Without this their file would go silent for the rest of the meeting and look like they had said nothing |
| Peer reconnects on a new call generation | same as above; a stale track from a superseded call is ignored |
| Manual recording, all calls end | recording continues (local microphone only) and stays stoppable |
| Auto-record, last participant leaves | recording finalizes |
| Auto-record preference switched off | only auto-started recordings are ended, never a manual one |
| App quits while recording | files are finalized before the runtime tears down, so no truncated WAV is left behind |
| Disk full or write error | that speaker's file is reported with an error; other speakers continue |

## Files on disk

```
<config dir>/recordings/2026-10-03T14-32-05-123Z/
├── 00-Ada (you).wav
├── 01-Alan Turing.wav
├── 02-Bob.wav
├── manifest.json
└── README.txt
```

Files are named from whatever is known when they are created (often a short node
id) and renamed to the resolved display name when the recording stops, because
that is the first point where presence has told the worker who everyone is.

Timestamps in directory names are UTC. Wire has no timezone database, and an
unambiguous instant is more useful here than a local-looking one that could be
wrong.

## Known gaps

- **Nobody is told.** The recording indicator is local only. Nothing in the call
  protocol tells a peer that this device is recording them. Settings says so
  plainly. Adding this means a presence flag plus a banner on the other side.
- **Screen-share audio is not recorded.**
- **No retention policy.** Recordings accumulate until the user deletes them.
  Settings shows the folder and its last entry; there is no automatic cleanup.
- **Uncompressed.** See the size note above.

## What to check by hand

Automated tests cover framing, silence padding, WAV headers, timeline
alignment, and the rejoin swap. These need a real call:

1. Start a 2-person call, record for a minute, stop. Each file plays, and each
   contains only that person.
2. Start recording *after* the call is already running. Both files appear.
3. Have one participant rejoin mid-recording. Their file continues rather than
   going silent.
4. Mute yourself mid-recording and keep talking. Your file still contains it.
5. Leave the call while recording by hand. The REC pill stays, and the button
   still stops it.
