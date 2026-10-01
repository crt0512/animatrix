# Installing

Read [Setup](setup.md) first: Animatrix expects a working `asusd`.

## Build dependencies (Debian/Ubuntu)

```sh
sudo apt install build-essential pkg-config libgtk-4-dev rustup
rustup install stable
```

Debian's own `rustc` package is too old for some dependencies, so the toolchain comes from `rustup`.

## Install or update: `make upgrade`

Run as your normal user, not with `sudo`; it asks for your password when it calls `apt`:

```sh
make upgrade
```

It:

1. backs up `~/.config/animatrix/config.json` to `config.json.bak`;
2. builds the Debian package;
3. installs it with `apt install --reinstall` (so rebuilding the same version still replaces it);
4. reloads the systemd user units and stops any copy of Animatrix started by hand;
5. enables and restarts `animatrix.service`, so it also starts at every login.

`make upgrade` only works on Debian-based systems with `apt`; elsewhere it tells you to use `make install`.

## Debian package only

```sh
make deb
sudo apt install ./target/debian/animatrix_*.deb
systemctl --user enable --now animatrix.service
```

## Without a package: `make install`

Installs into `/usr/local` (change with `PREFIX=`):

```sh
sudo make install
systemctl --user enable --now animatrix.service
```

`sudo make uninstall` removes it again.

## What gets installed

| Path                                                           | Purpose                                               |
|----------------------------------------------------------------|-------------------------------------------------------|
| `/usr/bin/animatrix`                                           | The application                                       |
| `/usr/share/applications/net._512mb.Animatrix.desktop`         | App menu entry                                        |
| `/usr/share/icons/hicolor/64x64/apps/net._512mb.Animatrix.png` | App icon                                              |
| `/usr/share/animatrix/gifs/`                                   | Bundled ASUS GIFs, also the fallback for missing GIFs |
| `/usr/lib/systemd/user/animatrix.service`                      | Starts Animatrix in the graphical session             |

`make install` uses `/usr/local` instead of `/usr`.

## Starting at login

`animatrix.service` is tied to `graphical-session.target`, which only systemd-managed sessions (GNOME, Plasma) start. Cinnamon, XFCE and MATE never start it, so the enabled service alone does not run there.

So on every launch Animatrix also writes `~/.config/autostart/animatrix.desktop`, which every desktop runs at login. It starts the service, or runs `animatrix --minimized` directly when the unit is not installed. Where the service is already running, the entry does nothing.

**Settings → Start at login** shows whether the entry is installed. **Reinstall** writes it again; **Uninstall** removes it and stops later launches from putting it back. `make uninstall` does not remove this per-user file.

## Running from the source tree

```sh
make run
```

Development builds also find the app icon and the bundled GIFs in `data/`.

## Make targets

| Target                            | Purpose                                                                |
|-----------------------------------|------------------------------------------------------------------------|
| `make`                            | Build the release binary                                               |
| `make run`                        | Run from the source tree                                               |
| `make test`                       | Run the full test suite                                                |
| `make test-core`                  | Test the core without GTK (no GTK development files needed)            |
| `make install` / `make uninstall` | Install to or remove from `PREFIX` (default `/usr/local`)              |
| `make deb`                        | Build the Debian package in `target/debian/`                           |
| `make upgrade`                    | Back up the config, build and install the package, restart the service |
| `make clean`                      | Remove build output                                                    |
