# UI improvement guide

How the October 2026 UI pass was done, what was decided, and how to run the
next one. Written for agents and developers changing Wire's egui interface.

## 1. See the UI before changing it

Do not reason about alignment from code. egui layout depends on font metrics,
item spacing, and allocation order. You have to look at the result.

```sh
scripts/ui-capture.sh .amp/in/artifacts/before        # all scenes
scripts/ui-capture.sh .amp/in/artifacts/after text-   # only scenes matching "text-"
```

How it works (`wire-app/src/app/ui_capture.rs`):

- `WIRE_UI_CAPTURE=<dir>` turns on a dev-only harness in `AppState::update`.
- After the endpoint binds, it injects fixture data into UI state only: four
  contacts (one long name, mixed online status), two DMs, and two groups (one
  long title). None of this goes through the worker or the chat documents.
- It walks `SCENES`. Each scene resizes the window, applies a closure (theme,
  mode, open dialog, fake call states), waits until the viewport settles, then
  saves a PNG through `ViewportCommand::Screenshot`. When all scenes are done
  it quits.
- The harness counts as a dev fixture, so the tray, hotkeys, and first-run
  settings stay out of the way. The script uses a throwaway `WIRE_CONFIG_DIR`.
- PNGs come out at 2× on retina. View them with an image viewer or the agent's
  media tool.

Fonts: Fraktion Sans/Mono and Kh-Interference are licensed and gitignored
(`wire-app/fonts/`). Without them, captures fall back to egui's default font,
and alignment conclusions are then wrong for production. Copy the files in
first. On the maintainer's Mac they are in `~/Library/Fonts`; Kh-Interference
there is `KHInterferenceTRIAL-Regular.otf`, renamed to `Kh-Interference.otf`.

Add a scene for every state you fix. A bug without a scene will come back.
Scenes are plain data: `Scene { name, size, apply: fn(&mut AppState) }`.

`screencapture` on macOS needs Screen Recording permission, which agent shells
usually don't have. That is why the harness uses egui's own screenshot path.

## 2. Workflow used

1. Capture the baseline at four sizes: default 1100×720, medium 780×600,
   minimum 460×500, and large 1600×1000. Include themes, call states, and
   dialogs.
2. List the defects visible in the images, ordered by user impact. Clipped
   or overflowing content comes first, then misalignment, then rhythm.
3. Fix one surface at a time (chat list, timeline, composer, header, top bar,
   call dock). Re-capture only the affected scenes with the filter argument.
4. Run `cargo test -p wire-app --lib` at the end. Layout math has unit tests
   (`participant_bar_height`, bursts, floating panels).

A release build takes about 40 s incrementally, so batch related edits before
each capture.

## 3. Defects found and the rule each fix established

| Defect | Cause | Rule |
| --- | --- | --- |
| Bubbles ran off the right edge at medium widths | Bubble column width was `available_width().min(680)` in a right-to-left layout, measured before the avatar was allocated | Compute widths from the container first: `column = (avail - gutter).clamp(min, BUBBLE_MAX_WIDTH)`, then `set_max_width` on the inner frame |
| Sidebar plus chat squeezed into 460 px, both unusable | Sidebar width was a percentage clamped at 220 px | Below `NARROW_CHAT_WIDTH` (640) show one pane: the list, or the chat with a back chevron. `ChatUiState::narrow_layout` carries this to the header |
| Every bubble repeated avatar, name, and time | Bubbles ignored `starts_group` | Bubbles and compact style share `messages_share_compact_group`. Continuations get no header, keep the gutter, and show the time on hover |
| Bursts split at 13:59 → 14:00 | Grouping used minute buckets | Same author within 5 minutes (`BURST_WINDOW_MS`) |
| Two-line and one-line sidebar rows had different heights and avatars | Height and avatar size depended on whether a subtitle existed | Fixed 44 px rows and 30 px avatars. The text block is measured and centered as a unit (`truncated_galley`) |
| Composer controls floated below the text field, and the send disc was oversized | Editor and controls were in separate vertical layouts | One `left_to_right(Align::Max)` row with every control at `COMPOSER_CONTROL` (32 px). Controls stay at the bottom while the draft grows |
| Header avatar and text top-aligned in a tall card | `horizontal_top` plus measured avatar size | Header scope uses `Layout::left_to_right(Align::Center)` over the full card height |
| Self card in the call dock was clipped and overlapped the controls | Absolutely placed card in a 66 px dock with a 2 px outer margin | Removed. Identity is already in the participant strip, so the dock only holds centered controls |
| Participant strip clipped its second row | Chrome reserved height from `body.width()`, the bar laid out columns from its inner width | Both sides must call `participant_bar_columns` with the same width. The bar adds its margins back |
| Empty stage text spilled outside the stage when short | Fixed-height block centered in a smaller area | Under 200 px show one truncated line. Otherwise clip the block to the area |
| Top bar pills, status text, and icons had different heights | Mixed Phosphor and Lucide glyphs, and fixed 100 px segment widths | `CHROME_CONTROL_HEIGHT` = 32. Segments sized to content. Icons go through `chrome_icon_button` (Lucide only) |

