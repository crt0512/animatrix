# Device behaviour and tray

Animatrix controls the panel itself through `asusctl`. Device settings are applied every time Animatrix starts.

## Brightness

**Brightness** at the left of the window header is the global cap on the panel: off, low, med or high. It applies on top of each element's own brightness, to every profile, as soon as you change it.

## Device behavior tab

The **Device behavior** tab holds [Switch profile automatically](profiles.md#switching-automatically) and the **Turn off** options. Turn off options apply as soon as they change.

| Setting                                   | Effect                                                                                       |
|-------------------------------------------|----------------------------------------------------------------------------------------------|
| When unplugged                            | Panel off while running on battery                                                           |
| While suspended                           | Panel off during suspend                                                                     |
| When the lid is closed                    | Panel off when the lid closes (see the options below)                                        |
| ↳ Unless plugged in                       | Keep running with the lid closed while on mains power; see [Lid close delay](#lid-close-delay) |
| ↳ Keep animating after the lid closes     | Seconds to keep going after the lid closes (on battery with *Unless plugged in*)             |

## Settings tab

| Setting                                   | Effect                                                                                       |
|-------------------------------------------|----------------------------------------------------------------------------------------------|
| Invert tray icon colors                   | White tray icons for dark panels; applies immediately                                        |
| Swap tray clicks                          | See [Tray](#tray); applies immediately                                                        |
| Tilt compensation (pixels per row)        | How far elements with *Tilt compensation* lean, see [Elements](elements.md); applies immediately |
| Preview row height                        | How tall a row of LEDs looks on the lid compared with a column's width (0.65 by default, 1 = square), so the [preview](#panel-preview) has the panel's proportions; applies immediately |
| Profile switch fade                       | Seconds to fade the shown profile out, then the next one in, whenever the active profile changes (window, tray, trigger, switch-back, script); 0 switches at once. See [Profiles](profiles.md#switching-profiles) |
| Enable built-in powersave animation       | ASUS's own animations when idle                                                              |
| Boot / awake / sleep / shutdown animation | ASUS built-in animation for each state, by name as `asusctl anime set-builtins` expects them |

Press **Apply built-in animations** to send the powersave and animation settings.

Animatrix only sends built in animation and powersave commands when they differ from what `asusd` reports, which avoids needless USB traffic.

## Lid close delay

With *Turn off → When the lid is closed* ticked, its two options (only usable while it is ticked) decide when:

| Unless plugged in | Delay | Lid closed on mains power | Lid closed on battery |
| --- | --- | --- | --- |
| off | 0 | off at once | off at once |
| off | N s | off after N s | off after N s |
| on | 0 | keeps running | off at once |
| on | N s | keeps running | off after N s |

Unplugging while the lid is already closed starts the delay at that moment; plugging back in or opening the lid turns the panel back on.

Only the first row is left to `asusd` (its normal behaviour). For every other combination Animatrix switches off `asusd`'s own lid handling and watches the lid (`/proc/acpi/button/lid`) and mains power (`/sys/class/power_supply`) itself.

If closing the lid suspends the laptop, the suspend setting takes over first.

## Panel preview

**Preview**, at the right of the status line at the bottom of the window, shows what the panel is displaying: every LED drawn where it sits, turned 45° the way the panel sits on the lid, with unlit LEDs faintly visible. It follows the panel live, including profile switches from triggers or [scripts](scripting.md), and goes dark with it.

Drag the line above the preview up or down to resize it, between 60 and 480 pixels and at most three fifths of the window. Whether it is shown and its size are remembered.

The preview only checks for new frames while it is shown and the window is open, and only redraws when the panel changed.

## Light show switch

The **Light show** switch in the window header (and the tray) turns Animatrix's display on or off. Turning it off blanks the panel; it does not stop `asusd` or change other ASUS settings.

## Tray

- **Left-click** the tray icon to turn the light show on or off. The icon shows the current state.
- **Right-click** for the menu: light show on/off, switch profile, open the window, quit.
- **Middle-click** also turns the light show on or off.
- Tick **Swap tray clicks** on the Settings tab to open the menu with a left click instead; middle-click then turns the light show on or off. Right-click still opens the menu, since the panel handles right clicks itself.
- The icons are black. On a dark panel tick **Invert tray icon colors** on the Settings tab to draw them white; this applies immediately.
- Closing the window keeps Animatrix running in the tray. Use *Quit* in the tray menu to exit.
