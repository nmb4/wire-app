# Audio pipeline platform notes

## Engine contract

Wire mixes, encodes, and decodes audio as 48 kHz stereo `f32` samples in 20 ms
chunks. One engine chunk is therefore 960 sample frames or 1,920 interleaved
samples. Device callbacks may use a different sample rate, channel count, and
buffer duration; those values must not be used as though they were engine
sample counts.

## macOS Bluetooth latency investigation (2026-07-17)

The issue was reproduced with a CMF Buds Pro 2 headset. CoreAudio selected
16 kHz mono input and output, with 320-sample callbacks (20 ms). Networking was
direct and local with sub-millisecond path latency, but audio became increasingly
delayed and arrived in bursts.

There were four interacting causes:

1. CPAL buffer sizes are sample frames, not interleaved samples. On macOS the
   requested buffer duration must be calculated from the selected device
   configuration, not the requested 48 kHz stereo engine format.
2. `fixed-resample`'s convenience resamplers use 1,024-frame input blocks. At
   16 kHz that buffers 64 ms and emits several 20 ms packets at once. The macOS
   device path now uses `FastFixedIn` with 10 ms input blocks.
3. Playback kept a three-chunk latency cushion in its ring, while the CoreAudio
   callback drained the entire ring into a private resampler buffer and played
   only one callback-sized portion. The producer immediately refilled the ring,
   so the hidden resampler queue grew by about 40 ms every 20 ms. The macOS
   callback now pulls only enough engine audio for its current device buffer.
4. The playback ring is stereo, but a mono Bluetooth output previously
   interpreted the interleaved samples as mono frames. macOS now processes
   engine-sized stereo frames and downmixes to mono before resampling.

The decoder's existing underflow recovery amplified the symptom. Batched mixer
ticks made it repeatedly add four silence ticks, but networking and Opus were
not the original source of the latency.

After the fix, a processor-enabled three-instance release trace showed:

- each 320-sample, 16 kHz mono capture callback produced one 1,920-sample,
  48 kHz stereo engine chunk;
- the private playback resampler queue remained at zero after 100 callbacks;
- decoder buffering remained bounded to one or two 20 ms packets;
- no capture or playback overruns, decoder silence insertions, lagged network
  frames, errors, or panics;
- two or three source misses per process during initial track startup only.

## Windows load-resilience investigation (2026-07-31)

A two-peer call was reported to suffer severe capture and playback degradation
while a large Rust build saturated the machine, with Windows the main target for
the investigation. Four scheduling problems made a temporary CPU stall turn
into sustained bad audio:

1. The pinned CPAL fork attempted to raise its WASAPI callback threads by
   casting `GetCurrentThreadId()` to a thread handle. `SetThreadPriority`
   therefore failed, and its return value was ignored. Wire now uses upstream
   CPAL with its `audio_thread_priority` support, which enrolls those callbacks
   in Windows MMCSS.
2. Wire's own capture and playback workers passed interleaved ring capacity to
   `audio_thread_priority`, although the API expects device frames per callback.
   They now report the actual 960-frame, 20 ms workload. This also gives macOS a
   realistic time-constraint policy instead of describing roughly 640 ms of
   audio as one callback.
3. CPAL buffer sizes are frames, but Windows requested 1,920 interleaved stereo
   samples as though they were frames. Device buffer requests now use the
   selected device rate and a real 20 ms frame count on every platform.
4. Windows workers use independent relative sleeps. An attempt to replace those
   timers with the macOS occupancy-driven capture and playback paths caused
   severe lag on both peers in a real Windows call. That part of the change was
   rolled back: Windows again uses its previous fixed-cadence workers, playback
   buffering, callback draining, and WebRTC processor framing. The CPAL upgrade,
   corrected real-time workload size, and frame-based device buffer request are
   retained because they are independent of the regressed queueing path.

The occupancy-driven approach is now macOS-only. Do not enable it on Windows
again without a real two-peer hardware test that verifies both latency and audio
quality under normal load and CPU saturation.

When the app is next tested on Windows:

1. Build and run the normal processor-enabled release.
2. Test a real two-machine voice call before treating a same-machine dev pair
   as representative. Three local processes share one physical device and can
   create feedback or doubled audio even when queue timing is healthy.
3. Run a CPU-saturating build during the call, confirm that audio remains
   realtime for several minutes, and watch for
   `capture xrun`, `playback xrun`, `audio source xrun`, `increase silence`, or
   `mediatrack recv lagged`.
4. If a regression appears, capture the selected input/output configurations,
   callback sizes, device names, and whether the path is 48 kHz stereo or
   requires resampling. Compare those facts before changing shared code.

## Trace recipe

macOS does not currently choose a persistent log directory from
`LOCALAPPDATA`, so set it explicitly when running a dev pair:

```sh
TRACE_DIR=$(mktemp -d /tmp/wire-audio-trace.XXXXXX)
LOCALAPPDATA="$TRACE_DIR" \
RUST_LOG='warn,wire::codec::opus=trace,wire::audio::playback=trace,wire::audio::capture=trace' \
target/release/wire-app --dev-pair audio-trace
```

Healthy 16 kHz mono macOS traces should settle on `get 320 push 1920`, show a
bounded `ring` value and near-zero `resampled` value in playback callback
messages, and keep decoder `now at` values near 1,920-3,840 samples. Network
path latency and connection establishment logs should be evaluated separately
from audio queue depth.
