# Setup

Animatrix is developed for, and only tested on, one machine. (I am not rich enough for more okay?!)

It was built with the intent of working on others too, but expect problems if your setup differs.

## Tested setup

|               |                                                                                                      |
|---------------|------------------------------------------------------------------------------------------------------|
| Laptop        | ASUS ROG Zephyrus G14 2022, board **GA402RK**                                                        |
| OS            | Debian 13 (trixie)                                                                                   |
| Kernel        | custom `7.2.8-crt-g14`                                                                               |
| AniMe service | `asusd`/`asusctl` **6.5.0**, custom build from [crt0512/asusctl](https://github.com/crt0512/asusctl) |
| Desktop       | Cinnamon (any desktop with StatusNotifier/AppIndicator tray support should do)                       |

## Why the custom asusd

Animatrix talks to the panel only through `asusd`. The custom build fixes AniMe bugs that stock 6.5.0 (as of 2026-10-01) has on the GA402:

- **"usb Pipe error" when turning built-in animations off.** `asusd` blanked the panel by sending the whole LED buffer as one raw USB write, which the GA402 rejects. The custom build sends it as proper packets, and blanks to dark instead of full white.
- **Built-in animation state out of sync.** `set-builtins` switched the hardware animations on without recording it, so later on/off requests misbehaved.
- **Missing default GIFs.** The Debian package built from the asusctl sources (`make deb`) left out `/usr/share/asusd/anime`, so `asusd` failed to load its own default boot animation and reset its config on every start.

With stock `asusd` Animatrix still runs, but you may see errors in `journalctl -u asusd`, a white panel after toggling built-in animations, or settings that do not stick.

## Other ASUS models

Animatrix knows the panel canvases of the GA401, GA402, GU604, and Strix (G635L/G835L) models and maps frames for each exactly like `asusctl` does, but only the GA402 has been tried on real hardware. Unknown boards are treated like a GA402.

## Requirements

- An ASUS laptop with an AniMe Matrix supported by `asusd`/`asusctl` 6.x, with `asusd` running
- GTK 4
- A desktop with StatusNotifier/AppIndicator tray support (for the tray icon)
- To build: a current Rust toolchain from `rustup` and the GTK 4 development files; see [Installing](installing.md)

Its also recommended to use a custom Kernel on Debian based distros as the latest Stable one is quiet a bit dated and doesnt include the armoury crate module, so if you use Debian on one of these laptops ... good luck to your battery life without a new Kernel.