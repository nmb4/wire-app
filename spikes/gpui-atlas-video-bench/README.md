# gpui-atlas-video-bench

Spike: does the GPUI-style CPU BGRA upload path sustain 1920x1080 at 30/60fps
on Windows, with a fixed-size in-place atlas tile, vs what Wire does today?

NOT part of the `wire-app` / `wire` build. Detached workspace (`[workspace]`
in this crate's `Cargo.toml`), no shipping Wire source touched.

## Pinned upstream

- gpui-ce `main` @ `254b5dbd47cbb5acbcc5bbdcbb322a339276c88a` (resolved 2026-10-04;
  also the value of `HEAD` at clone time).
- Vendored at `vendor/gpui-ce/` (excluded from the workspace so cargo never
  builds the vendor workspace itself).
- Direct git deps + `[patch."https://github.com/gpui-ce/gpui-ce"]` to the
  vendor paths, so the Step-2 fork is what actually compiles.

Deviations from the brief's sketch (same behaviour, adapted to the pinned rev):

- Dep names need `package =` renames: the crates are published as `gpui-ce` /
  `gpui_ce_*` (lib name `gpui`), so `gpui = { git = ... }` does not resolve.
- No `font-kit` feature exists on `gpui_ce_platform` (or `gpui-ce`) at this
  rev; text uses the default fontique/parley stack. Dropped, not needed.
- Patch keys are the real package names (`gpui-ce`, `gpui_ce_windows`, ...).
- This rev has no `paint_image_transformed` for images; the image path is
  `paint_image` -> `paint_image_with_corner_smoothing` (same atlas lookup,
  same `PolychromeSprite`), which is what Step 2(b) patches.
- Kept `get_or_insert_with` for glyph/SVG/old callers; `get_or_update_with`
  is a new trait method with a default impl that ignores the version, so the
  Metal/headless/test atlases compile untouched. Only DirectX (Windows) and
  wgpu implement the real three-case logic.
- Added `validate_upload` before the in-place upload / realloc paths too,
  preserving the "rejected bitmap never leaves a cached tile" invariant.

## Step-2 fork (reimplemented from the description; no Frame code copied)

- `vendor/gpui-ce/crates/gpui/src/assets.rs`: `RenderImage` gains
  `content_version`, `new_image_id()`, `new_with_id(id, version, data)`,
  `content_version()`; `new()` = `new_with_id(new_image_id(), 0, data)`.
  `PartialEq` still compares only `id`.
- `vendor/gpui-ce/crates/gpui/src/platform.rs`: `PlatformAtlas` gains
  `get_or_update_with(key, content_version, build)` (default: delegate).
- `vendor/gpui-ce/crates/gpui/src/window.rs`: image paint path passes
  `data.content_version()` through.
- `vendor/gpui-ce/crates/gpui_windows/src/directx_atlas.rs` and
  `vendor/gpui-ce/crates/gpui_wgpu/src/wgpu_atlas.rs`: `content_versions_by_key`
  alongside `tiles_by_key`; same-version skips upload, same-size uploads in
  place (`UpdateSubresource` / queued `write_texture`), size change
  reallocates; cleared on reset, dropped on remove.

## What the bench does

One window, one image, fixed 1920x1080 (8_294_400 bytes/frame):

- Per frame: fresh `Vec<u8>` alloc (like Zed's `alloc(w*h*4)`), every byte
  written (B/G gradients + moving R pattern), wrapped in `image::Frame` ->
  `RenderImage`, painted full-window via `window.paint_image` from a `canvas`
  element (direct `Arc` into paint, no `ImageCache`, like `frame`'s viewport).
- `fixed`: one `ImageId` + monotonic `content_version`, never dropped.
- `control`: fresh `RenderImage::new` per frame; the superseded tile is dropped
  right after the replacement paints (Zed's current/previous + drop_image
  shape), so exactly one live tile exists in both modes. Without this the
  control would OOM (1200 tiles x 8.3MB at 60fps/20s); the drop is what makes
  the comparison about churn, not leaks.
- Metrics (see `src/metrics.rs`): `frames_published`, `frames_presented`,
  `frames_dropped` (= published - presented), `frames_overwritten_before_present`,
  `bytes_per_frame`, `total_bytes_uploaded`, `build_render_image_us` (alloc +
  fill + wrap), `upload_us` (`paint_image` wall time, dominated by the atlas
  upload), `frame_interval_us` + derived fps avg/p95 (Wire's index rule),
  plus `publish_interval_us` (driver-side pacing, to separate driver jitter
  from present stalls). First second skipped as warmup (size-change realloc +
  window startup).
- `upload_us` is timed around `paint_image`, not inside the atlas call: for a
  full-window 1080p image the snap + primitive insert is microseconds next to
  the 8.3MB `UpdateSubresource`.
- Driver artifacts (in `main.rs`, not the measured path): hybrid sleep+spin
  frame pacing (pure Sleep caps at ~21fps on stock 15.6ms timer granularity —
  measured 46.9ms intervals before switching; pure spin starves renders: 600
  publishes, 1 present), `timeBeginPeriod(1)` via raw winmm FFI (no new deps)
  so the sleeps land within ~1ms, 3ms spin window because the driver task
  shares the UI thread, and `activate(true)` (did not move the numbers from a
  non-interactive launch).

## Running

Release only (debug numbers are meaningless here):

```powershell
cargo run --release --manifest-path spikes/gpui-atlas-video-bench/Cargo.toml -- --mode fixed --fps 30 --seconds 20
cargo run --release --manifest-path spikes/gpui-atlas-video-bench/Cargo.toml -- --mode fixed --fps 60 --seconds 20
cargo run --release --manifest-path spikes/gpui-atlas-video-bench/Cargo.toml -- --mode control --fps 30 --seconds 20
cargo run --release --manifest-path spikes/gpui-atlas-video-bench/Cargo.toml -- --mode control --fps 60 --seconds 20
```

On a 1.5x display, add `--win-width 1280 --win-height 720` so the 1920x1080
image maps 1:1 to device pixels (window size is logical, image size is device).
The runs behind `docs/gpuix-migration-research.md` §7 used those flags.

The JSON report prints to stdout on exit. Keep the window in the foreground and
unoccluded: an occluded window may not present, which would read as drops.

Working set: sampled externally while the bench runs (no extra deps in the
bench binary):

```powershell
powershell -File spikes/gpui-atlas-video-bench/scripts/sample-rss.ps1 -Name gpui-atlas-video-bench -Seconds 25
```

PresentMon is not installed on this machine, so present pacing comes from the
in-process intervals only.

## Licence note

The `content_version` in-place tile design was observed in `66HEX/frame`
(GPL-3.0-or-later). Only the described behaviour was reimplemented here from
the task brief against upstream gpui-ce sources; no Frame code was copied or
vendored. gpui-ce itself is Apache-2.0.
