# Profiles

A **profile** is a named group of [elements](elements.md). Exactly one profile is active at a time; pick it with the *Active* button on its card or from the tray menu.

Add a profile with **Add profile**, then add elements to it with the **+ Clock / + Text / + GIF / + Flashlight / + Battery** buttons on its card. A profile without elements leaves the panel dark.

The **↑ / ↓** buttons in a card's heading move the profile up or down the list, which is also the order of the tray menu. **Duplicate**, at the bottom of an expanded card, adds an independent copy below it ("Name (copy)", with all its elements and settings, not active; triggers keep pointing at the original). **Delete profile…** next to it removes the profile after asking for confirmation. The arrow at the left of a card collapses it to its heading row; element cards inside a profile have one too. When the window opens, only the active profile is expanded and every element is open; what you open or close stays that way until Animatrix quits.

## Layering

Each profile card has an **Elements** choice: **Layered (all at once)**, the default, or **One at a time**. Layered, every element of the profile is drawn at the same time. Each element is rendered on its own canvas and the layers are combined LED by LED, keeping the brightest value, so the order does not change how layers look. A dim [flashlight](elements.md#flashlight) under text makes a lit background with brighter text on top.

To place several elements next to each other, use their **offset** (pixels right and down, negative moves left or up), for example a clock moved up and a battery moved down, or two side by side.

## Element cycling

Choose **One at a time** to show the elements one after another instead. Each element card then shows **Shown for**, how long that element stays:

- a number of **seconds** (every element; 10 by default), or
- a number of **cycles**, complete plays of its animation (text and GIFs only). Static text and still images have nothing to count, so each cycle is one second.

Under it, each element can **fade in** and **fade out** (seconds of brightening from dark at the start of its turn and dimming to dark at its end, within the turn) and add a **pause after** (seconds of dark panel before the next element). Fades also apply to a GIF's overlay text; where fade in and out overlap in a short turn the dimmer one wins. While an element fades the panel is redrawn 30 times a second; otherwise the engine only wakes when a fade-out starts or a turn or pause ends.

Then the next element follows. With **Repeat** ticked (next to the choice, the default), the last wraps around to the first; unticked, the elements play once and the last one stays from then on, whatever its *Shown for*; it fades in but never out. **▶ Play from start**, in the heading of a profile showing one element at a time, starts the elements over from the first one (and makes the profile active if it is not). Elements that are not set up correctly are skipped. Each element's animation starts from the beginning when its turn comes, so scrolling text always enters from the edge.

Older configurations set one interval for the whole profile; their elements get it as their seconds, or their *Cycles* count if the profile waited for animation cycles.

### What one cycle is

| Element                        | One cycle                                                              |
|--------------------------------|------------------------------------------------------------------------|
| Scrolling text                 | one pass across the panel plus the pause                               |
| Bouncing text                  | there and back                                                         |
| Blinking, pulsing, waving text | one period                                                             |
| Typewriter text                | typing out the text plus the hold                                      |
| GIF                            | one loop, at its FPS override if set; forward and back when it bounces |
| GIF in *Animate* layout        | one pass of its movement                                               |

## Switching profiles

With **Profile switch fade** on the Settings tab above 0, a change of active profile, from the window, tray, a trigger, a switch-back, or a [script](scripting.md), fades the shown profile out, then the new one in, each over that many seconds. The old profile keeps animating while it fades; the new one starts from the beginning once it is gone.

It is a fallback for what elements do not set themselves: a profile showing one element at a time fades out by its current element's *fade out*, and fades in by its first element's *fade in*, where those are above 0. Layered profiles always use the setting.

## Switching automatically

**Switch profile automatically**, on the Device behavior tab, picks a profile to activate for each of these actions:

| Trigger | Fires when |
|---|---|
| When the lid is open and plugging in | the charger is connected while the lid is open |
| When the lid is open and unplugging | the charger is disconnected while the lid is open |
| When the lid closes | the lid closes, plugged in or not |
| When the lid opens | the lid opens, plugged in or not |
| When the lid is closed and plugging in | the charger is connected while the lid is closed |
| When the lid is closed and unplugging | the charger is disconnected while the lid is closed |

Each defaults to *Do nothing*. Only changes count: the state at startup never switches profiles. Deleting a profile clears any trigger pointing at it.

Every action belongs to exactly one trigger, so they never compete. If the lid and the power change at the same moment, the lid wins.

### Switching back

Each trigger has **Switch back after (seconds)**. With a value above 0, Animatrix returns to the profile that was active before the trigger once that time has passed. 0 stays on the new profile.

- Picking a profile yourself (in the window or the tray) before then cancels the switch back.
- If another trigger fires first, it takes over; if it also switches back, it returns to the profile from before the first trigger.
- If the profile to return to was deleted, nothing happens.

This combines with the [lid close delay](device.md#lid-close-delay): a profile picked for "when the lid closes" is what shows while the panel keeps running with the lid shut.

## Order

The **↑ / ↓** buttons on an element card move it within its profile, and **Duplicate** adds an identical copy right below it. The order is the rotation order when cycling; layering looks the same in any order.

## Tray

The tray menu lists all profiles and switches between them. Left-clicking the tray icon turns the light show on or off; see [Tray](device.md#tray).

## Older configurations

Configurations from before version 0.4, where each profile was a single clock, text, or GIF, are converted automatically: each becomes a profile containing that one element. The old global "cycle through profiles" setting from 0.3 is ignored.
