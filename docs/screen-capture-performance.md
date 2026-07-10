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

### Trade-off

On 4K → 1080p/1440p streams, WGC still captures at native resolution; we downscale in our pipeline (`fast_image_resize`). That costs CPU but keeps the system responsive. GPU downscale (D3D11 video processor) is the next step for higher stream FPS on large monitors.

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

If (2) is missing on the viewer, the encoder and decoder bitstream formats likely do not match (e.g. OpenH264 encode + Media Foundation decode). Both sides should use OpenH264 until MF encode/decode is paired.