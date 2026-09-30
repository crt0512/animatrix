# Device behaviour and tray

The **Device behavior** page controls the panel itself through `asusctl`. Press **Apply device behavior** to send the settings; they are also applied every time Animatrix starts.

## Settings

| Setting                                   | Effect                                                                                       |
|-------------------------------------------|----------------------------------------------------------------------------------------------|
| Turn off when unplugged                   | Panel off while running on battery                                                           |
| Turn off while suspended                  | Panel off during suspend                                                                     |
| Turn off when the lid is closed           | Panel off when the lid closes (see the options below)                                        |
| ↳ Unless plugged in                       | Keep running with the lid closed while on mains power; see [Lid close delay](#lid-close-delay) |
| ↳ Keep animating after lid closes         | Seconds to keep going after the lid closes (on battery with *Unless plugged in*)             |
| Enable built-in powersave animation       | ASUS's own animations when idle                                                              |
| Brightness                                | Global panel brightness: off, low, med, high; applies on top of each element's brightness    |
| Boot / awake / sleep / shutdown animation | ASUS built-in animation for each state, by name as `asusctl anime set-builtins` expects them |

Animatrix only sends built in animation and powersave commands when they differ from what `asusd` reports, which avoids needless USB traffic.

## Lid close delay

With *Turn off when the lid is closed* ticked, its two options (only usable while it is ticked) decide when:

| Unless plugged in | Delay | Lid closed on mains power | Lid closed on battery |
| --- | --- | --- | --- |
| off | 0 | off at once | off at once |
| off | N s | off after N s | off after N s |
| on | 0 | keeps running | off at once |
| on | N s | keeps running | off after N s |

Unplugging while the lid is already closed starts the delay at that moment; plugging back in or opening the lid turns the panel back on.

Only the first row is left to `asusd` (its normal behaviour). For every other combination Animatrix switches off `asusd`'s own lid handling and watches the lid (`/proc/acpi/button/lid`) and mains power (`/sys/class/power_supply`) itself.

If closing the lid suspends the laptop, the suspend setting takes over first.

## Light show switch

The **Light show** switch in the window header (and the tray) turns Animatrix's display on or off. Turning it off blanks the panel; it does not stop `asusd` or change other ASUS settings.

## Tray

- **Left-click** the tray icon to turn the light show on or off. The icon shows the current state.
- **Right-click** for the menu: light show on/off, switch profile, open the window, quit.
- The icons are black. On a dark panel tick **Invert tray icon colors** at the top of the Device behavior page to draw them white; this applies immediately.
- Closing the window keeps Animatrix running in the tray. Use *Quit* in the tray menu to exit.
