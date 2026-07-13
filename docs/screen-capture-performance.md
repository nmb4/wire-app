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

The custom title bar displays process CPU, GPU, and resident RAM once per second. CPU and RAM come from `sysinfo`; Windows GPU usage comes from the native `GPU Engine(*)\\Utilization Percentage` PDH counters filtered to the Wire process.

### Preview color and Windows viewer decode (2026-07-13)

The GPU preview surface is BGRA, while egui expects RGBA. Preview readback now swaps the red and blue channels on Windows before publishing the small preview frame; CPU/non-Windows RGBA previews remain unchanged.

The Windows viewer now completes the Media Foundation decoder state machine, supplies the required placeholder output type before the first input, handles delayed/multiple outputs and stream changes, and attaches a D3D11 device manager. Decoded NV12 is converted directly into the display buffer with the SIMD-enabled pure-Rust `yuvutils-rs` path. This removes the previous MF video-processor round trip and its several full-frame allocations and copies.

## Logging

Runtime logs are written per process to:

```
%LOCALAPPDATA%\wire\wire-app-<pid>.log
```

Two local instances append to separate files so send/receive paths can be correlated by timestamp and node id (`caf7327fd0`, etc.).

## Video pipeline checklist

When debugging remote stream delivery, look for this sequence in the **sharer** log:

1. `starting video send task for <peer>`
2. `encoded first video frame (N bytes)`
3. `sent first video frame (N bytes) to <peer>`

In the **viewer** log:

1. `receiving video from <peer>`
2. `decoded first video frame (WxH)`

If (2) is missing on the viewer, inspect decoder warnings for an MF initialization/runtime fault or a missing keyframe. Windows falls back to OpenH264 after repeated MF faults and waits for the next IDR before resuming output.
