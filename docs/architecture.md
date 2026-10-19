# Architecture and KDE regression checklist

Milestone 1 keeps KDE Plasma 6 on Wayland and Firefox as the only supported
runtime combination. The interfaces are internal seams for that behavior, not
promises that other platforms or browsers already work.

## Module boundaries

- `app.rs` parses process arguments, checks the supported environment, builds
  runtime dependencies, races the engine with X and Ctrl-C, then restores audio
  accessibility and stop-key resources. Accessibility restoration is bounded
  to five seconds; cleanup errors are reported with uncertain remote state
  called out explicitly.
- `cli.rs` parses an argument iterator into validated configuration. It has no
  process or desktop access.
- `engine.rs` runs browser acquisition, layout verification, startup delay,
  starting offsets, scroll passes, and optional audio hooks. It uses only the
  browser, platform, layout, configuration, and audio interfaces. It checks
  that adapters return exactly the requested number of unique, non-empty
  handles before any window manipulation.
- `browser/` owns browser identification, launch behavior, window readiness,
  and browser-specific metadata. Firefox is the only adapter.
- `platform/` exposes opaque window handles, logical-pixel geometry, activation,
  layout, pointer movement, scrolling, and button input. `kde_wayland.rs` owns
  environment detection, one `wdotool-core` 0.5.3 backend session, the
  application's KWin JavaScript, and its direct `qdbus6` script operations.
  Those owned qdbus children have a five-second operation limit, bounded output
  readers, and explicit termination and reaping on timeout. The tracked layout
  cleanup queue has a twenty-second total limit, and desktop accessibility
  restoration has a five-second limit. Failures report that remote KWin state
  is unconfirmed rather than claiming cleanup succeeded.
- `wdotool-core` also performs its own KWin window operations. In version
  0.5.3, it creates private transient scripts, calls KWin through its own D-Bus
  connection, and performs private best-effort unload cleanup whose errors are
  logged and swallowed rather than returned to this application. The owned
  qdbus timeout and cleanup queue do not bound or confirm that dependency's
  internal cleanup. If an operation is canceled after its D-Bus request was
  sent, remote script state can remain unconfirmed even when all application
  cleanup stages complete. This boundary is documented here; vendor code is
  unchanged.
- `layout.rs` calculates the current square-root grid, divides integer display
  dimensions, and detects invalid or intersecting rectangles. The KDE backend
  calculates equivalent JavaScript partition expressions when it applies the
  grid to KWin's active work area.
- `audio.rs` owns Firefox AT-SPI lookup, state-aware activation, and restoration
  of the previous desktop accessibility setting. `NoAudio` keeps the engine
  usable without accessibility setup.
- `stop.rs` carries the platform-neutral emergency stop signal. `stop_key.rs`
  registers the KDE global X shortcut and its D-Bus callback.

To add a browser in a later milestone, implement `BrowserAdapter` with that
browser's launch arguments, matching rules, readiness detection, and metadata.
The engine already receives browser selection through that boundary and does
not need to know which executable or accessibility tree the adapter uses.

## Manual KDE Wayland regression checklist

Run these checks only in a KDE Plasma 6 Wayland session. They move the pointer,
change window geometry, and may open Firefox windows. Sign into Instagram before
checking feed movement.

1. Close all Firefox windows, then run
   `cargo run -- --windows 2 --count 1 --stagger 0 --no-unmute`. Confirm that
   two Firefox windows open, tile without overlap, wait eight seconds, and
   scroll once. Close those windows, open two Instagram Reels windows manually,
   then run
   `cargo run -- --windows 2 --count 2 --interval 0.2 --stagger 1 --no-open --no-unmute`.
   `--no-open` uses the first N Firefox windows in KWin enumeration order, so
   other open Firefox windows can change which pages it selects; verify the
   actual selected windows and close other Firefox windows for this check.
   Confirm the selected windows are tiled with non-overlapping rectangles,
   scrolling waits eight seconds, and window two receives one 900 ms starting
   offset before the two counted passes.
2. Open four Firefox Reels windows, maximize one and make another fullscreen.
   Leave a separate non-Firefox window visible. Run
   `cargo run -- --windows 4 --count 1 --stagger 0 --no-open --no-unmute`.
   Confirm the four selected Firefox windows are restored and tiled, the other
   application window is untouched, and every reported rectangle is valid.
3. Check the insufficient-window path by closing Firefox windows until fewer
   remain than requested, then run
   `cargo run -- --windows 2 --no-open`. Confirm the program reports the number
   requested and found, performs no pointer input, and cleans up the X shortcut.
4. Check overlap refusal on a small display or with a KDE window rule that
   forces a selected Firefox window to an overlapping size. Confirm geometry is
   retried and the program stops before scrolling if overlap remains. Firefox's
   minimum window size can make a dense grid impossible on small displays.
5. For X cancellation, start a run with a long pass count and press plain `X`
   while Firefox has focus. Repeat by pressing X during the eight-second startup
   delay, during a stagger transition, and during repeated passes. Confirm each
   run stops and leaves Firefox open. Start another run to confirm the shortcut
   was released.
6. Repeat the same startup, stagger, and pass cancellation points with Ctrl-C in
   the launching terminal. Confirm the shortcut and input resources are cleaned
   up on each exit.
7. With two loaded Reels pages, run `cargo run -- --windows 2 --count 2
   --stagger 0 --unmute`. Confirm an already audible reel remains audible. To
   check warning-and-continue behavior, use a selected Instagram page whose
   audio control is hidden or inaccessible and confirm the warning appears while
   later scrolling continues. Run separately with `--no-unmute` and confirm no
   audio state changes.
8. Record the desktop accessibility setting before `--unmute`. Check that the
   prior value is restored after normal completion, X, Ctrl-C, and an engine
   error such as
   `cargo run -- --windows 2 --no-open --unmute` with fewer than two Firefox
   windows available. Repeat once with the setting initially enabled and once
   initially disabled.
9. Approve the KDE Remote Desktop prompt once. Confirm a run keeps one input
   session through all scrolling and does not ask for a fresh portal session per
   action.
10. In a disposable non-KDE session, run the program and confirm it reports
    `This version is for KDE Plasma on Wayland.` before registering X. On KDE,
    test missing-command errors only with a disposable `PATH`: omitting `qdbus6`
    should fail during stop-key setup; omitting `firefox` should fail when a new
    window is requested. Neither case should send pointer input.

Run each ignored desktop integration test separately. The shortcut test
temporarily reserves X; the accessibility test reads and changes the desktop
accessibility setting. The live speaker test can click a real control, so run it
only with a loaded Reels page and a deliberate check:

```bash
cargo test global_shortcut_calls_stop_and_unloads -- --ignored
cargo test desktop_accessibility_setting_is_restored -- --ignored
cargo test live_reels_speaker_unmutes_with_pointer_input -- --ignored
```

The offline test suite does not run these desktop checks. Layout behavior can
still vary with display size, Firefox minimum window sizes, KDE window rules,
and the selected window's monitor.
