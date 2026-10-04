# GPUix migration & video-performance research

Date: 2026-10-03. Read-only research (no code changed). Visual companion:
`Documents/Wire video pipeline.tldraw` (3 rows: current path, gpuix port, options).

Updated 2026-10-04: added `66HEX/frame` findings to §5, §6 and §7. Still no code
changed.

## 1. Question

How hard would a migration from the current stack to [gpuix.dev](https://gpuix.dev)
(React + TypeScript on Zed's GPUI via napi + Bun) be, with screen-share/video
performance as the hard requirement? Linux is not a priority.

## 2. Current Wire stack (what would be migrated)

- UI: `egui 0.33.3` + `eframe 0.33.3` + `winit 0.30.13` + `wgpu 27.0.1`,
  immediate-mode Rust, single process (`wire-app/Cargo.toml:24,48,55`,
  `wire-app/src/main.rs`, `wire-app/src/app.rs`).
- UI size ~13k lines: `app.rs` ~3756 (all `AppState`, `eframe::App::update`
  pumps `ServiceClient` each frame), `app/chat_ui.rs` ~3866, `app/calls_ui.rs`
  ~2874, `settings_ui.rs` ~1054, `profile_ui.rs` ~1053, `widgets.rs` 829,
  `theme.rs` 859, `title_bar.rs` 338, `window_frame.rs` ~255,
  `notifications.rs` ~1626 (own transparent viewport + toasts).
- Windows video fast path (the thing to preserve):
  `wire-app/src/video_decode.rs` (MF hardware decoder preferred, OpenH264
  fallback, strict arrival order, never skip — skipping breaks the reference
  chain), `wire-app/src/win_video_presenter.rs` (zero-copy D3D11
  `VideoProcessorBlt` into a swapchain on a child HWND, `Present(DO_NOT_WAIT)`
  with drop-on-busy, never blocks), `calls_ui.rs:2642 sync_rgba_texture`
  (egui-texture fallback with `uploaded_generation` skip + fps/avg/p95 stats).
- Reusable backend (no egui deps, could move into a napi sidecar as-is):
  whole `wire/` crate (audio/cpal/opus, video codec/transport, RTC-over-QUIC,
  net/config), `runtime/{mod,worker,video}`, `host.rs` (single-instance lock),
  `chat.rs` (+`blob_provider.rs`), `client_status.rs`, `profile.rs`,
  `persistence.rs`, `screen_capture.rs`, `scap_capture.rs`, `win_capture.rs`,
  `win_gdi_capture.rs`, `win_mf_codec.rs`, `win_mf_d3d.rs`, `yuv_convert.rs`,
  `video_decode.rs`, `system_audio.rs`, `update.rs`, `peer_update.rs`,
  `sounds.rs`, `resource_monitor.rs`, `autostart.rs`, `dev_pair.rs`.
  `Command` (33 variants) / `Event` (20+ variants) in `runtime/mod.rs` is
  already a clean IDL for a napi boundary. `[lib] crate-type = ["lib","cdylib"]`
  already exists. Thin adapters needed: `tray.rs`, `activation.rs`,
  `global_hotkeys.rs` (drop `egui::Context` wake → napi callback), `klipy.rs`,
  `win_video_presenter.rs` (needs GPUI surface/HWND). Rewrite in TS/React:
  all of `app.rs` + `app/`, notifications/toasts, `overlay_window.rs`,
  theme/title-bar/window-frame, `hidden_event_loop.rs`.

## 3. gpuix.dev assessment: hard, full rewrite + feature loss

- Stack: TypeScript React (`@gpuix/react`) or Solid 1.9, Rust GPUI (pinned Zed
  fork) via napi-rs, dev `bun --hot`, ship `bun build --compile`. Pre-1.0
  (`0.10.0`): pin adapter + `@gpuix/native` to the same exact version.
  UI **must** be JS; Rust logic survives only as a sidecar `.node` or a fork
  of `gpuix-native` (both unverified under `bun build --compile`).
- Supported: chat lists (`<virtual-list>`), `<img>` file/data-URL/http,
  `<markdown>/<code>/<diff>`, native `<input>` + IME, flexbox/Taffy,
  `motion.div` (numeric targets only), fullscreen/minimize/zoom,
  `promptForPaths()`, young `checkUpdate()` auto-update (GitHub Releases +
  `.sig`, no relaunch).
- Partial: frameless (`titlebarTransparent`, `transparent/blurred`), clipboard
  (no explicit API), scroll (no nested scrolling).
- Missing: per-frame video/canvas/texture element (only
  `img.setImagePixels(w,h,RGBA/BGRA)` CPU upload), multi-window (one renderer
  = one window, `createRoot` throws on second), tray icon, notifications,
  global hotkeys, autostart, single-instance lock, always-on-top overlay
  (except Wayland layer-shell). Only 3 prebuilt napi targets; Linux ignores
  `focus:false/show:false`.
- Effort: napi sidecar moderate; UI rewrite very large (all `app/*.rs` →
  React, ~months); gap-fill means forking pre-1.0 native code with ongoing
  maintenance; video quality regression likely (see §4).

## 4. Zed-on-Windows deep proof: CPU path, not zero-copy

Zed is fast on macOS and slow on Windows — different code paths, verified in
`zed/crates/livekit_client`:

- `remote_video_track_view.rs`: `#[cfg(macos)]` returns
  `gpui::surface(CVPixelBuffer)` (IOSurface zero-copy);
  `#[cfg(not(macos))]` returns `gpui::img(Arc<RenderImage>)` with
  `current/previous_rendered_frame` + `drop_image` bookkeeping.
- `livekit_client/playback.rs`: macOS `RemoteVideoFrame = CVPixelBuffer`
  (pool + `i420_to_nv12`, native-buffer fast path, no copy). Everywhere else
  `RemoteVideoFrame = Arc<RenderImage>`: `alloc(w*h*4)` +
  `buffer.to_argb()` (YUV→ARGB on CPU via libwebrtc/libyuv) +
  `RgbaImage::from_raw` + `RenderImage::new()` — a new `ImageId` per frame.
  Only filter: `<10x10` black flash. Every frame does
  `latest_frame = Some(frame); cx.notify()`. No generation skip, no busy-drop,
  no hidden-window discard (Wire has all three).
- Atlas churn is Zed-acknowledged: commit `e4ebd3aa` (#53088) — "when atlas
  tiles are rapidly allocated and freed (e.g. watching a shared screen in
  Collab)… panicked at `wgpu_atlas.rs:231: texture must exist". Fix drains
  pending uploads / skips missing textures: crash fix, not perf fix.
- Windows atlas (`gpui_windows/src/directx_atlas.rs`): `Polychrome →
  B8G8R8A8_UNORM`, per-tile `UpdateSubresource` CPU→GPU copy; 1080p forces a
  large allocation (default atlas 1024px, max 16384). Backend is DirectX 11
  (see `zed.dev/blog/windows-progress-report`), so compositing is GPU but
  every video upload is a CPU copy.
- Math: 1080p BGRA = 8.3 MB/frame → ~250 MB/s at 30fps, ~500 MB/s at 60fps
  of alloc + convert + upload, plus atlas alloc/free. Wire: zero CPU bytes
  (NV12 stays on GPU, `VideoProcessorBlt` + non-blocking `Present`).
- Why Zed gets away with it: screenshare published as VP8 with
  `maintain-resolution` at low fps for static screens. Wire wants call-grade
  1080p30/60 across tiles — 4–10x the frame rate, sustained.
- To turn code proof into numbers (not done — research-only): instrument
  `video_frame_buffer_from_webrtc` convert ms + atlas upload ms at
  1080p30/60 on Windows with PresentMon vs `win_video_presenter.rs:log_stats`
  and `sync_rgba_texture` fps/avg/p95.

## 5. Ecosystem: gpui-kit, community GPUI, gpui-ce apps, video crates

- **GPUI Kit** (`longbridge/gpui-kit`, ~15.7k stars, ships Longbridge Pro):
  `gpui-kit` facade re-exporting pinned GPUI + `gpui-base` + `gpui-component`
  (75+ components) + assets. Helps enormously *around* video (dock, tables,
  dialogs, themes, multi-window docs, `SystemNotification` with tag/actions
  and per-OS caveats, wry `WebView`, packaging/auto-update,
  `gpui_kit::test`, fps HUD, `ImageSource::Render/Custom` + cache +
  `drop_image` primitives) but has **no live-frame element**. Key
  architectural win over gpuix: Rust in-process, so decode stays next to
  paint with no napi copy.
- **Community GPUI**: `gpui-ce/gpui-ce` (~1.1k stars, `cargo add gpui-ce`) =
  independent versioning/governance, API-compatible for now; value is release
  process, not video tech. `longbridge/gpui-fast` (experimental, 116 stars) =
  retained-mode (up to ~94% idle CPU saving) + window composition
  (`zed#62379`, native views inside GPUI windows — the principled version of
  the child-HWND trick); watch, don't depend on.
- **Atlas tile reuse via `content_version`** (found 2026-10-04 in `66HEX/frame`'s
  gpui-ce fork, not upstream) is the fix §4 only got a crash patch for. Its fork
  adds `RenderImage::new_image_id()`, `new_with_id(id, content_version, data)`
  and `content_version()`
  (`vendor/gpui-ce/crates/gpui/src/assets.rs`; the field is documented as
  "monotonic content version for renderers that can update an existing atlas
  tile"), consumed at `window.rs:4230` via
  `sprite_atlas.get_or_update_with(&params, data.content_version(), …)`. In
  `directx_atlas.rs:101` and `wgpu_atlas.rs:134`, an existing tile of unchanged
  size takes a plain `UpdateSubresource` into the *same* bounds; realloc happens
  only on size change, and an unchanged version skips the upload entirely. So
  one stable `ImageId` per video tile plus a monotonic counter gives a fixed
  atlas slot per frame — killing the churn that caused `wgpu_atlas.rs:231` in
  Zed, not just papering over it. Verified **not upstream**: gpui-ce `main` has
  neither `content_version` nor `new_with_id`, zero upstream issues/PRs mention
  it, and gpui-ce publishes no Releases (consumers pin a git rev).
- **`66HEX/frame`** (FFmpeg GUI, ~2.0k stars, v0.33.1, ships WinGet / Homebrew /
  Flathub with a real Windows build) is the closest shipping non-toy GPUI-CE app
  doing live video on Windows. It is an *app*, not a library, and it is
  **GPL-3.0-or-later** while Wire is MIT/Apache — so read it for design and
  reimplement anything worth taking. Concrete value:
  - **A live-frame path that works on Windows** (`frame-app/src/preview_engine/`):
    ffmpeg subprocess → `-pix_fmt bgra -f rawvideo`, scaled in the filter graph
    → BGRA `RenderImage` → `window.paint_image_transformed(…, Arc<RenderImage>, …)`
    (`app/preview_panel/viewport.rs`). Direct `Arc` into paint, no `ImageCache`,
    in-process, no napi hop.
  - **Frame-drop accounting Wire already has, plus the missing benchmark
    harness**: `preview_engine/frame_store.rs` (`LatestFrameStore` — generation,
    `mark_presented`, `overwritten_before_present`) is the same shape as
    `sync_rgba_texture` / `uploaded_generation` (`calls_ui.rs:2746`), arrived at
    independently. `preview_engine/metrics.rs` is a ready-made per-stage timing
    schema (read/convert/present µs, first-frame ms, drops) for §7's benchmark.
  - **The fork workflow, demonstrated rather than feared**: root `Cargo.toml`
    does `[patch."https://github.com/gpui-ce/gpui-ce"]` → `vendor/gpui-ce/*`
    (361 files checked in). Maintaining a gpui-ce fork is a solved, shipping
    exercise.
  - **Confirms option 5's window story**: `app/chrome.rs` uses
    `WindowDecorations` with per-OS titlebars (mac/windows/linux), so frameless
    custom chrome on Windows works. Notifications come from `notify-rust` and
    dialogs from `rfd` — crates, not framework features.
  - **What it does not change**: `elements/surface.rs` is `#[cfg(macos)]`
    `CVPixelBuffer` plus linux/freebsd `Texture` only, so there is **no Windows
    surface path** and option 6 stays open. Bytes are still `w*h*4` BGRA every
    frame (scaling is pushed into ffmpeg, not skipped), and its own defaults cap
    at 1280×720 @ 30fps (`preview_engine/types.rs`) — below Wire's 1080p30/60
    call-grade target, so §4's math stands. Single window, no tray/hotkeys. Live
    semantics don't transfer: it previews *files* by spawning and killing ffmpeg
    per seek, with no arrival-order / never-skip reference chain.
- **Video crates**: `cijiugechu/gpui-video-player` (92 stars, GStreamer file
  playback, `Video::new(&uri)`, `buffer_capacity`, macOS CVPixelBuffer HW /
  sprite-atlas software) — wrong abstraction for live QUIC frames + heavy
  system deps; buffering/element pattern reusable only.
  `freesurfacemodules/gpui-video` (FFmpeg+CPAL fork, lighter) — still
  file-oriented. `zortax/gpui-video` (3 stars, Linux-only, patched GPUI fork)
  is the technical ideal (VA-API DMA-BUF / NVDEC CUDA zero-copy, NV12 compute
  shader into `Surface`) but has no Windows/macOS story.
  `fran0220/gpui-box` `media::VideoPlayer` is chrome-only by its own docs
  ("does not decode a frame"; host supplies `frame()` per-frame) — usable as
  tile chrome, not a decoder.

## 6. Options to make a port work (cheapest first)

1. **BGRA + binary + gating** (days, no fork): BGRA decoder output (skip R/B
   swap), raw napi buffers (never base64), generation skip + pause hidden
   tiles. Ceiling ~1–2 tiles at 1080p30.
2. **Less work** (product lever, multiplies everything): active-speaker
   fullscreen, 360p thumbnails, pause hidden tiles.
3. **Native overlay** (days, fragile): keep `win_video_presenter.rs` under a
   GPUI hole. Breaks with transforms/rounded corners/overlays.
4. **NV12 GPU convert** (1–3 wks + gpuix fork): ship YUV planes, convert in
   shader (~60% fewer bytes; matches MF native format).
5. **Rust GPUI instead** (re-scope): sidecar becomes the app via `gpui-kit`;
   no napi copy at all. `66HEX/frame` is the existence proof and de-risks this
   option specifically: it ships a Windows GPUI-CE app with a live BGRA video
   path and a frameless custom titlebar, and its gpui-ce fork carries the
   `content_version` atlas fix from §5 — so the atlas-churn risk stops being a
   reason to hesitate. It does not supply zero-copy (§5), and its 720p30 default
   is not evidence that 1080p60 holds.
6. **Zero-copy import** (weeks–months + renderer fork + driver testing):
   D3D11 shared-handle → wgpu on Windows, `CVPixelBuffer` surface on macOS.
   Restores today's perf.

Standing recommendation: 1+2 to find the ceiling, then 4 or 5; 6 only if
full-density screen share is the product.

Note on the atlas primitive from §5: it lives in **gpui-ce**, so it is free with
option 5 but would have to be ported into `gpuix-native`'s copy of the same
atlas code for options 1, 4 or 6. It is not a reason to prefer one over the
other — it is roughly 30 lines either way.

## 7. Measured: GPUI CPU-upload spike (2026-10-04)

Spike: `spikes/gpui-atlas-video-bench/` (own branch, not in Wire's build).
gpui-ce `main` @ `254b5dbd`, vendored + patched with the reimplemented
`content_version` in-place tile (§5). One window, one 1920x1080 BGRA image
(8_294_400 B/frame) painted full-window, fresh alloc + full byte rewrite per
frame (Zed-like), release build, 20 s runs, 1:1 device pixels (1280x720
logical window on the 1.5x display). `fixed` = one `ImageId` + monotonic
version (Step-2 primitive); `control` = fresh `RenderImage::new` per frame +
`drop_image` of the superseded tile (Zed's current/previous shape, one live
tile in both modes). First second excluded as warmup. PresentMon is not
installed here, so pacing is in-process intervals only.

| run | publ. | pres. | drops | fps avg | interval avg/p95 | build avg/p95 | upload avg/p95 | MiB/s | peak WS |
|---|---|---|---|---|---|---|---|---|---|
| fixed 30fps | 600 | 600 | 0 | 30.05 | 33.34 / 37.82 ms | 2.01 / 2.38 ms | 0.56 / 0.74 ms | 237.7 | 81 MB |
| fixed 60fps (x4) | 1200 | 1108–1117 | 83–92 (6.9–7.7%) | 55.5–55.9 | ~18.0 / ~33.1 ms | ~2.0 / ~2.3 ms | ~0.54 / ~0.69 ms | ~441 | 80–85 MB |
| control 30fps | 600 | 600 | 0 | 30.04 | 33.34 / 45.14 ms | 2.08 / 2.36 ms | 0.51 / 0.70 ms | 237.7 | 78 MB |
| control 60fps (x2) | 1200 | 1140, 1147 | 60, 53 (~5%) | 57.2, 57.4 | ~17.5 / ~30 ms | ~2.08 / ~2.3 ms | ~0.54 / ~0.67 ms | ~453 | 86 MB |
| Wire D3D11 presenter, 1080p60 (**from prior runs**, `docs/screen-capture-performance.md` 2026-07-13) | — | 60fps sustained | 0 | 60 | present 0.7–1.0 ms avg, decode→present handoff 0.3–0.4 ms | 0 (zero-copy) | 0 (zero-copy) | 0 | — (viewer CPU ~2.9%, GPU 3D ~3.8%) |

Verdict:

- **1080p30: yes, comfortably.** 600/600, zero drops, per-frame CPU work
  (alloc+fill+wrap ~2.0 ms + atlas upload ~0.56 ms) is ~8% of the 33.3 ms
  budget. The ~250 MB/s figure from §4 is real (measured 238 MiB/s) and
  simply fits.
- **1080p60: no — ~56fps, ~7% drops.** But the bottleneck is *not* the upload
  bytes: the 8.3 MB `UpdateSubresource` measures 0.54 ms avg (3% of the
  16.6 ms budget). Every drop lines up 1:1 with a ~33.3 ms present interval
  (vsync miss on the 60 Hz display) plus a matching ~28 ms publish outlier:
  the frame driver shares the UI thread, so one missed vsync delays the next
  publish past its deadline and the slot is lost. The CPU path has headroom;
  the pacing architecture has none.
- **Was Step 2 worth it? On throughput: no measurable win** — control
  (alloc+free per frame) ran 57.2–57.4fps vs fixed 55.5–55.9fps; the delta is
  small and, if anything, favours control (possibly a same-tile
  write-after-read hazard vs fresh tiles — unproven). Its value is stability,
  not speed: one fixed slot, no churn, kills the alloc/free crash class from
  §4 without costing anything measurable. Working set is flat (~80 MB) in both
  modes with no growth over 20 s.
- Sanity: measured throughput (238/441 MiB/s) matches the §4 arithmetic
  (248.8/497.7 MB/s) within vsync/pacing losses — no clipped/skipped-image
  artefact inflating the numbers (full-window paint, 1:1 device px verified
  via `scale_factor=1.5` log).

Caveats / not measured:

- PresentMon absent (no present-pacing data beyond in-process intervals).
- Window-foreground state from a non-interactive launch is uncontrolled;
  `activate(true)` did not move the 60fps numbers.
- Bench artifacts, not measured-path costs: hybrid sleep+spin frame driver
  (pure Sleep caps at ~21fps on stock 15.6 ms timer granularity — measured
  46.9 ms/21 fps before switching; pure spin starves renders: 600 publishes,
  1 present), `timeBeginPeriod(1)` for the driver, one spinning core.
- Wire-side numbers above are the documented 2026-07-13 local benchmark, not
  fresh measurements (needs two peers + manual setup).
- Single tile only. 4–10 tile grids (Wire's real target) multiply everything
  above and were not tested — the 60fps single-tile result already says the
  per-tile budget does not exist for that.

Remaining open threads:

- Unverified: second `.node` under `bun build --compile`; Hermes path
  (no `fetch`/TLS, CJS-only) if binary size matters (Bun `.app` ~82 MB vs
  Hermes ~34 MB per gpuix docs).
- Decision: gpuix fork (options 4/6) vs Rust GPUI re-scope (option 5).
