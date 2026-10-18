# ig-reels-scroll

A Rust program that opens Instagram Reels in Firefox, tiles the windows, and scrolls them for you on **KDE Plasma Wayland**.

For when scrolling one window by hand has become an administrative burden.

## What it does

- Opens between 1 and 16 Firefox windows and fits them into a grid on one screen.
- Moves the pointer into each window and scrolls it in turn.
- Starts additional windows further down the feed to reduce simultaneous repeats.
- Stops when you press **X**, even with Firefox focused. **Ctrl-C** in the terminal also works.
- Reuses one input control session per run.
- Optionally unmutes reels with `--unmute`, leaving already audible reels alone.

This is desktop automation. It takes over your pointer and window focus while it runs. Instagram handles the recommendations; this program handles the clerical work.

## Requirements

- Linux with **KDE Plasma 6 running a Wayland session**.
- Firefox, available as `firefox` in your `PATH`.
- A recent Rust toolchain with Cargo and a working native build toolchain.
- `qdbus6`, available in your `PATH`.
- KDE's desktop portal backend with Remote Desktop input control support. Approve the input control request if prompted.

The program uses [`wdotool-core`](https://crates.io/crates/wdotool-core) as a library. You do **not** need the `wdotool` command installed separately.

GNOME, other Wayland compositors, X11, Windows, and macOS are not supported by this version.

## Build and run

```bash
git clone https://github.com/Papajittan/ig-reels-scroll.git
cd ig-reels-scroll
cargo build --release --locked
```

Sign into Instagram in Firefox first. Then run:

```bash
./target/release/ig-reels-scroll --windows 2 --count 20 --interval 3
```

The program registers the stop key, opens the windows, and tiles them. It then waits **8 seconds** for you to dismiss any Instagram dialogs before applying the starting offsets and beginning the regular scroll passes.

Press **X anywhere** to stop. Firefox windows remain open.

While the program is running, KDE reserves the plain X key for stopping it. Typing an x into a search box will also stop the program. That is the emergency brake doing its job, with rather literal enthusiasm.

For development, run through Cargo:

```bash
cargo run -- --windows 2 --stagger 5
```

## Options

| Option | Default | Meaning |
| --- | --- | --- |
| `--windows N` | `1` | Number of Firefox windows, from 1 to 16. |
| `--count N` | `20` | Number of regular scroll passes. Each pass scrolls every selected window once. Must be positive. |
| `--interval SECONDS` | `3` | Pause between completed passes. Accepts decimals; minimum is 0.2 seconds. |
| `--stagger N` | `5` | Extra starting scroll steps per window index, from 0 to 100. Use 0 to disable. |
| `--no-open` | Off | Use existing Firefox windows instead of opening new ones. |
| `--unmute` | Off | Automatically activate the visible unmute control in each window. |
| `--no-unmute` | On | Leave audio unchanged. This is the default; the last audio flag wins. |
| `--help` | | Show usage and exit. |

`--interval` is the pause between passes, not an exact timer for each reel. Focusing windows and sending input takes additional time. Starting offsets happen before the counted passes.

### Automatic unmuting

```bash
./target/release/ig-reels-scroll --windows 2 --unmute
```

With `--unmute`, the program checks the active Instagram window after startup and after scrolling. It reads the state of the small speaker at the bottom-right of the reel and clicks it when muted. Firefox exposes this control as an **Adjust volume** slider. The program reads its volume, finds its position relative to the actual tiled window, and sends a normal mouse click. It checks the resulting volume to confirm the click worked. An already audible reel stays audible. Multiple windows can play sound together; the program makes no attempt to turn that into a sensible listening experience.

Layouts with explicit **Unmute**, **Turn audio on**, or **Audio is muted** buttons are also supported. The program does not write Firefox's native accessibility volume properties.

Without `--unmute` (or with `--no-unmute`), the program leaves the existing audio state alone. It does not force videos to be muted.

This feature uses Firefox's AT-SPI accessibility tree and currently recognizes English audio labels. It temporarily enables desktop accessibility if needed, then restores the previous setting on normal exit, X, or Ctrl-C. An accessibility bus (`org.a11y.Bus`, typically provided by `at-spi2-core`) must be available. A forced kill may leave accessibility enabled.

If the control is missing, hidden, ambiguous, or inaccessible, it prints a warning and continues scrolling without toggling audio. Popups, Firefox accessibility restrictions, and Instagram layout changes can prevent detection. This option cannot override a muted Firefox tab, system volume, or a reel with no audio track.

### Starting offsets

With `--stagger 5`, the starting positions are:

| Window | Extra starting scroll steps |
| --- | --- |
| 1 | 0 |
| 2 | 5 |
| 3 | 10 |
| 4 | 15 |

Each starting scroll step includes a 900 ms pause for the page transition. More windows and larger offsets mean a longer startup.

The program scrolls roughly one content viewport per step. Instagram controls how that translates into reel navigation, so a step is **not guaranteed to equal exactly one reel**.

### Use existing windows

```bash
./target/release/ig-reels-scroll --windows 2 --no-open --stagger 0
```

Open the desired Reels pages beforehand. `--no-open` selects the first N Firefox windows returned by KWin. It does not filter by Instagram URL, so close unrelated Firefox windows if you want predictable selection.

## Recommendation feeds

All newly opened windows use Firefox's normal profile and Instagram login. They share the same account's recommendations.

Staggering changes the positions in those feeds. It does **not** create independent recommendation histories or guarantee different videos. Separate accounts in separate Firefox profiles can develop different recommendations, but this program does not manage those profiles or accounts.

## If something goes wrong

### Windows still overlap

The program stops rather than sending input into overlapping windows. It restores maximized windows first, waits for Firefox to acknowledge the change, then applies the grid and checks for overlap. It retries geometry changes if Firefox restores an old size late.

Try fewer windows, check for KDE window rules that force a size or position, and confirm that the terminal is running inside your KDE Wayland session. Firefox's minimum window size can prevent large grids from fitting on smaller displays.

All selected windows are placed on the screen containing the first selected window. There is no layout across multiple monitors yet.

### X cannot be registered

Check **KDE System Settings → Keyboard → Shortcuts → KWin** for an existing X assignment. The program checks that KDE assigned X to its stop action and exits if registration fails.

Normal exit and Ctrl-C remove the shortcut. A forced kill may leave its KWin script behind; the next run attempts to clean up that stale script before registering again. If X remains reserved after a crash, check the KWin shortcut settings.

### Reels are not advancing

Dismiss login prompts, cookie dialogs, and Instagram's sleep mode notice before scrolling starts. The program sends mouse input and cannot identify or dismiss web page dialogs for you.

If you need longer than the startup delay, press X, finish signing in, then restart with `--no-open`.

### KDE says a remote control session started

That is the desktop portal announcing the input permission this program uses. One run keeps one input session alive. Multiple running copies or repeated restarts can produce multiple notifications. The operating system has paperwork too.

## Development checks

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The KDE shortcut integration test is ignored by default because it needs a live desktop session. Run it separately when no other copy of the scroller is running:

```bash
cargo test global_shortcut_calls_stop_and_unloads -- --ignored
```

That test registers X, invokes its callback through KDE, verifies the stop request, then checks cleanup. It briefly reserves X but does not open Firefox or scroll any pages.

The optional audio tests cover the accessibility setting lifecycle and activation of a simulated audio control over D-Bus. The desktop setting test also requires a live accessibility bus:

```bash
cargo test desktop_accessibility_setting_is_restored -- --ignored
```

To check the real speaker, open a Reels window first and run the following separately. It focuses the last Instagram window returned by KWin and may click its speaker, so this test changes live playback state:

```bash
cargo test live_reels_speaker_unmutes_with_pointer_input -- --ignored
```

Run desktop integration tests one at a time so they do not change the shared accessibility setting during another test.

The implementation lives in [`src/main.rs`](src/main.rs); the global stop shortcut and D-Bus receiver live in [`src/stop_key.rs`](src/stop_key.rs). Optional audio detection lives in [`src/audio.rs`](src/audio.rs).
