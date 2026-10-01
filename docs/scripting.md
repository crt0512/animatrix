# Scripting

A running Animatrix can be told to switch profiles or turn the light show on and off from a shell script, a keyboard shortcut, a udev rule or anything else that can run a command.

Animatrix must already be running, for example through the [systemd user service](installing.md). Nothing here starts it, and nothing polls: the running instance only wakes up when a command arrives.

## From the command line

```sh
animatrix --list-profiles             # every profile; * marks the active one
animatrix --profile "Happy Cat"       # switch to a profile
animatrix --light-show off            # on, off or toggle
```

`--list-profiles` prints one profile per line: the marker, the name, a tab, and the profile's id.

```text
  Clock	18da2e9c5d82ceec-0
* Happy Cat	18da37c57b8cc655-3
```

`--profile` takes a name or an id. A name matches exactly first, then ignoring case if only one profile fits. If two profiles share a name, use the id.

The commands exit straight away. The exit status is `0` once the command was sent, and `1` if Animatrix is not running or no such profile exists; the error goes to stderr.

Changes made this way behave like changes in the window: they are saved, show up in the window and tray, and cancel a pending [switch back](profiles.md#switching-back).

### Examples

Switch profile depending on the time of day:

```sh
#!/bin/sh
hour=$(date +%H)
if [ "$hour" -ge 22 ] || [ "$hour" -lt 7 ]; then
    animatrix --profile Night
else
    animatrix --profile Clock
fi
```

Turn the panel off for a video call, then back on:

```sh
animatrix --light-show off
some-video-call-app
animatrix --light-show on
```

Read the active profile's name:

```sh
animatrix --list-profiles | sed -n 's/^\* \(.*\)\t.*/\1/p'
```

## Over D-Bus

The command line is a thin wrapper around actions that Animatrix exports on the session bus. Anything that speaks D-Bus can call them directly.

|             |                         |
|-------------|-------------------------|
| Bus name    | `net._512mb.Animatrix`  |
| Object path | `/net/_512mb/Animatrix` |
| Interface   | `org.gtk.Actions`       |

| Action              | Parameter                  | Effect                                                 |
|---------------------|----------------------------|--------------------------------------------------------|
| `set-profile`       | string: profile name or id | Switches profile; unknown names are logged and ignored |
| `set-light-show`    | boolean                    | Turns the light show on or off                         |
| `toggle-light-show` | none                       | Flips the light show                                   |

With `gdbus`:

```sh
gdbus call --session --dest net._512mb.Animatrix --object-path /net/_512mb/Animatrix \
    --method org.gtk.Actions.Activate set-profile "[<'Happy Cat'>]" "{}"

gdbus call --session --dest net._512mb.Animatrix --object-path /net/_512mb/Animatrix \
    --method org.gtk.Actions.Activate toggle-light-show "[]" "{}"
```

Over D-Bus there is no error for an unknown profile: the call succeeds and Animatrix writes a message to its log (`journalctl --user -u animatrix`). The command line checks the name first, so prefer it in scripts.

Scripts run as another user, or from a system service, cannot reach the session bus. Run them as the logged-in user.
