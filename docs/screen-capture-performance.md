# Screen capture performance (Windows)

## System-wide stutter with GDI capture

On high-resolution displays (e.g. 3840×2160), we initially downscaled using **GDI `StretchBlt`** on the desktop DC before encoding. That path captures the full native framebuffer and scales on the CPU in one synchronous blit.

### Symptoms

- Entire desktop felt choppy while sharing, not just the Wire window
- Mouse cursor stuttered when dragging **any** window
- Effect was visible at the OS/compositor level

### Cause

GDI desktop capture blocks the **Desktop Window Manager (DWM)** compositor. A full-screen `StretchBlt` every frame (~16 ms at 60 fps) forces synchronous composition work on the UI thread path and competes with normal window dragging and cursor updates.

### Fix (2026-07-08)

Prefer **Windows Graphics Capture (WGC)** via `zed-scap` for all resolutions. WGC delivers frames asynchronously through the modern capture pipeline and does not stall the whole desktop the way GDI does.

GDI remains only as a **fallback** when WGC cannot start (permissions, unsupported environment).

### GPU-native sender path (2026-07-10)

On Windows, Wire now keeps WGC frames on the capture device. Frames are copied into a bounded three-texture D3D11 ring, scaled and converted from BGRA to NV12 by the D3D11 video processor, and submitted directly to the Media Foundation encoder as DXGI samples. This removes the per-frame 4K CPU readback, CPU resize, color conversion, and GPU re-upload.

The previous CPU path remains an automatic fallback when device sharing, video processing, or DXGI-surface encoding is unavailable. Local preview readback runs separately at 5 fps so it cannot hold up the encode thread.

### Local CPU regression and fallback correction (2026-07-13)

The newest log containing screen sharing showed that the NVIDIA Media Foundation encoder initialized, but the first GPU-native frames failed with `E_INVALIDARG`. After five failures the sender switched to OpenH264, producing only 2-9 fps with encode times of roughly 76-250 ms. This explained the high CPU usage seen while sharing with no active calls.

The GPU video processor now renders to a preallocated NV12 texture with the required render-target binding before copying into the Media Foundation allocator surface. If that zero-copy path still fails on a driver, the fallback is the already-proven CPU-fed Media Foundation hardware encoder; OpenH264 is only the final fallback after that path also fails repeatedly.

The encoder is now idle when no video send task is subscribed and resumes with a forced keyframe when a receiver joins. The local preview is downscaled to 640x360 on the GPU before readback, so an idle share no longer maps and copies a full 4K frame five times per second.

The custom title bar displays process CPU, GPU, and resident RAM once per second. CPU and RAM come from `sysinfo`; Windows GPU usage comes from the native `GPU Engine(*)\\Utilization Percentage` PDH counters filtered to the Wire process. The displayed GPU value is the busiest engine, rather than a sum across engines, which matches Task Manager's process-level semantics and avoids double-counting shared work.

### Preview color and Windows viewer presentation (2026-07-13)

The GPU preview surface is BGRA, while egui expects RGBA. Preview readback now swaps the red and blue channels on Windows before publishing the small preview frame; CPU/non-Windows RGBA previews remain unchanged.

The Windows viewer completes the Media Foundation decoder state machine, including delayed or multiple outputs, `MF_E_NOTACCEPTING`, stream changes, and D3D11 device-manager setup. Media Foundation's NV12 output stays on the GPU: each decoded frame is copied out of the decoder-owned sample into a bounded four-surface pool, then presented in a child window by the D3D11 video processor. Scaling and BT.709 studio-range YCbCr-to-full-range RGB conversion happen on the GPU.

This removes the steady-state GPU readback, CPU NV12-to-RGBA conversion, egui image allocation, and OpenGL texture upload. Each remote stream owns a presenter, so the existing grid can show multiple streams. egui still owns layout and controls; the native child is confined to the video rectangle and is hidden when covered, unused, or ended.

If native presentation or device sharing fails, the current GPU frame is read back and converted through the existing `yuvutils-rs` CPU path, then rendered as a normal egui texture. Non-Windows and OpenH264 decoding retain the RGBA interface. Presentation is newest-wins and bounded: decoded frames may be skipped after the decoder consumes them, but encoded inter-frame dependencies are never discarded before decoding.

### Measured checkpoint (2026-07-13)

A local release benchmark streamed a 3840x2160 display as 1920x1080 at 60 fps. Results are workload- and hardware-specific, but establish the intended steady-state behavior:

