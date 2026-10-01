# Animatrix

A Linux desktop and tray app for the ASUS ROG **AniMe Matrix** LED panel on the lid. Build profiles out of clocks, animated text, GIFs, a flashlight, and battery gauges, and switch them from the tray.

> [!WARNING]
> **Built for one very specific setup.** Animatrix is only tested on a ROG Zephyrus G14 2022 (GA402RK) running Debian 13 with my custom kernel (`7.2.8-crt-g14`) and my custom build of `asusd`/`asusctl`. On anything else you will most likely run into some issues **unless you read this**: [docs/setup.md](docs/setup.md) before installing.

## Features

- **Profiles** made of layered elements, or elements shown one after another.
- **Clock**, **text** (scroll, infinite scroll, bounce, blink, pulse, typewriter, wave), **GIF/Image** (GIF, PNG, or JPEG; as is, fit, stretch, scaled, or animated like text), **flashlight**, and **battery** (five styles) elements.
- **Tray icon**: left-click toggles the light show; the menu switches profiles.
- **Scripting**: `animatrix --profile NAME` or D-Bus actions switch profiles from scripts; see [docs/scripting.md](docs/scripting.md).
- **Device behaviour**: brightness cap in the header, off when unplugged/suspended/lid closed (optionally after a delay), profile switching on power and lid changes (optionally switching back after a while), built-in ASUS animations.
- Frames go straight to `asusd` over D-Bus, fast enough for ~30 FPS.

## Install (Debian)

```sh
sudo apt install build-essential pkg-config libgtk-4-dev rustup
rustup install stable
make upgrade
```

`make upgrade` builds a `.deb`, installs it, and starts Animatrix in your session; run it again after pulling changes. Other ways to build and install are in [docs/installing.md](docs/installing.md).

## Documentation

Everything else lives in [docs/](docs/README.md): setup requirements, every element and setting, device behaviour, and how Animatrix works internally.

## License

MIT
