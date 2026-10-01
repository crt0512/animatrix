# Elements

Elements are the building blocks of a [profile](profiles.md). Every file setting (fonts and GIFs) has a **Browse…** button next to its path.

Settings shared by several elements:

- **Offset**: moves the element right (→, negative moves left) and down (↓, negative moves up) in pixels. Elements are drawn with a margin around the panel, so moving one brings in what lay past the panel's edge instead of a blank strip. Clock, text, battery, and GIFs in every layout.
- **Ignore safe area**: lays the element out on the whole panel canvas instead of the guaranteed-visible part. It can be larger, but may be cut off where the panel tapers; see [Canvas and safe area](architecture.md#canvas-and-safe-area). Clock, text, battery, and animated GIFs.
- **Shown for** (seconds, or cycles for text and GIFs) appears at the top of each card while its profile shows [one element at a time](profiles.md#element-cycling).
- **FPS** settings go up to 30; one frame to the panel takes about 30 ms.
- **Font sizes** go down to 1 for small pixel fonts, and are used as set for clocks, text, and GIF overlays laid out on the safe area: centred on it, even where they run past it. With *Ignore safe area* ticked, clocks and text ignore the font size and scale to fill the canvas instead. Battery text always fits its gauge.
- **Rotate °**, in each element card's heading, turns the element clockwise by any number of degrees (negative turns the other way). It turns about the middle of the area it is laid out in, so a clock in the safe area stays there; scrolling text turns about the middle of the whole canvas along the way it scrolls, and placed GIFs (*As is*, *Fit*, *Stretch*) about the middle of the canvas. Quarter turns (90, 180, 270) move whole pixels and stay sharp; other angles blend neighbouring pixels. A turned element keeps its size, so 90° text 40 pixels wide becomes 40 pixels tall and can reach past the safe area; lower the font size or tick *Ignore safe area*. The offset still moves the element the same way on the panel. Clock, text, GIF, and battery.
- **Tilt compensation**, also in the heading, leans the element against the panel's row shift that makes upright strokes look italic: rows above the element's middle move one way, rows below the other. How far is set once for all elements on the Settings tab (**Tilt compensation (pixels per row)**, 0.5 by default). If ticked elements look more slanted, not less, use the opposite sign. Clock, text, GIF, and battery.
- **Smooth font edges**, also in the heading, anti-aliases text. Untick it to light each LED fully or not at all (a pixel counts as lit when the glyph covers at least half of it), which often reads crisper on the panel's coarse LEDs. On by default. Clock, text, battery, and GIF overlay text. Unticked, a turned element also picks the nearest pixel instead of blending, so crisp text stays crisp at any angle.

## Clock

- TrueType/OpenType font and size
- 12- or 24-hour time, optional seconds
- optional date with a configurable format (`strftime` style, default `%Y-%m-%d`), below the time or, with **Date above the time**, above it
- optional **milliseconds**, 1–3 digits (**Millisecond digits**, default 3, cut off rather than rounded), redrawn at their own FPS (1–30, default 10); they imply seconds

The clock redraws when the displayed second or minute changes, or at the millisecond FPS. Long strings are shrunk to fit the width.

## Text

- text, font and size. The text box takes several lines (Enter starts a new one); lines are centred under each other and the font shrinks until all of them fit the area. Scrolling sideways keeps the lines together as one block. Typewriter and wave count characters across all lines.
- **Animation**:
  - **Static**: centred, no movement
  - **Scroll**: enters at one edge of the panel and leaves past the other, in the chosen **direction** (left, right, up, down)
  - **Infinite scroll**: like scroll, but the next copy follows straight behind instead of waiting until the panel is empty, so after the first copy has come in the text never runs out. Copies follow right behind each other; **Pause between passes** puts its worth of travel between them (add spaces to the text for a gap in characters). One *cycle* is one copy passing
  - **Bounce**: back and forth between the edges of the area; horizontally for left/right, vertically for up/down
  - **Blink**: visible for the first half of each period
  - **Pulse**: fades in and out once per period
  - **Typewriter**: types one character per period, then holds the full text for the pause
  - **Wave**: the characters ride a sine wave, one wave per period
- **Pixels per second** for scroll and bounce
- **Frames per second** for animations (1–30, default 5)
- **Period** in seconds for blink, pulse, and wave, or per typed character
- **Pause between passes**: a blank gap after each scroll pass, or the hold after typing
- **Text color**: *White* lights LEDs. *Black* cuts the text out of the layers beneath it (elements earlier in the profile), so it shows as dark letters over a flashlight or GIF; on its own it shows nothing.
- **Outline** (0–4 px): a border in the opposite color, so the text stays readable over a flashlight or GIF. Around white text it is dark and cuts out the layers beneath; around black text it is lit, and never covers the letters. It follows the text through rotation, offset, and animation.
- **Invert**: flips the text before its color applies. White lights the whole panel and leaves the text dark; black cuts out everything beneath except the text, so the layers beneath only show through the letters.

On the safe area the text keeps the chosen size and is centred on it, even where it runs past it. With *Ignore safe area* the font size is ignored and the text is scaled to fill the canvas: as large as fits its width and height, or for scroll and bounce as large as fits across the way it moves (the height when moving sideways, the width when moving up or down).

Note : I am too lazy to add proper padding to text, just use spaces to add more padding at each end.

## GIF/Image

- **GIF, PNG, or JPEG file**. A PNG or JPEG is shown as a still picture: its lightness is the LED brightness and transparent areas stay dark. Files are recognised by their content, not their name. Everything below applies to pictures too; FPS, loop, and cycles have nothing to play
- **Brightness** (0–2): below 1 dims, above 1 boosts dim GIFs (bright pixels clip at full)
- **Black level** (0–90 %): pixels at or below this brightness turn off and the rest is stretched back to the full range, which removes dim haze or noise around shapes. Applied before contrast
- **Contrast** (0.2–4, 1 = unchanged): above 1 pushes dim pixels darker and bright ones brighter, below 1 flattens them towards mid-grey. Unlit pixels stay unlit and full ones stay full, so the dark area around a GIF never lights up
- **FPS**: 0 uses the GIF's own frame delays; 1–30 plays at a fixed rate
- **Loop**: *Restart from the beginning*, or *Bounce back and forth* (plays backwards to the start and forwards again, without repeating the end frames)
- **Layout**:
  - *Leave as is*: pixels map 1:1, centred on the canvas; larger GIFs are cropped, smaller ones dont cover entire area
  - *Fit*: scaled up or down to fit the canvas, keeping the aspect ratio
  - *Stretch*: scaled to fill the canvas exactly
  - *Animate*: see below
- **Scale** (×0.1–10): multiplies the size the layout picks, still centred: ×2 doubles a GIF shown *as is*, ×0.5 halves a fitted one. Use the offset to move an enlarged GIF around. Not for *Animate*, which has its own size
- **Invert**: lights the dark pixels and darkens the lit ones, after black level and contrast. The overlay is not inverted. By default the whole panel around the GIF lights up too; tick **Only the GIF's own area** to invert just its rectangle (with *Animate*, just where the sprite is drawn), leaving the rest dark.
- **Overlay text** (optional, several lines allowed), drawn like a text element on top of the finished panel frame, after every element of the profile has been layered, in **white** (lights LEDs over whatever is underneath) or **black** (cuts them out of everything), with its own font and size. The GIF's brightness, offset, rotation, and tilt do not touch it; it is centred in the safe area and only its own settings below apply:
  - **Bold** / **Italic**: uses the font's own bold, italic/oblique, or bold-italic file when one sits next to it (`DejaVuSans-Bold.ttf`, `DejaVuSerif-Italic.ttf`, … in any capitalization); otherwise the style is faked by thickening or slanting the text
  - **Outline** (0–4 px): a solid border in the opposite color (dark around white text, lit around black text) that keeps the text readable over busy GIFs
- **Overlay position**: pixels right (→) and down (↓) from the middle of the safe area; negative moves left or up.
- **Overlay rotation °**: turns the overlay text clockwise about its own middle by any number of degrees (negative turns the other way), before the overlay position moves it. Quarter turns stay sharp; other angles blend unless *Smooth font edges* is unticked.
- **Overlay animation**, **direction**, **pixels per second**, **period**, and **pause**: the [text animations](#text), applied to the overlay. There is no separate FPS: the overlay steps at the GIF's **FPS** when set, otherwise as fast as the GIF's quickest frame (at most 30 FPS).

Older configurations that drew the overlay into the GIF itself now draw it over the panel: it is no longer dimmed by the GIF's brightness or moved with the GIF.
- **Smooth GIF scaling**: blends pixels when the GIF is scaled (*Fit*, *Stretch*, *Animate* size) or turned. Untick it to pick the nearest pixel instead, so pixel art stays crisp; quarter turns are crisp either way. The overlay text follows *Smooth font edges* in the card heading.

GIFs are decoded like `asusctl anime pixel-gif`: each frame's opaque pixels are painted over the previous frame and the red channel is the LED brightness. Unlike `asusctl`, each frame's disposal method is honoured, so transparent GIFs that clear areas between frames ("background" or "previous" disposal) turn those LEDs off again instead of leaving stale pixels lit. The bundled ASUS GIFs keep every frame, so they look the same either way. Shrinking averages the covered pixels so detail survives; enlarging keeps pixel art crisp.

ASUS's own 74×36 GIFs are one row shorter than the GA402's 74×39 canvas, so *Leave as is* centres them one row lower than `asusctl` places them.

### Animate layout

The GIF becomes a sprite of a chosen **size** (height in pixels, width keeps the aspect ratio). It keeps playing its own frames while moving with any [text animation](#text), with its own direction, speed, movement FPS, period, pause, offset, and safe-area setting. *Typewriter* wipes the GIF in column by column and *Wave* ripples its columns.

### GIF files and fallback

The ASUS GIFs from `data/gifs` are installed to `/usr/share/animatrix/gifs` (`/usr/local/share/animatrix/gifs` with `make install`). When a GIF/Image element's file is empty or missing, Animatrix tries in order:

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
- **Scale** (×0.25–4): grows or shrinks the whole drawing (gauge, percentage, and label) about the middle of its area, like a zoom; the pixels blend unless *Smooth font edges* is unticked. Move it with the **offset** (→ / ↓) and turn it with *Rotate °*; an enlarged battery can be moved to bring any part into view

Animated styles redraw at 10 FPS; *Classic* only when the charge or charging state changes.
