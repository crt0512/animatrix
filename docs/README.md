# Animatrix documentation

Start with **Setup** if you have not installed Animatrix yet, then **Profiles** and **Elements** to build something on the panel.

| Page                                   | What it covers                                                                           |
|----------------------------------------|------------------------------------------------------------------------------------------|
| [Setup](setup.md)                      | The hardware and software Animatrix is tested with, and why the custom `asusd` matters   |
| [Installing](installing.md)            | Build dependencies, `make upgrade`, the Debian package, `make install`, all Make targets |
| [Profiles](profiles.md)                | Profiles, layering, element cycling, play counts, reordering, the tray                   |
| [Elements](elements.md)                | Clock, text, GIF, flashlight, and battery, with every option                             |
| [Device behaviour and tray](device.md) | Brightness, power and lid settings, lid close delay, Settings tab, tray icon            |
| [Scripting](scripting.md)              | Switching profiles and the light show from scripts, the CLI flags and D-Bus actions      |
| [How it works](architecture.md)        | Engine, D-Bus frames, panel canvas and safe area, configuration file, safety             |

## Quick answers

- **Where is my configuration?** `~/.config/animatrix/config.json`. `make upgrade` keeps a copy in `config.json.bak`.
- **My GIF says "not found".** Missing GIFs fall back to the bundled ones; see [GIF files and fallback](elements.md#gif-files-and-fallback).
- **Text is cut off at the edges.** Add Padding using Spaces, If that doesnt help the panel's corners are not visible; see [Safe area](architecture.md#canvas-and-safe-area).
- **Can a script switch profiles?** Yes: `animatrix --profile NAME`; see [Scripting](scripting.md).
- **The tray icon is invisible on my dark panel.** Tick *Invert tray icon colors*; see [Tray](device.md#tray).
- **Why is the `src` folder organized so poorly?!** This was quickly coubled together as a working demo by myself in one evening... The rest of the features, most of the comments and the docs I quiet honestly just let an LLM help me with.