## 4. egui techniques that worked

- **Paint text from galleys for pixel-exact rows.** `Label` inside nested
  layouts adds item spacing and baseline drift. For list rows and identity
  blocks, lay out with `truncated_galley` (single line, ellipsis at a width),
  then position with `painter().galley(...)`. Center the combined block height,
  minus about 2 px of shared leading, on the row.
- **Set `item_spacing = ZERO` in hand-built rows** and add explicit
  `add_space`. The theme's 8×6 default spacing silently adds up.
- **Use the `layout` of `scope_builder` for vertical centering:**
  `UiBuilder::new().max_rect(r).layout(Layout::left_to_right(Align::Center))`.
- **One helper per control type.** `chrome_icon_button(size, glyph)`,
  `composer_*_button`, `chat_segment_button`. Hover fill, glyph color, and
  radius then match everywhere.
- **Measured Fraktion metrics** (with `FONT_BOOST`): `sans(13)` gives a 22 px
  row with baseline 15 and cap top 4. `sans(11)` gives a 19 px row with
  baseline 13. Kh-Interference at 12 gives a 12 px row with no internal
  leading, which is why uppercase labels sit higher than sans text in the same
  row. Center them instead of top-aligning.
- **Popups instead of `menu_button`**: `egui::Popup::menu(&response)` on a
  custom-painted trigger keeps the trigger visually identical to the other
  icon buttons.

## 5. Character to preserve

Wire should not drift into a generic Discord look. Keep:

- Kh-Interference for the wordmark, section labels (`CONVERSATIONS`,
  `STAGE`), and avatar initials. Fraktion for everything else.
- Warm, low-contrast palettes per theme. Use the accent sparingly: unread
  dots, primary buttons, send, and the active GIF state.
- Rounded cards on a flat background, with hairline borders
  (`chat_hairline`) rather than shadows.
- Personal accent colors on names (title, authors, participants).
- Quiet chrome. Status is a dot plus a short word, and only errors use strong
  color.

## 6. Open items for the next pass

These are visible in the current captures (`settings-*`, `profile-editor`,
`group-editor`, `calls-idle`):

- **Profile editor** cuts off Save/Cancel at the default size. It also uses
  egui's centered native title, unlike the settings and contacts header.
  Switch it to `floating_dialog_header` with a pinned footer.
- **Group editor**: default egui checkboxes look out of place, and the dialog
  has too much empty height. Use selectable member rows with avatars. Size the
  dialog to its content.
- **Settings**: the profile block duplicates the profile editor. Combo boxes
  and checkboxes are egui defaults and don't match the custom controls.
- **Contacts dialog** (`calls-idle`): rows are taller than the content needs.
  The name sits below the avatar center because of `two_line_avatar_size`
  combined with label spacing. Apply the sidebar-row technique.
- **Call participant chips** still use `ellipsize` by character count. Use
  `truncated_galley` so the truncation point depends on pixel width.
- **Theme coverage**: only Amber has been reviewed closely. Check
  `theme-terminal`, `theme-oled`, and `theme-slate` for contrast, especially
  `chat_selected_surface` on OLED.
- **Manual-only checks**: long multi-line drafts, GIF picker placement,
  resizing across 640 px with a chat open, real avatars instead of initials,
  and live video tiles.
