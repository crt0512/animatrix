# Profiles

A **profile** is a named group of [elements](elements.md). Exactly one profile is active at a time; pick it with the *Active* button on its card or from the tray menu.

Add a profile with **Add profile**, then add elements to it with the **+ Clock / + Text / + GIF / + Flashlight / + Battery** buttons on its card. A profile without elements leaves the panel dark.

## Layering

By default every element of a profile is drawn at the same time. Each element is rendered on its own canvas and the layers are combined LED by LED, keeping the brightest value, so the order does not change how layers look. A dim [flashlight](elements.md#flashlight) under text makes a lit background with brighter text on top.

To place several elements next to each other, use their **vertical offset** (pixels, negative moves up), for example a clock moved up and a battery moved down.

## Element cycling

Tick **Cycle through elements every N seconds** on a profile to show its elements one at a time instead of layering them:

- each element stays for N seconds, then the next one follows, and the last wraps around to the first;
- elements that are not set up correctly are skipped;
- each element's animation starts from the beginning when its turn comes, so scrolling text always enters from the edge.

### Waiting for animations: *or after animation cycles finish*

Ticking **or after animation cycles finish** lets animated elements set their own turn length. A text or GIF element with **Cycles** ticked stays until it has played that many times, then the next element follows. Elements without a cycle count (clocks, batteries, or text and GIFs without *Cycles*) still use the N seconds.

One cycle is:

| Element                        | One cycle                                                              |
|--------------------------------|------------------------------------------------------------------------|
| Scrolling text                 | one pass across the panel plus the pause                               |
| Bouncing text                  | there and back                                                         |
| Blinking, pulsing, waving text | one period                                                             |
| Typewriter text                | typing out the text plus the hold                                      |
| GIF                            | one loop, at its FPS override if set; forward and back when it bounces |
| GIF in *Animate* layout        | one pass of its movement                                               |

## Switching automatically

**Switch profile automatically**, at the top of the Profiles page, picks a profile to activate when something changes:

- when plugged in
- when unplugged
- when the lid closes
- when the lid opens

Each defaults to *Do nothing*. Only changes count: the state at startup never switches profiles. If the lid and the power change at the same moment, the lid wins. Deleting a profile clears any trigger pointing at it.

This combines with the [lid close delay](device.md#lid-close-delay): a profile picked for "when the lid closes" is what shows while the panel keeps running with the lid shut.

## Order

The **↑ / ↓** buttons on an element card move it within its profile. The order is the rotation order when cycling; layering looks the same in any order.

## Tray

The tray menu lists all profiles and switches between them. Left-clicking the tray icon turns the light show on or off (or opens the window, if you choose); see [Tray](device.md#tray).

## Older configurations

Configurations from before version 0.4, where each profile was a single clock, text, or GIF, are converted automatically: each becomes a profile containing that one element. The old global "cycle through profiles" setting from 0.3 is ignored.
