# How it works

Animatrix is one Rust program with one owner for the matrix display.

## Parts

- **GTK 4 window** (`src/ui.rs`) edits profiles and device behaviour.
- **Tray item** (`src/tray.rs`) is a StatusNotifier item: toggles the light show, switches profiles, opens the window.
- **Engine** (`src/engine.rs`) runs on its own thread. It works out which elements are visible, redraws only when something changed, and sends frames to the panel.
- **Renderer** (`src/render.rs`) draws clocks, text, and battery gauges, and moves text and animated GIFs.
- **GIF decoding** (`src/animation.rs`) and **bundled GIF lookup** (`src/assets.rs`).
- **LED mapping** (`src/matrix.rs`) turns a frame into the panel's LED order.
- **Configuration store** (`src/config.rs`) saves `~/.config/animatrix/config.json`.

The GTK application ID is `net._512mb.Animatrix`. Starting Animatrix a second time brings up the running instance instead of starting a second display controller.

## Talking to the panel

Animatrix uses the existing `asusd` system service for hardware access; it needs no privileges of its own.

- **Frames** go straight to `asusd` over the system D-Bus (`xyz.ljones.Anime.Write` on `/xyz/ljones/aura/anime`). Skipping a per-frame `asusctl` process makes a frame take about 30 ms instead of about 90 ms on a GA402, enough for roughly 30 FPS.
- **Device settings** (brightness, power and lid behaviour, built-in animations, display on/off) go through the `asusctl` command line, called with argument lists and never through a shell.

`src/matrix.rs` is a port of `rog_anime::AnimeDiagonal`, and the GIF decoding follows `rog_anime::AnimeGif`. Both were checked against `rog_anime` and produce byte-identical LED buffers for every supported model.

## Canvas and safe area

At startup Animatrix reads the DMI board name and picks the diagonal canvas that `asusctl anime pixel-image` expects:

| Model                      | Canvas |
|----------------------------|--------|
| GA401                      | 74×36  |
| GA402 (and unknown boards) | 74×39  |
| GU604                      | 70×43  |
| Strix G635L / G835L        | 68×34  |

These are transport canvases, not the physical LED layout or Armoury Crate artwork sizes quoted online.

Not all of the canvas is visible. On the GA402 the upper rows taper to a two-pixel tip; rows 19–38 share a guaranteed-visible **40×20 safe area**. Clock, text, and battery elements lay out inside it unless *Ignore safe area* is ticked. Scrolling text still travels across the whole canvas so it enters and leaves at the real panel edges.

## Redrawing

Animatrix is event-driven and does not poll when there is nothing to do.

Each visible element reports when it can next look different: a clock at the next second or minute boundary, animations at their FPS, a GIF at its next frame, a still battery gauge every 5 seconds, and static text or a flashlight never. The engine sleeps until the soonest of those and only sends a frame when something actually changed. A clock without seconds therefore costs one wake-up per minute.

Other reasons to wake up:

- **Settings changes** from the window or tray wake the engine immediately.
- **Sensors** are read only if a setting needs them: the lid for the [lid close delay](device.md#lid-close-delay) or lid triggers, mains power for *Unless plugged in* or power triggers; both for the *lid and plugged in* triggers. Then they are checked once a second, which also runs pending [switch-backs](profiles.md#switching-back).
- **Element cycling** checks for the next turn every 200 ms.
- A frame that could not be sent (for example while `asusd` restarts) is retried after a second.

With the light show off and no [profile triggers](profiles.md#switching-automatically), the engine sleeps until you change something. The tray only updates when the engine reports a change, and the window's status line only refreshes while the window is open. Fonts are loaded once and reused.

## Configuration file

`~/.config/animatrix/config.json` holds the light show switch, the active profile, all profiles and their elements, device behaviour, and the tray icon setting. Settings added in later versions get defaults when missing, and older layouts are converted on load (see [Older configurations](profiles.md#older-configurations)).

## Safety

- Disabling or quitting Animatrix never stops `asusd` or changes unrelated ASUS settings.
- Invalid elements are reported in the window's status line instead of being drawn.
- Hardware errors are shown in the window instead of being ignored.

## Status and plans

Done: profile and element model, rendering, D-Bus display engine, GTK editor, tray, session autostart, Debian packaging.

Ideas for later: more element types (music visualizer for example), a live preview in the editor, profile import/export, richer animation timelines.
