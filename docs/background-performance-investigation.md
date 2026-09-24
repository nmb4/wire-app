# Background Performance Investigation Plan

## Objective

Determine why a hidden `wire-app` process consumes roughly 300 MB RAM and about 10% CPU while idle, without changing refactor direction prematurely. Establish a reproducible baseline, identify the dominant contributors, fix only confirmed causes, and verify the result under both visible and tray-hidden operation.

## Constraints

- Keep calls, audio, chat delivery, presence, and outgoing screen sharing alive while the window is hidden.
- Do not treat Task Manager's single instantaneous sample as sufficient; use time-series CPU and memory measurements.
- Separate debug-build overhead from release-build behavior.
- Preserve the current host/tray lifecycle and avoid broad speculative refactors.
- Do not continue the presenter/snapshot refactor until this performance checkpoint is understood.

## Investigation phases

### 1. Establish reproducible baselines

1. Build both debug and release binaries.
2. Run a normal visible launch and a `--background` launch in separate clean sessions.
3. Record at least 60 seconds of:
   - working set and private bytes;
   - CPU time deltas and normalized CPU percentage;
   - thread count and thread CPU time;
   - handle/GPU/resource counts where available.
4. Repeat after the process has been idle for 5–10 minutes to distinguish startup/transient cost from steady-state cost.
5. Confirm the baseline against the user's reported values and note whether CPU is bursty or sustained.

### 2. Attribute the cost

Inspect and measure, in this order:

1. egui/eframe hidden-frame scheduling and the 250 ms fallback repaint path.
2. `ActivationWatcher` polling and tray callback wakeups.
3. service worker timers, presence refresh, chat sync/keepalive, and network discovery.
4. audio context initialization, audio callbacks, and device polling.
5. screen-capture/video decoder tasks and preview/media event delivery.
6. logging, resource monitoring, persistence, and notification work.
7. GPU/renderer allocations retained by the hidden viewport.

Use logs, thread CPU samples, ETW/WPR or an available Windows profiler, and temporary counters only where needed. Compare visible versus hidden runs and call-free versus call-active runs.

### 3. Form hypotheses and test them independently

Likely hypotheses to verify, not assume:

- hidden egui still repainting because of `request_repaint_after` or service event callbacks;
- `ActivationWatcher` or tray integration wakes too often;
- a service timer or network retry loop has an unexpectedly short interval;
- audio/D3D initialization retains large buffers even with no call;
- a media/capture task continues work when sharing is inactive;
- release/debug or GPU driver allocation behavior explains the memory baseline.

For each confirmed cause, add the smallest targeted fix. Avoid masking symptoms by merely lowering logging or hiding the process.

### 4. Verify and checkpoint

After each fix:

1. Re-run the same visible/background measurement protocol.
2. Confirm no call/audio/chat/presence regression.
3. Run `cargo check -p wire-app`, targeted tests, and the full relevant test suite.
4. Perform a manual tray hide/show and second-launch activation check.
5. Record before/after numbers in this document or a follow-up results section.
6. Create a local GitButler checkpoint only after the cause and fix are supported by evidence.

## Acceptance criteria

- Steady-state hidden CPU is materially lower and clearly explained.
- Idle memory is either reduced or documented as expected renderer/audio/runtime residency.
- No background work is performed at a high frequency without a user-visible reason.
- Hidden mode does not stop calls, audio, chat, presence, or outgoing sharing.
- Visible mode and tray lifecycle remain usable.

## Findings so far

### Confirmed CPU cause

The high CPU is in the native presentation thread, not the service, audio, activation watcher, or tray code. In an unmodified hidden debug run, the main OS thread accumulated approximately one full core while `App::update` ran only once; the process then remained in the wgpu/eframe redraw path. Removing the 250 ms fallback repaint, disabling the tray, changing present mode, and minimizing the window did not materially reduce it.

Using eframe's external event-loop API and dropping `RedrawRequested` while the presenter is hidden reduced the same debug measurement to approximately **0.22% average / 0.90% maximum** over 45 seconds. This is the targeted fix now being integrated: the service and tray remain alive, but the hidden native surface is not continuously painted.

### Release/debug comparison

Using an isolated `WIRE_CONFIG_DIR` and no call:

| Build | Average normalized CPU | Maximum | Working set | Private bytes |
|---|---:|---:|---:|---:|
| Release, 60 s | 0.15% | 0.90% | 356 MB | 296 MB |
| Debug, 60 s | 8.48% | 9.49% | 319 MB | 297 MB |
| Debug + hidden-redraw suppression, 45 s | 0.22% | 0.90% | 320 MB | 298 MB |

The user's approximately 10% CPU matches the debug build's sustained hidden redraw behavior. The release build's steady-state CPU is already low, but the hidden-redraw fix removes the debug-specific waste as well.

### Memory attribution

A 180-second release run held private bytes steady around 294–295 MB, so this is residency rather than an obvious short-term leak. The working set later fell to about 181 MB as pages were reclaimed.

An experimental Glow renderer reduced private memory to approximately 135 MB (versus 298 MB with the current Vulkan backend), while leaving the hidden CPU issue unchanged. Audio initialization was not the memory cause: forcing audio-device initialization to fail still measured about 295 MB private. The current memory baseline is therefore dominated by the Vulkan/wgpu renderer, not calls or audio capture.

Changing the renderer is a product/compatibility decision because the project intentionally selects Vulkan and uses platform-specific video presentation. It is being left unchanged for now. If lower memory is required, the next options are a renderer choice with visual/platform validation or a genuinely headless service/UI process split.

### Next verification

- Run the full test suite and clippy after the event-loop change.
- Rebuild release and repeat the same 60-second background measurement.
- Recheck visible mode, close-to-tray, tray activation, second-launch activation, and updater exit.
- Keep the service event/state path unchanged until presenter snapshots are introduced; hidden rendering is now suppressed at the native event boundary rather than by stopping the service.
