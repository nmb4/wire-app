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
