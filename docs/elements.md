# Elements

Elements are the building blocks of a [profile](profiles.md). Every file setting (fonts and GIFs) has a **Browse…** button next to its path.

Settings shared by several elements:

- **Vertical offset**: moves the element down (positive) or up (negative) in pixels. Clock, text, battery, and animated GIFs.
- **Ignore safe area**: lays the element out on the whole panel canvas instead of the guaranteed-visible part. It can be larger, but may be cut off where the panel tapers; see [Canvas and safe area](architecture.md#canvas-and-safe-area). Clock, text, battery, and animated GIFs.
- **FPS** settings go up to 30; one frame to the panel takes about 30 ms.

## Clock

- TrueType/OpenType font and size
- 12- or 24-hour time, optional seconds
- optional date with a configurable format (`strftime` style, default `%Y-%m-%d`)
- optional **milliseconds**, redrawn at their own FPS (1–30, default 10), they imply seconds

The clock redraws when the displayed second or minute changes, or at the millisecond FPS. Long strings are shrunk to fit the width.

## Text

- text, font and size
- **Animation**:
  - **Static**: centred, no movement
  - **Scroll**: enters at one edge of the panel and leaves past the other, in the chosen **direction** (left, right, up, down)
  - **Bounce**: back and forth between the edges of the area; horizontally for left/right, vertically for up/down
  - **Blink**: visible for the first half of each period
  - **Pulse**: fades in and out once per period
  - **Typewriter**: types one character per period, then holds the full text for the pause
  - **Wave**: the characters ride a sine wave, one wave per period
- **Pixels per second** for scroll and bounce
- **Frames per second** for animations (1–30, default 5)
- **Period** in seconds for blink, pulse, and wave, or per typed character
- **Pause between passes**: a blank gap after each scroll pass, or the hold after typing
- **Cycles**: play count used by [element cycling](profiles.md#waiting-for-animations-or-after-animation-cycles-finish)

Static and non-horizontal animations shrink the text to fit the width; horizontal scroll and bounce keep the chosen size.

Note : I am too lazy to add proper padding to text, just use spaces to add more padding at each end.

## GIF

- **GIF file**
- **Brightness** (0–2): below 1 dims, above 1 boosts dim GIFs (bright pixels clip at full); also applies to the overlay text
- **Black level** (0–90 %): pixels at or below this brightness turn off and the rest is stretched back to the full range, which removes dim haze or noise around shapes. Applied before contrast
- **Contrast** (0.2–4, 1 = unchanged): above 1 pushes dim pixels darker and bright ones brighter, below 1 flattens them towards mid-grey. Unlit pixels stay unlit and full ones stay full, so the dark area around a GIF never lights up
- **FPS**: 0 uses the GIF's own frame delays; 1–30 plays at a fixed rate
- **Loop**: *Restart from the beginning*, or *Bounce back and forth* (plays backwards to the start and forwards again, without repeating the end frames)
- **Cycles**: play count used by [element cycling](profiles.md#waiting-for-animations-or-after-animation-cycles-finish)
- **Layout**:
  - *Leave as is*: pixels map 1:1, centred on the canvas; larger GIFs are cropped, smaller ones dont cover entire area
  - *Fit*: scaled up or down to fit the canvas, keeping the aspect ratio
  - *Stretch*: scaled to fill the canvas exactly
  - *Animate*: see below
- **Overlay text** (optional), drawn centred over the GIF in **white** (lit LEDs) or **black** (cut out of the GIF), with its own font and size:
  - **Bold** / **Italic**: uses the font's own bold, italic/oblique, or bold-italic file when one sits next to it (`DejaVuSans-Bold.ttf`, `DejaVuSerif-Italic.ttf`, … in any capitalization); otherwise the style is faked by thickening or slanting the text
  - **Outline** (0–4 px): a solid border in the opposite color (dark around white text, lit around black text) that keeps the text readable over busy GIFs

GIFs are decoded like `asusctl anime pixel-gif`: each frame's opaque pixels are painted over the previous frame and the red channel is the LED brightness. Unlike `asusctl`, each frame's disposal method is honoured, so transparent GIFs that clear areas between frames ("background" or "previous" disposal) turn those LEDs off again instead of leaving stale pixels lit. The bundled ASUS GIFs keep every frame, so they look the same either way. Shrinking averages the covered pixels so detail survives; enlarging keeps pixel art crisp.

ASUS's own 74×36 GIFs are one row shorter than the GA402's 74×39 canvas, so *Leave as is* centres them one row lower than `asusctl` places them.

### Animate layout

The GIF becomes a sprite of a chosen **size** (height in pixels, width keeps the aspect ratio). It keeps playing its own frames while moving with any [text animation](#text), with its own direction, speed, movement FPS, period, pause, vertical offset, and safe-area setting. *Typewriter* wipes the GIF in column by column and *Wave* ripples its columns.

### GIF files and fallback

The ASUS GIFs from `data/gifs` are installed to `/usr/share/animatrix/gifs` (`/usr/local/share/animatrix/gifs` with `make install`). When a GIF element's file is empty or missing, Animatrix tries in order:

1. the path as given;
2. the path relative to the bundled folder, so `music/DJ.gif` works;
3. a bundled GIF with the same file name;
4. `gaming/UFO.gif`, which is also what new GIF elements start with.

A substitution is logged once instead of failing on every frame.

## Flashlight

Lights every LED at the element's **brightness** (0–1). Layered under other elements it acts as a dim background. The device brightness from [Device behaviour](device.md) applies on top.

## Battery

Shows the charge of the first battery in `/sys/class/power_supply`, with the percentage in a configurable font and size.

- **Style**:
  - *Classic*: battery icon with fill, a bolt while charging
  - *Liquid*: upright battery whose liquid surface sloshes, with bubbles rising while charging
  - *Segments*: five cells; while charging the empty cells light one after another, and the last cell blinks when low
  - *Ring*: circular gauge with a dim track and a comet orbiting while charging
  - *Big digits*: the percentage in large digits, lit up to the charge level with a wavy tide line
- **Label** (optional): text shown under the gauge

Animated styles redraw at 10 FPS; *Classic* only when the charge or charging state changes.
