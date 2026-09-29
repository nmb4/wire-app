# Floating panel cleanup

Floating forms use one frame and viewport sizing policy. Settings and contacts
keep their fixed header; settings also keeps Save changes outside its scroll area.
Other forms keep the native egui title and close control above scrollable content.
Image preview remains movable and resizable.

## UI audit

| Severity | Location | Before | After | Principle and user impact |
| --- | --- | --- | --- | --- |
| HIGH | `wire-app/src/app/widgets.rs:141`, `settings_ui.rs:35`, `calls_ui.rs:61`, `chat_ui.rs:2869`, `profile_ui.rs:476` | Inconsistent minimum widths and unbounded form/list heights | Shared inset, current width constraints, bounded vertical scrolling | Responsive layout keeps content and actions reachable |
| HIGH | `wire-app/src/app.rs:1296` | Native video could cover profile and chat panels | All these panels hide native video presenters while visible | Surface order preserves interaction |
| MEDIUM | `wire-app/src/app/widgets.rs:369` | Title and subtitle competed with the close control on one row | Two-line heading reserves the close control first | Optical alignment and hierarchy |
| MEDIUM | `wire-app/src/app/calls_ui.rs:1279`, `chat_ui.rs:2987` | Long names consumed space needed by row actions | Bounded, truncated names with full-name hover text | Layout preserves action hit areas |
| MEDIUM | `wire-app/src/app/profile_ui.rs:806` | Crop image could paint outside its stage; stage stayed fixed on short windows | Clipped and scaled preview with matching drag coordinates | Contained surfaces and responsive sizing |
| MEDIUM | `wire-app/src/app/calls_ui.rs:103` | Narrow capture picker stacked two scrolling lists | Screens/Windows tabs and a shorter list; changing tabs clears selection | Grouping and reachable sharing controls |
| MEDIUM | `wire-app/src/app/chat_ui.rs:2780` | Image preview opened with its top-left at the viewport center; toolbar overflowed | Centered initial geometry, constrained size, wrapping actions | Placement and responsive layout |
| LOW | All floating forms | Mixed frames and default profile buttons | Shared radius, border, shadow, spacing and action-button tones | Consistent elevation and controls |

## Automated verification

Headless egui regression tests exercise large -> minimum -> large viewport changes
with overflowing content and verify panel bounds and restored width. A separate
regression checks long header subtitles with and without a close control. Existing
capture-picker tests cover the wide and narrow layout geometry.

Checks completed:

- `cargo check -p wire-app`: passed.
- Scoped `rustfmt --check` and `git diff --check`: passed.
- Both new floating-panel regression tests: passed.
- Full library suite: 117 passed, 3 hardware tests ignored, 1 decoder timing failure.
  `decode_worker_shutdown_does_not_wait_for_another_packet` exceeded its two-second
  limit during decoder initialization. Its isolated rerun passed in 0.47 seconds.
  This was outside the changed UI paths; the full-suite failure is still recorded.

## Native checkpoint — Not verified

1. Open settings at 460 x 500, scroll through each section, and save. Try a long
   audio-device name. Resize larger, then minimize and restore with settings open.
2. Open contacts with long names and many friends. Check Call and menu hit areas,
   scroll the list, expand More options, and open Edit profile.
3. Open the crop editor from settings and from the profile editor. Drag and zoom,
   resize the app while cropping, then Save, Cancel, and close with X. The parent
   profile editor should be disabled while crop is open.
4. During a screen share, open each floating form. Native video must not cover
   them; closing them must restore the video.
5. Resize the capture picker across its one/two-column breakpoint. On narrow
   windows, switch Screens/Windows, select a target, toggle system audio, refresh,
   cancel, and start sharing. Switching tabs must clear the hidden selection.
6. Open group creation and member lists with many long names; use Add friend and
   close the child form. Open image preview and try its smallest size, Fill window,
   Fullscreen, Download, Delete (draft), and Close.
7. Inspect hover, focus, pressed, disabled, loading and empty states in all four
   themes at 100% and 150% display scaling. The UI bridge does not expose native
   Windows app surfaces, so pixel appearance and mouse interaction need this pass.

Approve the inspected source changes; native interaction and visual coverage
remain pending the checkpoint above.