- Viewer CPU: about 2.9% (previous path: about 11.5%)
- Viewer GPU 3D: about 3.8% average (previous path: about 17.6%); Video Decode: about 6.1%
- Decoder-to-presenter handoff: 0.3-0.4 ms; native presentation: 0.7-1.0 ms average
- Sustained 60 fps during the active interval with no decode-queue or presentation drops

## IsoCast integration (2026-10-24)

The Windows sender adapts code from [IsoCast at 243aeca](https://github.com/R44VC0RP/isocast/tree/243aeca23f1d6ee6f6b7caf14b2f43133e9eb418).
Source paths, adaptations and the MIT notice are in
[`third-party/isocast`](../third-party/isocast/README.md).

- Capture admits frames by their WGC timestamps **before** the full-resolution
  GPU copy. A phase deadline preserves 60 fps on non-divisible refresh rates
  such as 144 Hz; excess frames require no Wire texture copy.
- Capture and raw encode/preview handoffs retain the newest pending picture.
  Encoded H.264 packets retain their existing ordered transport and decode path.
- Windows pacing uses a high-resolution waitable timer with absolute `Instant`
  deadlines and relative Windows due-times. It does not spin or change global
  timer resolution. OS sleep is the fallback if timer creation is unavailable.
- The MF allocator requests renderable NV12 surfaces. Compatible drivers now
  receive video-processor output directly, removing the intermediate NV12 copy.
  The previous intermediate-target copy remains available automatically. During
  the first 40 GPU inputs, the encoder interleaves both paths, discards eight
  warmup samples, and compares 16 timings per path. It selects direct rendering
  only when its median call time is more than 5% lower; otherwise it retains
  the GPU copy path. This makes the choice per encoder session and driver,
  without changing resolution, bitrate, frame rate or codec.
- Video-processor views are cached with bounded texture ownership; immutable
  processor rectangles/background are configured once.
- GPU preview scaling and readback now execute on the preview worker. Both
  threads share the protected D3D device; preview never performs a readback on
  the encoder worker. CPU frames use shared immutable buffers for handoff.
- Keyframe requests use `VT_UI4`, matching IsoCast's MF implementation. A single
  retained raw picture services keyframe requests and late joins even when WGC
  has no new desktop changes. Capture waits are bounded so stopping does not
  require another desktop update.

Local RTX 3070 checks (driver 32.0.16.1742, **debug test build**, synthetic four
BGRA textures at 3840x2160, GPU scale/encode to 1920x1080 at 60 Hz, 60 warmup
frames followed by 300 timed frames):

| Path | Mean encode call | p95 | Pictures decoded out of 360 |
| --- | --- | --- | --- |
| Original encoder, two runs | 4.20-4.28 ms | 8.18-8.50 ms | 301 |
| Direct-render encoder, two runs | 3.47-3.79 ms | 4.33-7.73 ms | 360 |
| Cached GPU-copy encoder, forced fallback | 2.60 ms | 3.36 ms | 360 |

These are local encode/decode observations, not a network or two-computer
latency benchmark. Keyframe correction also removes the baseline's initial
decode delay in this workload. GPU scheduling and clocks affect timings. The
copy-path result is why the production encoder measures both paths rather
than assuming direct rendering is always fastest.

Initial **release-build** verification decoded 360/360 pictures in all three
modes, including sampled RGB color checks. Automatic selection chose direct
rendering in that run (3.40 ms mean / 4.37 ms p95); forced direct measured
3.28 / 4.26 ms, and forced copy measured 3.83 / 5.40 ms. The preferred path can
vary with GPU clocks and contention, as the debug/release runs demonstrate.
All eight hardware/lifecycle tests passed, including idle preview in a fresh
process, static-frame recovery, and repeated start-stop memory checks.

Reproduce the GPU probe and force the intermediate-copy fallback:

```powershell
cargo test -p wire-app --lib gpu_screen_share -- --ignored --nocapture --test-threads=1
```

The ignored `static_frame_serves_late_joiners_and_keyframe_requests` test checks
recovery without new capture frames. The existing ignored capture/encoder
start-stop tests check retained memory after eight cycles. Run hardware tests
serially on an interactive Windows desktop using the Visual Studio developer
environment. Tests do not write desktop images or connect to other peers.

### Live viewer and lifecycle checks (2026-10-24)

The normal tile overlays previously disabled native video presentation at all
times. Windows GPU video now uses the native presenter while idle; hovering
reveals the existing egui controls through the CPU composition fallback. Native
regions expose the rounded border and owner avatar, and the display child
passes mouse hit tests through to egui. Dialogs, popups, tooltips and drags
restore egui composition. CPU frames and other platforms retain their previous
overlay behavior. The two CPU video fixtures are pixel-identical before/after.

Video and preview arrivals now wake a visible presenter. Previously those events
explicitly suppressed wakeups, leaving video dependent on unrelated UI/audio
events. Hidden and minimized windows still suppress these wakeups.

A release run used three isolated local dev-pair profiles (one sender, two
viewers), an animated synthetic 3840x2160 window, and 1920x1080/60 sharing on the
RTX 3070. Each phase was sampled for 90 seconds:

| Viewer path | Presented cadence | Mean CPU per viewer, percent of one logical core |
| --- | --- | --- |
| Existing tile CPU readback/upload | 29.5 fps | 61.5-61.6% |
| Native idle video, before wakeup fix | 29.6 fps | 18.4-18.5% |
| Native video with frame-driven wakeup | 56-58 fps | 42.5-43.0% |

On this 12-logical-CPU machine the last row is approximately 3.5-3.6% of total
CPU, versus 5.1% initially. It presents nearly twice as many pictures with about
30% less viewer CPU. Sender CPU remained approximately 21% of one core. The
capture source delivered about 55-58 fps; no claim of exactly 60 displayed fps
or two-computer/capture-to-display latency is implied. Transport packet age
measured after encoding was typically 0-1 ms on the local connection.

Additional live checks covered a new viewer joining a frozen source, landscape
and portrait source resizing, closing the captured window, and 30 share
start/stop cycles. All apps stayed responsive; the cycles had matching worker
starts/exits and no hardware-encoder fallback. Closing the source released all
three capture workers about 26 ms after closure detection. These checks used
virtual audio input and disabled system-audio sharing and recording.

Teardown now explicitly shuts down the MF encoder activation. Codec enumeration
also releases each COM activation reference before freeing the enumeration
array, including on errors; freeing just the array leaked the references.
Ignored lifecycle tests print process handle counts alongside retained memory
to make resource growth visible.

The final serial release run passed all **179 tests**, including all eight
hardware tests. All three GPU encode modes decoded 360/360 pictures with color
checks. Clippy and nightly formatting checks passed, and all 31 visual fixture
scenes rendered successfully. The four existing active-call fixtures and two
CPU video fixtures were compared against their before captures.

There is a remaining resource-lifecycle limitation: after explicit activation
shutdown, isolated tests still retained about five additional Windows handles
per full capture restart (previously about ten), about four in encoder-only
restarts and three in capture-only restarts. Shared-device counts are not
additive. A standalone control held handle counts flat for repeated memory
queries and D3D device creation; creating/destroying the MF sample allocator
alone retained two handles per cycle. Explicit allocator uninitialization,
manager detachment and an optional shutdown-interface probe did not remove
that growth. The capture-only remainder is not yet attributed. Memory plateau
tests and successful live cycles do **not** prove leak-free Windows teardown.

The opt-in dev-pair harness accepts `WIRE_DEV_CAPTURE_WINDOW=<window id>` and
`WIRE_DEV_CALLS_VIEW=1` alongside `--dev-auto-share`. A requested window that
cannot be resolved fails closed. `WIRE_DEV_SHARE_TOGGLE_CYCLES=<count>` repeats
the same selected target and audio preference. These variables only affect
explicit dev-pair sessions.

## Logging

Runtime logs are written per process to:

```
%LOCALAPPDATA%\wire\wire-app-<pid>.log
```

Two local instances append to separate files so send/receive paths can be correlated by timestamp and node id (`caf7327fd0`, etc.).

Development-pair sessions add the session and role to the filename:

```
%LOCALAPPDATA%\wire\wire-app-dev-<session>-<host|peer>-<pid>.log
```

## Video pipeline checklist

When debugging remote stream delivery, look for this sequence in the **sharer** log:

1. `starting video send task for <peer>`
2. `encoded first video frame (N bytes)`
3. `sent first video frame (N bytes) to <peer>`

In the **viewer** log:

1. `receiving video from <peer>`
2. `MF decoder native D3D11 presentation enabled`
3. `native D3D11 video presenter ready`
4. `decoded first video frame (WxH)`

If the native messages are missing, inspect warnings for presenter/device initialization failure and confirm that the CPU presentation fallback activated. If decoding never starts, inspect for an MF initialization/runtime fault or a missing keyframe. Windows falls back to OpenH264 after repeated MF faults and waits for the next IDR before resuming output.
