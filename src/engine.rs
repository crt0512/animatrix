use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, Timelike};
use image::RgbImage;

use crate::animation::GifAnimation;
use crate::asusctl::{Asusctl, MatrixControl};
use crate::model::{
    AppConfig, DevicePolicy, DisplayProfile, Element, ElementKind, GifLayout, MatrixGeometry, TurnLength,
    MAX_TRANSITION,
    OverlayColor, ProfileTriggers, TextMode, Trigger,
};
use crate::render::{self, MatrixRenderer};
use crate::{matrix, sensors};

#[derive(Clone, Copy, Debug)]
pub enum EngineCommand {
    Refresh,
    /// Start the active profile's elements over from the first.
    Restart,
    ApplyPolicy,
    Shutdown,
}

/// Whoever wants to hear about configuration changes (the tray).
type Listeners = Arc<Mutex<Vec<mpsc::Sender<()>>>>;

/// The LED levels last sent to the panel, in buffer order (see
/// [`crate::matrix::led_positions`]), for the window's preview. Empty while
/// the panel is dark. `serial` counts up with every change.
#[derive(Clone, Debug, Default)]
pub struct PanelFrame {
    pub serial: u64,
    pub leds: Vec<u8>,
}

#[derive(Clone)]
pub struct EngineHandle {
    sender: mpsc::Sender<EngineCommand>,
    status: Arc<Mutex<Option<String>>>,
    listeners: Listeners,
    frame: Arc<Mutex<PanelFrame>>,
}

impl EngineHandle {
    pub fn start(config: Arc<Mutex<AppConfig>>) -> Self {
        Self::start_with(config, Asusctl::default())
    }

    pub fn start_with<C>(config: Arc<Mutex<AppConfig>>, control: C) -> Self
    where
        C: MatrixControl,
    {
        let (sender, receiver) = mpsc::channel();
        let status = Arc::new(Mutex::new(None));
        let listeners = Listeners::default();
        let frame = Arc::new(Mutex::new(PanelFrame::default()));
        let (thread_status, thread_listeners, thread_frame) = (Arc::clone(&status), Arc::clone(&listeners), Arc::clone(&frame));
        thread::spawn(move || run_engine(config, control, receiver, thread_status, thread_listeners, thread_frame));
        Self { sender, status, listeners, frame }
    }

    /// The panel's current frame if it changed since `serial`.
    pub fn frame_after(&self, serial: u64) -> Option<PanelFrame> {
        self.frame.lock().ok().filter(|frame| frame.serial != serial).map(|frame| frame.clone())
    }

    /// A channel that receives `()` whenever the configuration may have
    /// changed: after every command, and when a trigger switches profile.
    pub fn subscribe(&self) -> mpsc::Receiver<()> {
        let (sender, receiver) = mpsc::channel();
        if let Ok(mut listeners) = self.listeners.lock() {
            listeners.push(sender);
        }
        receiver
    }

    pub fn send(&self, command: EngineCommand) {
        let _ = self.sender.send(command);
    }

    pub fn refresh(&self) {
        self.send(EngineCommand::Refresh);
    }

    pub fn apply_policy(&self) {
        self.send(EngineCommand::ApplyPolicy);
    }

    pub fn status(&self) -> Option<String> {
        self.status.lock().ok().and_then(|value| value.clone())
    }
}

/// Redraw rate of the animated battery styles.
const BATTERY_FPS: f64 = 10.0;

/// How often a still battery gauge checks the charge.
const BATTERY_POLL: Duration = Duration::from_secs(5);

/// Retry delay after a frame could not be prepared or sent.
const RETRY: Duration = Duration::from_secs(1);

/// With the light show off, how often to look at the lid and power for
/// profile triggers. Without triggers the engine sleeps until a command.
const TRIGGER_POLL: Duration = Duration::from_secs(1);

fn run_engine<C: MatrixControl>(
    config: Arc<Mutex<AppConfig>>,
    control: C,
    receiver: mpsc::Receiver<EngineCommand>,
    status: Arc<Mutex<Option<String>>>,
    listeners: Listeners,
    shown: Arc<Mutex<PanelFrame>>,
) {
    let geometry = MatrixGeometry::detect();
    // Publishes what the panel shows for the preview; an empty frame is dark.
    let show = |leds: &[u8]| {
        if let Ok(mut frame) = shown.lock() {
            frame.serial += 1;
            frame.leds.clear();
            frame.leds.extend_from_slice(leds);
        }
    };
    let mut dark = true;
    let mut last_key = None;
    let mut last_enabled = None;
    // How long to wait for a command before the next frame; `None` sleeps
    // until one arrives (nothing on screen and nothing to watch).
    let mut tick = Some(RETRY);
    let mut playing: Option<(String, Instant)> = None;
    // The profile drawn last, since when, and its shown element's own
    // fade-out (0 if none), to fade it out when another becomes active.
    let mut drawn: Option<(DisplayProfile, Instant, f32)> = None;
    // A profile being faded out after a switch, and the new one fading in.
    let mut leaving: Option<Leaving> = None;
    let mut entering: Option<(Instant, Duration)> = None;
    let mut gif_cache: HashMap<PathBuf, GifAnimation> = HashMap::new();
    // One animation cycle per element (keyed by its settings), for cycling.
    let mut cycle_cache: HashMap<String, Option<Duration>> = HashMap::new();
    let mut lid_closed_since: Option<Instant> = None;
    // Previous lid/power reading, to spot changes for profile triggers, and
    // which of the two were being watched when it was taken.
    let mut last_lid: Option<LidState> = None;
    let mut last_watch = (false, false);
    // A trigger's pending switch back to the profile before it.
    let mut revert: Option<Revert> = None;

    if let Ok(config) = config.lock() {
        record(&status, control.apply_policy(&config.policy));
    }

    // When the current pass started; waits are measured from here, so the
    // time spent drawing and sending a frame does not push the next one back.
    let mut started = Instant::now();
    loop {
        let command = match tick {
            Some(timeout) => receiver.recv_timeout(timeout.saturating_sub(started.elapsed())),
            None => receiver.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
        };
        started = Instant::now();
        let mut changed = false;
        match command {
            Ok(EngineCommand::Refresh) => {
                last_key = None;
                changed = true;
            }
            Ok(EngineCommand::Restart) => {
                // The profile counts as just shown again.
                playing = None;
                last_key = None;
            }
            Ok(EngineCommand::ApplyPolicy) => {
                if let Ok(config) = config.lock() {
                    record(&status, control.apply_policy(&config.policy));
                }
                last_key = None;
                changed = true;
            }
            Ok(EngineCommand::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        // Paths that bail out below (a hardware or lock error) try again
        // after this; every other path sets its own wake-up.
        tick = Some(RETRY);

        let (snapshot, lid) = match config.lock() {
            Ok(mut config) => {
                // Only read the sensors some setting actually needs.
                let watch = sensors_needed(&config);
                if watch != last_watch {
                    // Start over so a newly watched sensor does not look like a change.
                    last_lid = None;
                    last_watch = watch;
                }
                let lid = LidState {
                    closed: watch.0 && sensors::lid_closed() == Some(true),
                    on_mains: watch.1 && sensors::on_mains() == Some(true),
                };
                changed |= apply_triggers(&mut config, last_lid, lid, &mut revert, Instant::now());
                last_lid = Some(lid);
                (config.clone(), lid)
            }
            Err(error) => {
                set_error(&status, format!("configuration lock failed: {error}"));
                continue;
            }
        };

        if changed {
            notify(&listeners);
        }

        let blanked = snapshot.enabled && lid_blanks(&snapshot.policy, lid, &mut lid_closed_since, Instant::now());
        let enabled = snapshot.enabled && !blanked;
        if last_enabled != Some(enabled) {
            if !record(&status, control.set_enabled(enabled)) {
                continue;
            }
            last_enabled = Some(enabled);
            last_key = None;
        }
        if !enabled {
            if !dark {
                show(&[]);
                dark = true;
            }
            // Nothing on screen to fade out when it comes back.
            (drawn, leaving, entering) = (None, None, None);
            // Nothing to draw. Keep an eye on the lid if it is what blanked
            // the panel, on lid and power if triggers want them, otherwise
            // sleep until the next command.
            // The lid is read once a second while watched, so a blanked
            // panel needs no closer look than that.
            tick = (blanked || last_watch != (false, false) || revert.is_some()).then_some(TRIGGER_POLL);
            continue;
        }

        let Some(target) = snapshot.active_profile().cloned() else {
            set_error(&status, "no active profile selected".into());
            // Picking one is a settings change, which wakes the engine.
            tick = (last_watch != (false, false) || revert.is_some()).then_some(TRIGGER_POLL);
            continue;
        };
        let profile_fade = Duration::from_secs_f32(snapshot.profile_fade.clamp(0.0, MAX_TRANSITION));
        // Another profile became active: fade out the one on screen first,
        // by its element's own fade-out if it has one.
        if leaving.is_none()
            && let Some((from, since, element_out)) = drawn.as_ref().filter(|(from, ..)| from.id != target.id)
        {
            let out = if *element_out > 0.0 { Duration::from_secs_f32(*element_out) } else { profile_fade };
            if !out.is_zero() {
                leaving = Some(Leaving { profile: from.clone(), since: *since, started: Instant::now(), length: out });
            }
        }
        // How bright the whole frame is, for those fades.
        let mut level = 1.0_f32;
        let (profile, since) = match &leaving {
            // Still animating on its own timeline while it fades.
            Some(going) if going.started.elapsed() < going.length => {
                level = 1.0 - going.started.elapsed().as_secs_f32() / going.length.as_secs_f32();
                (going.profile.clone(), going.since)
            }
            _ => {
                let switched = leaving.take().is_some();
                // Animations restart whenever another profile becomes active.
                let since = match &playing {
                    Some((id, since)) if *id == target.id && !switched => *since,
                    _ => {
                        // Fade it in, unless its first element does that itself.
                        let own = target.cycle.enabled
                            && target.elements.first().is_some_and(|element| element.transition.fade_in > 0.0);
                        if (playing.is_some() || switched) && !own && !profile_fade.is_zero() {
                            entering = Some((Instant::now(), profile_fade));
                        }
                        let since = Instant::now();
                        playing = Some((target.id.clone(), since));
                        since
                    }
                };
                if let Some((start, length)) = entering {
                    let into = start.elapsed();
                    if into < length {
                        level = into.as_secs_f32() / length.as_secs_f32();
                    } else {
                        entering = None;
                    }
                }
                (target, since)
            }
        };
        let shown_for = since.elapsed();

        let now = Local::now();
        if cycle_cache.len() > 256 {
            cycle_cache.clear();
        }
        let (visible, next_turn) = visible_elements(&profile, shown_for, |element| {
            turn_length(element, geometry, &mut gif_cache, &mut cycle_cache)
        });
        let element_out = if profile.cycle.enabled {
            visible.first().map_or(0.0, |(element, _, _)| element.transition.fade_out)
        } else {
            0.0
        };
        drawn = Some((profile.clone(), since, element_out));
        // One refresh period per element; the frame is redrawn when any changes.
        // The soonest any visible element needs redrawing; `None` if nothing
        // on screen changes by itself.
        let mut wake: Option<Duration> = None;
        if level < 1.0 {
            earliest(&mut wake, FADE_FRAME);
        }
        let mut periods = Vec::with_capacity(visible.len());
        let mut failed = false;
        for &(element, element_time, fade) in &visible {
            // Fading: redraw smoothly until it settles.
            if fade < 1.0 {
                earliest(&mut wake, FADE_FRAME);
            }
            periods.push((fade * 255.0) as i64);
            let period = match &element.kind {
                ElementKind::Gif { path, fps, looping, layout, mode, motion_fps, overlay_text, overlay_mode, .. } => {
                    if !gif_cache.contains_key(path) {
                        match GifAnimation::open(path) {
                            Ok(animation) => {
                                gif_cache.insert(path.clone(), animation);
                            }
                            Err(error) => {
                                record(&status, Err(error));
                                failed = true;
                                break;
                            }
                        }
                    }
                    let animation = &gif_cache[path];
                    if animation.is_animated() {
                        earliest(&mut wake, animation.until_next_frame(element_time, *fps, *looping));
                    }
                    let frame = animation.index_at(element_time, *fps, *looping) as i64;
                    // An animated overlay steps along with the GIF.
                    if !overlay_text.is_empty() && *overlay_mode != TextMode::Static {
                        let rate = animation.overlay_fps(*fps);
                        earliest(&mut wake, until_next_frame(element_time, rate));
                        periods.push(frame_index(element_time, rate));
                    }
                    if *layout == GifLayout::Animate && *mode != TextMode::Static {
                        // Redraw for the GIF's next frame or the next movement step.
                        let fps = f64::from(motion_fps.max(1.0));
                        earliest(&mut wake, until_next_frame(element_time, fps));
                        let step = frame_index(element_time, fps);
                        (frame << 32) | step
                    } else {
                        frame
                    }
                }
                kind => {
                    if let Some(interval) = frame_interval(kind, now, element_time) {
                        earliest(&mut wake, interval);
                    }
                    refresh_period(kind, now, element_time)
                }
            };
            periods.push(period);
        }
        // Wake right as the next element's turn starts; nothing to watch
        // for in between.
        if let Some(next_turn) = next_turn {
            earliest(&mut wake, next_turn + Duration::from_millis(1));
        }
        if last_watch != (false, false) || revert.is_some() {
            earliest(&mut wake, TRIGGER_POLL);
        }
        tick = wake;
        if failed {
            tick = Some(RETRY);
            continue;
        }
        // Only keep GIFs the active profile still uses.
        gif_cache.retain(|path, _| profile.elements.iter().any(|element| {
            matches!(&element.kind, ElementKind::Gif { path: used, .. } if used == path)
        }));

        let profile_json = serde_json::to_string(&profile).unwrap_or_else(|_| profile.id.clone());
        let shown: Vec<_> = visible.iter().map(|(element, _, _)| element.id.as_str()).collect();
        // Tilt is a global setting, so it is not in the profile.
        let key = format!("{profile_json}:{shown:?}:{periods:?}:{}:{}", snapshot.tilt_per_row, (level * 255.0) as u8);
        if last_key.as_ref() == Some(&key) {
            continue;
        }

        let result = (|| {
            profile.validate()?;
            // Layers combine per LED by taking the brightest value.
            let mut leds = matrix::led_buffer(&RgbImage::new(geometry.width, geometry.height), 1.0, geometry);
            // Overlays drawn over the finished frame, once every layer is in.
            let mut on_top = Vec::new();
            for &(element, element_time, fade) in &visible {
                let layer = match &element.kind {
                    ElementKind::Gif {
                        path, brightness, contrast, black_level, fps, looping, layout, size, overlay_text, overlay_color, overlay_font,
                        overlay_size, overlay_bold, overlay_italic, overlay_outline, overlay_x_offset, overlay_y_offset, motion_fps,
                        smooth_scaling, scale, overlay_rotation, overlay_mode, overlay_direction, overlay_speed, overlay_period,
                        overlay_scroll_pause, invert, invert_gif_only, ..
                    } => {
                        let animation = &gif_cache[path];
                        let frame = animation.index_at(element_time, *fps, *looping);
                        // Drawn with a margin round the panel, so an offset or
                        // turn can bring in what lies past its edge.
                        let stage = render::Stage::padded(geometry);
                        let moved = on_frame_grid(element_time, f64::from(*motion_fps));
                        let mut levels: Vec<u8> = if *layout == GifLayout::Animate {
                            let sprite = animation.sprite(frame, *size, *smooth_scaling);
                            let image = MatrixRenderer::render_sprite_on(element, &sprite, moved, geometry, stage)?;
                            image.pixels().map(|pixel| pixel.0[0]).collect()
                        } else {
                            animation.placed_on(frame, *layout, geometry, stage.canvas, *smooth_scaling, *scale)
                        };
                        crate::animation::apply_tone(&mut levels, *black_level, *contrast);
                        if *invert {
                            // Where the GIF is, for inverting only that: its
                            // placed rectangle, or wherever the sprite is drawn
                            // right now (a solid one, moved the same way).
                            let area: Option<Vec<bool>> = if !*invert_gif_only {
                                None
                            } else if *layout == GifLayout::Animate {
                                let (width, height) = animation.sprite_size(*size);
                                let solid = crate::animation::Sprite { width, height, levels: vec![255; (width * height) as usize] };
                                let image = MatrixRenderer::render_sprite_on(element, &solid, moved, geometry, stage)?;
                                Some(image.pixels().map(|pixel| pixel.0[0] > 0).collect())
                            } else {
                                let (left, top, w, h) = animation.placed_rect(*layout, geometry, stage.canvas, *scale);
                                let width = stage.canvas.width as i64;
                                Some((0..levels.len() as i64).map(|index| {
                                    let (x, y) = (index % width, index / width);
                                    (left..left + w as i64).contains(&x) && (top..top + h as i64).contains(&y)
                                }).collect())
                            };
                            for (index, level) in levels.iter_mut().enumerate() {
                                if area.as_ref().is_none_or(|area| area[index]) {
                                    *level = 255 - *level;
                                }
                            }
                        }
                        // The overlay goes on top of the finished frame, after
                        // every layer, so the GIF's brightness and turns leave it be.
                        if !overlay_text.is_empty() {
                            let style = render::OverlayStyle {
                                color: *overlay_color,
                                bold: *overlay_bold,
                                italic: *overlay_italic,
                                outline: *overlay_outline,
                                smooth: element.smooth_text,
                                offset: (*overlay_x_offset, *overlay_y_offset),
                                rotation: *overlay_rotation,
                                opacity: fade,
                                animation: render::OverlayAnimation {
                                    mode: *overlay_mode,
                                    direction: *overlay_direction,
                                    speed: *overlay_speed,
                                    period: *overlay_period,
                                    pause: *overlay_scroll_pause,
                                    elapsed: on_frame_grid(element_time, animation.overlay_fps(*fps)),
                                },
                            };
                            on_top.push((overlay_text, overlay_font, *overlay_size, style));
                        }
                        let levels = stage.finish_levels(&levels, element, geometry, snapshot.tilt_per_row);
                        (matrix::level_buffer(&levels, *brightness, geometry), None)
                    }
                    ElementKind::Text { fps, outline, invert, color, .. } => {
                        let element_time = on_frame_grid(element_time, f64::from(*fps));
                        let frame = MatrixRenderer::render_tilted(element, now, element_time, geometry, snapshot.tilt_per_row)?;
                        let mut text: Vec<u8> = frame.pixels().map(|pixel| pixel.0[0]).collect();
                        // Around the text as drawn, so it follows every placement.
                        let ring = (*outline > 0).then(|| render::outline_ring(&text, geometry, *outline));
                        // A lit outline goes around the letters, never on them.
                        let lit_ring: Option<Vec<u8>> = ring.as_ref().map(|ring| {
                            ring.iter().zip(&text).map(|(&ring, &glyph)| if glyph > 0 { 0 } else { ring }).collect()
                        });
                        if *invert {
                            for level in &mut text {
                                *level = 255 - *level;
                            }
                        }
                        let to_leds = |levels: &[u8]| matrix::level_buffer(levels, 1.0, geometry);
                        match color {
                            // Lit text; a dark outline cuts out what lies beneath.
                            OverlayColor::White => (to_leds(&text), ring.as_deref().map(to_leds)),
                            // Text cut out of what lies beneath, a lit outline round it.
                            OverlayColor::Black => {
                                let lit_ring = lit_ring.unwrap_or_else(|| vec![0; text.len()]);
                                (to_leds(&lit_ring), Some(to_leds(&text)))
                            }
                        }
                    }
                    kind => {
                        let brightness = match kind {
                            ElementKind::Flashlight { brightness } => *brightness,
                            _ => 1.0,
                        };
                        let frame = MatrixRenderer::render_tilted(element, now, element_time, geometry, snapshot.tilt_per_row)?;
                        (matrix::led_buffer(&frame, brightness, geometry), None)
                    }
                };
                // An outline cuts out what lies beneath before the layer goes on.
                let (mut layer, mut cut) = layer;
                if fade < 1.0 {
                    for level in layer.iter_mut().chain(cut.iter_mut().flatten()) {
                        *level = (f32::from(*level) * fade) as u8;
                    }
                }
                if let Some(ring) = cut {
                    render::cut_under(&mut leds, &ring);
                }
                for (led, level) in leds.iter_mut().zip(layer) {
                    *led = (*led).max(level);
                }
            }
            for (text, font, size, style) in on_top {
                render::overlay_text_on_panel(&mut leds, geometry, text, font, size, style)?;
            }
            if level < 1.0 {
                for led in &mut leds {
                    *led = (f32::from(*led) * level) as u8;
                }
            }
            show(&leds);
            control.write_leds(leds, geometry.model)
        })();
        // The frame was published for the preview even if sending failed.
        dark = false;
        if record(&status, result) {
            last_key = Some(key);
        } else {
            // e.g. asusd restarting: try again soon even if nothing changes.
            tick = Some(RETRY);
        }
    }
}

/// Which sensors are worth reading: (lid, mains power). The lid for engine
/// lid handling or lid triggers, mains power for "unless plugged in" or
/// power triggers.
fn sensors_needed(config: &AppConfig) -> (bool, bool) {
    let policy = &config.policy;
    let triggers = &config.triggers;
    let engine_lid = config.enabled && policy.engine_handles_lid();
    let set = |trigger: &Trigger| trigger.profile.is_some();
    // Power triggers depend on whether the lid is open, so they read both.
    let power_triggers = [&triggers.plugged_in, &triggers.unplugged, &triggers.closed_plugged_in, &triggers.closed_unplugged]
        .into_iter()
        .any(set);
    let lid = engine_lid || power_triggers || set(&triggers.lid_closed) || set(&triggers.lid_opened);
    let power = engine_lid && policy.lid_stay_on_when_plugged || power_triggers;
    (lid, power)
}

fn notify(listeners: &Listeners) {
    if let Ok(mut listeners) = listeners.lock() {
        listeners.retain(|listener| listener.send(()).is_ok());
    }
}

/// The trigger a change from `before` to `now` fires, skipping ones set to
/// "do nothing". Each action has its own trigger, so they never compete for
/// the same change: the lid closing or opening, and plugging in or
/// unplugging with the lid open or with it closed. If the lid and the power
/// change at the same moment, the lid wins.
fn fired_trigger(triggers: &ProfileTriggers, before: LidState, now: LidState) -> Option<&Trigger> {
    let lid = match (before.closed, now.closed) {
        (false, true) => Some(&triggers.lid_closed),
        (true, false) => Some(&triggers.lid_opened),
        _ => None,
    };
    let power = match (before.on_mains, now.on_mains, now.closed) {
        (false, true, false) => Some(&triggers.plugged_in),
        (true, false, false) => Some(&triggers.unplugged),
        (false, true, true) => Some(&triggers.closed_plugged_in),
        (true, false, true) => Some(&triggers.closed_unplugged),
        _ => None,
    };
    [lid, power].into_iter().flatten().find(|trigger| trigger.profile.is_some())
}

/// Switch back to `to` at `at`, unless the active profile is no longer
/// `from` by then (picked by hand or by another trigger).
#[derive(Clone, Debug, PartialEq)]
struct Revert {
    to: Option<String>,
    from: String,
    at: Instant,
}

/// Runs due switch-backs and fired triggers; true if the active profile
/// changed. `before` is `None` on the first reading, which fires nothing.
fn apply_triggers(
    config: &mut AppConfig,
    before: Option<LidState>,
    now: LidState,
    revert: &mut Option<Revert>,
    at: Instant,
) -> bool {
    let exists = |config: &AppConfig, id: &str| config.profiles.iter().any(|profile| profile.id == id);
    let mut changed = false;
    if let Some(pending) = revert.as_ref() {
        if config.active_profile.as_deref() != Some(pending.from.as_str()) {
            *revert = None;
        } else if at >= pending.at {
            if let Some(to) = pending.to.as_deref().filter(|to| exists(config, to)) {
                config.active_profile = Some(to.to_owned());
                changed = true;
            }
            *revert = None;
        }
    }
    let Some(trigger) = before.and_then(|before| fired_trigger(&config.triggers, before, now)) else {
        return changed;
    };
    let Some(id) = trigger.profile.clone().filter(|id| exists(config, id)) else {
        return changed;
    };
    let after = trigger.revert_after_secs.filter(|&secs| secs > 0);
    // Temporary switches in a row all lead back to where the first started.
    let home = match revert.take() {
        Some(pending) => pending.to,
        None => config.active_profile.clone(),
    };
    *revert = after.map(|secs| Revert { to: home, from: id.clone(), at: at + Duration::from_secs(secs.into()) });
    changed |= config.active_profile.as_ref() != Some(&id);
    config.active_profile = Some(id);
    changed
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LidState {
    closed: bool,
    on_mains: bool,
}

/// Whether the engine must blank the panel because of the lid. Only used when
/// asusd's own lid handling is off (see `DevicePolicy::engine_handles_lid`):
/// with "unless plugged in" the panel stays on while on mains power;
/// otherwise it keeps running for the delay after the lid closes, or after
/// unplugging with the lid already closed, then blanks until the lid opens.
fn lid_blanks(policy: &DevicePolicy, lid: LidState, countdown: &mut Option<Instant>, now: Instant) -> bool {
    let counting = policy.engine_handles_lid()
        && lid.closed
        && !(policy.lid_stay_on_when_plugged && lid.on_mains);
    if !counting {
        *countdown = None;
        return false;
    }
    let since = countdown.get_or_insert(now);
    now.duration_since(*since) >= Duration::from_secs(policy.lid_close_delay_secs.into())
}

/// The elements to draw, how long each has been on screen and how bright
/// its fades leave it (1 = full), and how long until something next changes
/// by itself: a fade-out starting, a turn or pause ending (`None`: nothing).
/// Normally all elements, layered, for as long as the profile has been
/// active; with element cycling, one valid element at a time, each for
/// `turn_length` of it, then its pause with nothing shown.
fn visible_elements<'a>(
    profile: &'a DisplayProfile,
    shown_for: Duration,
    mut turn_length: impl FnMut(&Element) -> Duration,
) -> (Vec<(&'a Element, Duration, f32)>, Option<Duration>) {
    if !profile.cycle.enabled || profile.elements.len() < 2 {
        return (profile.elements.iter().map(|element| (element, shown_for, 1.0)).collect(), None);
    }
    let valid: Vec<_> = profile.elements.iter().filter(|element| element.validate().is_ok()).collect();
    // With nothing valid, cycle anyway so the errors are reported.
    let pool: Vec<_> = if valid.is_empty() { profile.elements.iter().collect() } else { valid };
    let nanos = |seconds: f32| Duration::from_secs_f32(seconds.max(0.0)).as_nanos();
    // (turn, pause) per element.
    let slots: Vec<(u128, u128)> = pool.iter()
        .map(|element| (turn_length(element).as_nanos().max(MIN_TURN.as_nanos()), nanos(element.transition.pause)))
        .collect();
    let shown_for = shown_for.as_nanos();
    let mut into_round = if profile.cycle.repeat {
        shown_for % slots.iter().map(|(turn, pause)| turn + pause).sum::<u128>()
    } else {
        // Played once through, the last element stays however long its turn:
        // it fades in, but never out.
        let before_last: u128 = slots[..slots.len() - 1].iter().map(|(turn, pause)| turn + pause).sum();
        if shown_for >= before_last {
            let last = *pool.last().expect("cycling needs two elements");
            let time = shown_for - before_last;
            return (vec![(last, Duration::from_nanos(time as u64), fade(last, time, None))], None);
        }
        shown_for
    };
    for (element, (turn, pause)) in pool.into_iter().zip(slots) {
        if into_round < turn {
            let fade_out = nanos(element.transition.fade_out).min(turn);
            // Next: the fade-out starting, or the turn ending.
            let next = if into_round < turn - fade_out { turn - fade_out - into_round } else { turn - into_round };
            let shown = (element, Duration::from_nanos(into_round as u64), fade(element, into_round, Some(turn)));
            return (vec![shown], Some(Duration::from_nanos(next as u64)));
        }
        into_round -= turn;
        if into_round < pause {
            return (Vec::new(), Some(Duration::from_nanos((pause - into_round) as u64)));
        }
        into_round -= pause;
    }
    unreachable!("position is within one round")
}

/// How bright `element`'s fades leave it `time` nanoseconds into its turn,
/// which ends at `turn` (`None`: it stays, so it never fades out). Where the
/// fades overlap the dimmer one wins.
fn fade(element: &Element, time: u128, turn: Option<u128>) -> f32 {
    let seconds = |nanos: u128| nanos as f64 / 1e9;
    let ramp = |into: f64, length: f32| if length > 0.0 { (into / f64::from(length)).clamp(0.0, 1.0) } else { 1.0 };
    let fade_in = ramp(seconds(time), element.transition.fade_in);
    let fade_out = turn.map_or(1.0, |turn| ramp(seconds(turn.saturating_sub(time)), element.transition.fade_out));
    fade_in.min(fade_out) as f32
}

/// A profile on its way out after another became active.
struct Leaving {
    profile: DisplayProfile,
    /// When it started showing, so its animations carry on while it fades.
    since: Instant,
    started: Instant,
    length: Duration,
}

/// Redraw rate while an element fades.
const FADE_FRAME: Duration = Duration::from_millis(33);

/// One "cycle" of an element with no animation to count.
const STILL_CYCLE: Duration = Duration::from_secs(1);

/// Shortest time any element is shown when cycling.
const MIN_TURN: Duration = Duration::from_millis(100);

/// How long an element stays on screen when its profile cycles: its seconds,
/// or its cycle count times one animation cycle.
fn turn_length(
    element: &Element,
    geometry: MatrixGeometry,
    gif_cache: &mut HashMap<PathBuf, GifAnimation>,
    cycle_cache: &mut HashMap<String, Option<Duration>>,
) -> Duration {
    let plays = match element.turn.unwrap_or_default() {
        TurnLength::Seconds(seconds) => return Duration::from_secs(seconds.max(1).into()),
        TurnLength::Cycles(plays) => plays.max(1),
    };
    // Something that never repeats (static text, a still image) counts one
    // second per cycle.
    let fallback = STILL_CYCLE * plays;
    let key = serde_json::to_string(element).unwrap_or_else(|_| element.id.clone());
    let one_play = *cycle_cache.entry(key).or_insert_with(|| match &element.kind {
        ElementKind::Gif { path, fps, looping, layout, size, .. } => {
            if !gif_cache.contains_key(path) {
                let animation = GifAnimation::open(path).ok()?;
                gif_cache.insert(path.clone(), animation);
            }
            let animation = &gif_cache[path];
            // A moving GIF counts passes of its movement, otherwise loops.
            let movement = (*layout == GifLayout::Animate)
                .then(|| MatrixRenderer::sprite_cycle(element, &animation.sprite(0, *size, true), geometry).ok().flatten())
                .flatten();
            // A still GIF has no loop to count.
            movement.or_else(|| animation.is_animated().then(|| animation.cycle_duration(*fps, *looping)))
        }
        _ => MatrixRenderer::text_cycle(element, geometry).ok().flatten(),
    });
    one_play.map_or(fallback, |one_play| one_play * plays)
}

/// Changes whenever the frame for a non-GIF profile needs redrawing.
fn refresh_period(kind: &ElementKind, now: DateTime<Local>, elapsed: Duration) -> i64 {
    match kind {
        ElementKind::Clock { show_millis: true, fps, .. } => {
            (now.timestamp_millis() as f64 * f64::from(fps.max(1.0)) / 1000.0) as i64
        }
        ElementKind::Clock { show_seconds: true, .. } => now.timestamp(),
        ElementKind::Clock { .. } => now.timestamp() - i64::from(now.second()),
        ElementKind::Text { mode, fps, .. } if *mode != TextMode::Static => frame_index(elapsed, f64::from(*fps)),
        ElementKind::Battery { style, .. } => {
            let state = sensors::battery()
                .map_or(-1, |battery| i64::from(battery.percent) * 2 + i64::from(battery.charging));
            // Animated styles also change with time.
            let step = if style.is_animated() { (elapsed.as_secs_f64() * BATTERY_FPS) as i64 } else { 0 };
            (step << 8) | (state & 0xff)
        }
        ElementKind::Text { .. }
        | ElementKind::Gif { .. }
        | ElementKind::Flashlight { .. } => 0,
    }
}

/// Frame number `elapsed` falls in at `fps`. The nudge keeps a frame time
/// that float error puts a hair before its boundary in that frame.
fn frame_index(elapsed: Duration, fps: f64) -> i64 {
    (elapsed.as_secs_f64() * fps.max(1.0) + 1e-6).floor() as i64
}

/// `elapsed` moved back to the start of its frame. Animations are drawn at
/// that time, not at whenever the engine woke up, so every frame moves by
/// the same amount: one pixel per frame when the FPS matches the speed,
/// however late or early the wake-up was.
fn on_frame_grid(elapsed: Duration, fps: f64) -> Duration {
    Duration::from_secs_f64(frame_index(elapsed, fps) as f64 / fps.max(1.0))
}

/// Time until the next frame starts, plus a moment so the wake-up lands
/// inside it. Frames keep to a fixed grid instead of each waiting a full
/// interval after the previous one finished.
fn until_next_frame(elapsed: Duration, fps: f64) -> Duration {
    const SETTLE: Duration = Duration::from_millis(1);
    let next = Duration::from_secs_f64((frame_index(elapsed, fps) + 1) as f64 / fps.max(1.0));
    next.saturating_sub(elapsed) + SETTLE
}

/// How long until a non-GIF element may look different; `None` if it only
/// changes when its settings do.
fn frame_interval(kind: &ElementKind, now: DateTime<Local>, elapsed: Duration) -> Option<Duration> {
    // Just past the boundary, so the new second/minute is already showing.
    const SETTLE: Duration = Duration::from_millis(5);
    let into_second = Duration::from_nanos(now.timestamp_subsec_nanos().into());
    match kind {
        ElementKind::Text { mode, fps, .. } if *mode != TextMode::Static => Some(until_next_frame(elapsed, f64::from(*fps))),
        ElementKind::Text { .. } | ElementKind::Flashlight { .. } => None,
        ElementKind::Battery { style, .. } if style.is_animated() => Some(Duration::from_secs_f64(1.0 / BATTERY_FPS)),
        ElementKind::Battery { .. } => Some(BATTERY_POLL),
        ElementKind::Clock { show_millis: true, fps, .. } => {
            // On the wall clock's grid, matching `refresh_period`.
            let since_epoch = Duration::from_millis(now.timestamp_millis().max(0) as u64);
            Some(until_next_frame(since_epoch, f64::from(*fps)))
        }
        ElementKind::Clock { show_seconds: true, .. } => Some(Duration::from_secs(1) - into_second + SETTLE),
        ElementKind::Clock { .. } => {
            let to_minute = Duration::from_secs(u64::from(59 - now.second().min(59))) + Duration::from_secs(1) - into_second;
            Some(to_minute + SETTLE)
        }
        ElementKind::Gif { .. } => None,
    }
}

fn earliest(wake: &mut Option<Duration>, candidate: Duration) {
    *wake = Some(wake.map_or(candidate, |current| current.min(candidate)));
}

fn record(status: &Arc<Mutex<Option<String>>>, result: anyhow::Result<()>) -> bool {
    match result {
        Ok(()) => {
            if let Ok(mut status) = status.lock() {
                *status = None;
            }
            true
        }
        Err(error) => {
            set_error(status, format!("{error:#}"));
            false
        }
    }
}

fn set_error(status: &Arc<Mutex<Option<String>>>, message: String) {
    eprintln!("animatrix: {message}");
    if let Ok(mut status) = status.lock() {
        *status = Some(message);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::model::MatrixModel;

    #[derive(Default)]
    struct FakeControl {
        frames: Arc<AtomicUsize>,
        last: Arc<Mutex<Vec<u8>>>,
    }

    impl MatrixControl for FakeControl {
        fn set_enabled(&self, _: bool) -> anyhow::Result<()> { Ok(()) }
        fn apply_policy(&self, _: &DevicePolicy) -> anyhow::Result<()> { Ok(()) }
        fn write_leds(&self, leds: Vec<u8>, _: MatrixModel) -> anyhow::Result<()> {
            *self.last.lock().unwrap() = leds;
            self.frames.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    /// Runs the engine until it has written one frame and returns it.
    fn first_frame(config: AppConfig) -> Vec<u8> {
        let control = FakeControl::default();
        let (frames, last) = (Arc::clone(&control.frames), Arc::clone(&control.last));
        let handle = EngineHandle::start_with(Arc::new(Mutex::new(config)), control);
        handle.refresh();
        let deadline = Instant::now() + Duration::from_secs(2);
        while frames.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
            thread::yield_now();
        }
        handle.send(EngineCommand::Shutdown);
        assert!(frames.load(Ordering::Relaxed) > 0, "no frame written");
        last.lock().unwrap().clone()
    }

    #[test]
    fn engine_renders_active_profile() {
        assert!(first_frame(AppConfig::default()).iter().any(|&led| led > 0));
    }

    #[test]
    fn preview_sees_what_the_panel_was_sent_and_goes_dark_with_it() {
        let control = FakeControl::default();
        let (frames, last) = (Arc::clone(&control.frames), Arc::clone(&control.last));
        let config = Arc::new(Mutex::new(AppConfig::default()));
        let handle = EngineHandle::start_with(Arc::clone(&config), control);
        handle.refresh();
        let wait_for = |done: &dyn Fn() -> bool| {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !done() && Instant::now() < deadline {
                thread::yield_now();
            }
        };
        wait_for(&|| frames.load(Ordering::Relaxed) > 0);
        let shown = handle.frame_after(0).expect("a frame for the preview");
        assert_eq!(shown.leds, *last.lock().unwrap());
        assert!(handle.frame_after(shown.serial).is_none(), "unchanged frames are not handed out again");

        config.lock().unwrap().enabled = false;
        handle.refresh();
        wait_for(&|| handle.frame_after(shown.serial).is_some_and(|frame| frame.leds.is_empty()));
        let dark = handle.frame_after(shown.serial).unwrap();
        handle.send(EngineCommand::Shutdown);
        assert!(dark.leds.is_empty());
    }

    #[test]
    fn elements_only_wake_the_engine_when_they_can_change() {
        use chrono::TimeZone;
        use crate::model::Element;

        let now = Local.with_ymd_and_hms(2026, 10, 1, 12, 34, 50).unwrap() + chrono::Duration::milliseconds(250);
        let clock = |seconds| {
            let mut element = Element::clock();
            if let ElementKind::Clock { show_seconds, .. } = &mut element.kind {
                *show_seconds = seconds;
            }
            frame_interval(&element.kind, now, Duration::ZERO).unwrap()
        };
        // 12:34:50.250 -> next minute in 9.75 s, next second in 0.75 s (plus a few ms).
        assert!((Duration::from_millis(9_750)..Duration::from_millis(9_800)).contains(&clock(false)));
        assert!((Duration::from_millis(750)..Duration::from_millis(800)).contains(&clock(true)));

        let mut still = Element::text();
        if let ElementKind::Text { mode, .. } = &mut still.kind {
            *mode = TextMode::Static;
        }
        assert_eq!(frame_interval(&still.kind, now, Duration::ZERO), None);
        assert_eq!(frame_interval(&Element::flashlight().kind, now, Duration::ZERO), None);
        // Animations run at their FPS (5 by default for text), on a fixed
        // grid: 30 ms into a frame, the next one is 170 ms away.
        let text = |elapsed| frame_interval(&Element::text().kind, now, elapsed).unwrap();
        assert_eq!(text(Duration::ZERO), Duration::from_millis(201));
        assert_eq!(text(Duration::from_millis(1_030)), Duration::from_millis(171));
    }

    fn fired<'a>(triggers: &'a ProfileTriggers, before: LidState, now: LidState) -> Option<&'a str> {
        fired_trigger(triggers, before, now).and_then(|trigger| trigger.profile.as_deref())
    }

    #[test]
    fn animation_frames_keep_to_a_fixed_grid() {
        // 30 FPS: waking a hair early or late still draws the same frame time,
        // and float error at an exact boundary does not fall a frame short.
        let fps = 30.0;
        for frame in [1_i64, 29, 30, 9_000, 30_000_000] {
            let start = Duration::from_secs_f64(frame as f64 / fps);
            assert_eq!(frame_index(start, fps), frame);
            assert_eq!(frame_index(start + Duration::from_millis(20), fps), frame);
            assert_eq!(on_frame_grid(start + Duration::from_millis(20), fps), on_frame_grid(start, fps));
        }
        assert_eq!(frame_index(Duration::from_secs_f64(1.0 / fps) - Duration::from_micros(500), fps), 0);
    }

    #[test]
    fn triggers_fire_on_changes_only() {
        let triggers = ProfileTriggers {
            plugged_in: Trigger::to("ac"),
            unplugged: Trigger::to("battery"),
            lid_closed: Trigger::to("closed"),
            ..ProfileTriggers::default()
        };
        let state = |closed, on_mains| LidState { closed, on_mains };
        assert_eq!(fired(&triggers, state(false, false), state(false, false)), None);
        assert_eq!(fired(&triggers, state(false, false), state(false, true)), Some("ac"));
        assert_eq!(fired(&triggers, state(false, true), state(false, false)), Some("battery"));
        assert_eq!(fired(&triggers, state(false, true), state(true, true)), Some("closed"));
        // "Do nothing" for opening the lid.
        assert_eq!(fired(&triggers, state(true, true), state(false, true)), None);
        // Lid and power at once: the lid wins.
        assert_eq!(fired(&triggers, state(false, true), state(true, false)), Some("closed"));
    }

    #[test]
    fn each_action_has_its_own_trigger() {
        let triggers = ProfileTriggers {
            plugged_in: Trigger::to("open plug"),
            unplugged: Trigger::to("open unplug"),
            lid_closed: Trigger::to("closed"),
            lid_opened: Trigger::to("opened"),
            closed_plugged_in: Trigger::to("closed plug"),
            closed_unplugged: Trigger::to("closed unplug"),
        };
        let state = |closed, on_mains| LidState { closed, on_mains };
        // Plugging in or unplugging, with the lid open or closed.
        assert_eq!(fired(&triggers, state(false, false), state(false, true)), Some("open plug"));
        assert_eq!(fired(&triggers, state(false, true), state(false, false)), Some("open unplug"));
        assert_eq!(fired(&triggers, state(true, false), state(true, true)), Some("closed plug"));
        assert_eq!(fired(&triggers, state(true, true), state(true, false)), Some("closed unplug"));
        // The lid closing or opening, plugged in or not.
        for on_mains in [false, true] {
            assert_eq!(fired(&triggers, state(false, on_mains), state(true, on_mains)), Some("closed"));
            assert_eq!(fired(&triggers, state(true, on_mains), state(false, on_mains)), Some("opened"));
        }
    }

    #[test]
    fn triggers_switch_back_after_their_timeout() {
        let mut config = AppConfig::default();
        let home = config.profiles[0].id.clone();
        let away = DisplayProfile::new("Away", Vec::new());
        let (away_id, start) = (away.id.clone(), Instant::now());
        config.profiles.push(away);
        config.triggers.unplugged = Trigger { profile: Some(away_id.clone()), revert_after_secs: Some(60) };
        let (plugged, unplugged) = (LidState { closed: false, on_mains: true }, LidState { closed: false, on_mains: false });
        let later = |seconds| start + Duration::from_secs(seconds);
        let mut revert = None;

        assert!(apply_triggers(&mut config, Some(plugged), unplugged, &mut revert, later(0)));
        assert_eq!(config.active_profile.as_ref(), Some(&away_id));
        assert!(!apply_triggers(&mut config, Some(unplugged), unplugged, &mut revert, later(59)));
        assert!(apply_triggers(&mut config, Some(unplugged), unplugged, &mut revert, later(60)));
        assert_eq!(config.active_profile.as_ref(), Some(&home));
        assert_eq!(revert, None);

        // Picking a profile by hand cancels the switch back.
        apply_triggers(&mut config, Some(plugged), unplugged, &mut revert, later(100));
        config.active_profile = Some(home.clone());
        apply_triggers(&mut config, Some(unplugged), unplugged, &mut revert, later(101));
        assert_eq!(revert, None);
    }

    #[test]
    fn deleted_profiles_stop_triggering() {
        let config = AppConfig::default();
        let before = LidState { closed: false, on_mains: true };
        let after = LidState { closed: false, on_mains: false };
        let mut triggers = ProfileTriggers { unplugged: Trigger::to(&config.profiles[0].id), ..ProfileTriggers::default() };
        assert_eq!(fired(&triggers, before, after), Some(config.profiles[0].id.as_str()));
        triggers.forget(&config.profiles[0].id);
        assert_eq!(fired(&triggers, before, after), None);
    }

    #[test]
    fn lid_rules_follow_power_and_delay() {
        let start = Instant::now();
        let later = |seconds| start + Duration::from_secs(seconds);
        let (closed_ac, closed_battery, open) = (
            LidState { closed: true, on_mains: true },
            LidState { closed: true, on_mains: false },
            LidState { closed: false, on_mains: false },
        );
        let mut policy = DevicePolicy { off_when_lid_closed: true, lid_stay_on_when_plugged: true, ..DevicePolicy::default() };
        policy.lid_close_delay_secs = 10;
        let mut countdown = None;

        // Plugged in: stays on however long the lid is closed.
        assert!(!lid_blanks(&policy, closed_ac, &mut countdown, later(0)));
        assert!(!lid_blanks(&policy, closed_ac, &mut countdown, later(600)));
        // Unplugged with the lid closed: the delay starts now.
        assert!(!lid_blanks(&policy, closed_battery, &mut countdown, later(600)));
        assert!(!lid_blanks(&policy, closed_battery, &mut countdown, later(609)));
        assert!(lid_blanks(&policy, closed_battery, &mut countdown, later(610)));
        // Plugging back in or opening the lid turns it back on.
        assert!(!lid_blanks(&policy, closed_ac, &mut countdown, later(611)));
        assert!(!lid_blanks(&policy, open, &mut countdown, later(612)));

        // "Unless plugged in" with no delay: off at once on battery.
        policy.lid_close_delay_secs = 0;
        assert!(lid_blanks(&policy, closed_battery, &mut None, later(0)));
        // Without "unless plugged in" the delay applies on mains too.
        policy.lid_stay_on_when_plugged = false;
        policy.lid_close_delay_secs = 5;
        let mut countdown = None;
        assert!(!lid_blanks(&policy, closed_ac, &mut countdown, later(0)));
        assert!(lid_blanks(&policy, closed_ac, &mut countdown, later(5)));
        // Plain "turn off when closed" is asusd's job.
        policy.lid_close_delay_secs = 0;
        assert!(!lid_blanks(&policy, closed_battery, &mut None, later(0)));
    }

    #[test]
    fn element_cycling_rotates_one_valid_element_at_a_time() {
        use crate::model::{DisplayProfile, Element};

        let (clock, mut broken_gif, text) = (Element::clock(), Element::gif(), Element::text());
        if let ElementKind::Gif { path, .. } = &mut broken_gif.kind {
            *path = "notes.txt".into();
        }
        let mut profile = DisplayProfile::new("Cycle", vec![clock.clone(), broken_gif, text.clone()]);
        let shown = |profile: &DisplayProfile, seconds: u64| -> Vec<(String, Duration)> {
            visible_elements(profile, Duration::from_secs(seconds), |_| Duration::from_secs(5)).0
                .into_iter().map(|(element, time, _)| (element.id.clone(), time)).collect()
        };

        // Without cycling every element is layered for the whole time.
        assert_eq!(shown(&profile, 7).len(), 3);

        profile.cycle.enabled = true;
        // The GIF points at a non-GIF file, so it is skipped; each element restarts its clock.
        assert_eq!(shown(&profile, 2), [(clock.id.clone(), Duration::from_secs(2))]);
        assert_eq!(shown(&profile, 7), [(text.id.clone(), Duration::from_secs(2))]);
        assert_eq!(shown(&profile, 11), [(clock.id, Duration::from_secs(1))]);
    }

    #[test]
    fn cycling_honours_each_elements_turn_length() {
        use crate::model::{DisplayProfile, Element};

        let (short, long) = (Element::clock(), Element::text());
        let mut profile = DisplayProfile::new("Turns", vec![short.clone(), long.clone()]);
        profile.cycle.enabled = true;
        let turn = |element: &Element| Duration::from_secs(if element.id == short.id { 2 } else { 5 });
        let at = |millis: u64| {
            let shown = visible_elements(&profile, Duration::from_millis(millis), turn).0;
            (shown[0].0.id.clone(), shown[0].1)
        };
        assert_eq!(at(1_000), (short.id.clone(), Duration::from_secs(1)));
        assert_eq!(at(3_000), (long.id.clone(), Duration::from_secs(1)));
        assert_eq!(at(7_500), (short.id.clone(), Duration::from_millis(500)));
    }

    #[test]
    fn without_repeat_the_last_element_stays() {
        use crate::model::{DisplayProfile, Element};

        let (first, last) = (Element::clock(), Element::text());
        let mut profile = DisplayProfile::new("Once", vec![first.clone(), last.clone()]);
        profile.cycle.enabled = true;
        let at = |profile: &DisplayProfile, seconds: u64| {
            let shown = visible_elements(profile, Duration::from_secs(seconds), |_| Duration::from_secs(5)).0;
            (shown[0].0.id.clone(), shown[0].1)
        };
        assert_eq!(at(&profile, 12).0, first.id);
        profile.cycle.repeat = false;
        assert_eq!(at(&profile, 2), (first.id.clone(), Duration::from_secs(2)));
        // Past its own 5 seconds and well beyond: the last one stays, its time running on.
        assert_eq!(at(&profile, 12), (last.id.clone(), Duration::from_secs(7)));
        assert_eq!(at(&profile, 3_600), (last.id, Duration::from_secs(3_595)));
    }

    #[test]
    fn cycling_wakes_only_when_the_next_turn_starts() {
        use crate::model::{DisplayProfile, Element};

        let mut profile = DisplayProfile::new("Turns", vec![Element::clock(), Element::text()]);
        let next = |profile: &DisplayProfile, millis: u64| {
            visible_elements(profile, Duration::from_millis(millis), |_| Duration::from_secs(5)).1
        };
        // Layered: nothing switches by itself.
        assert_eq!(next(&profile, 1_200), None);
        profile.cycle.enabled = true;
        assert_eq!(next(&profile, 1_200), Some(Duration::from_millis(3_800)));
        assert_eq!(next(&profile, 7_000), Some(Duration::from_millis(3_000)));
        // Played once through, the last element has no next turn.
        profile.cycle.repeat = false;
        assert_eq!(next(&profile, 7_000), None);
    }

    #[test]
    fn switching_profiles_fades_out_then_in() {
        use crate::model::{DisplayProfile, Element};

        let (first, second) = (DisplayProfile::new("A", vec![Element::flashlight()]), DisplayProfile::new("B", vec![Element::flashlight()]));
        let second_id = second.id.clone();
        let config = Arc::new(Mutex::new(AppConfig {
            active_profile: Some(first.id.clone()),
            profiles: vec![first, second],
            profile_fade: 0.3,
            ..AppConfig::default()
        }));
        let control = FakeControl::default();
        let (frames, last) = (Arc::clone(&control.frames), Arc::clone(&control.last));
        let handle = EngineHandle::start_with(Arc::clone(&config), control);
        handle.refresh();
        let brightest = || last.lock().unwrap().iter().copied().max().unwrap_or(0);
        let deadline = Instant::now() + Duration::from_secs(2);
        while (frames.load(Ordering::Relaxed) == 0 || brightest() < 255) && Instant::now() < deadline {
            thread::yield_now();
        }
        assert_eq!(brightest(), 255, "starts at full brightness, no fade on startup");

        config.lock().unwrap().active_profile = Some(second_id);
        handle.refresh();
        // Sample the panel through the fade-out and fade-in.
        let mut seen = Vec::new();
        let deadline = Instant::now() + Duration::from_millis(900);
        while Instant::now() < deadline {
            seen.push(brightest());
            thread::sleep(Duration::from_millis(10));
        }
        handle.send(EngineCommand::Shutdown);
        assert!(seen.iter().any(|&level| level > 0 && level < 200), "{seen:?}");
        assert_eq!(*seen.last().unwrap(), 255, "ends at full brightness");
    }

    #[test]
    fn elements_fade_and_pause_around_their_turns() {
        use crate::model::{DisplayProfile, Element, Transition};

        let (first, second) = (Element::clock(), Element::text());
        let first = Element { transition: Transition { fade_in: 1.0, fade_out: 2.0, pause: 1.5 }, ..first };
        let mut profile = DisplayProfile::new("Fades", vec![first.clone(), second.clone()]);
        profile.cycle.enabled = true;
        let at = |profile: &DisplayProfile, millis: u64| {
            let (shown, next) = visible_elements(profile, Duration::from_millis(millis), |_| Duration::from_secs(5));
            (shown.first().map(|(element, _, fade)| (element.id.clone(), *fade)), next)
        };
        // Fading in over the first second, then full until the fade-out starts at 3 s.
        assert_eq!(at(&profile, 500), (Some((first.id.clone(), 0.5)), Some(Duration::from_millis(2_500))));
        assert_eq!(at(&profile, 2_000), (Some((first.id.clone(), 1.0)), Some(Duration::from_secs(1))));
        // Fading out over the last two seconds of its turn.
        assert_eq!(at(&profile, 4_000), (Some((first.id.clone(), 0.5)), Some(Duration::from_secs(1))));
        // Then a dark pause before the next element.
        assert_eq!(at(&profile, 5_500), (None, Some(Duration::from_secs(1))));
        assert_eq!(at(&profile, 6_600).0, Some((second.id, 1.0)));
        // Played once through, the last element never fades out.
        profile.elements.reverse();
        profile.cycle.repeat = false;
        assert_eq!(at(&profile, 60_000), (Some((first.id, 1.0)), None));
    }

    #[test]
    fn elements_stay_for_their_seconds_or_cycles() {
        use crate::model::Element;

        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let turn = |element: &Element| turn_length(element, geometry, &mut HashMap::new(), &mut HashMap::new());
        let with = |element: Element, turn: TurnLength| Element { turn: Some(turn), ..element };
        let text = Element::text();
        let one_play = MatrixRenderer::text_cycle(&text, geometry).unwrap().unwrap();
        assert_eq!(turn(&with(text.clone(), TurnLength::Seconds(7))), Duration::from_secs(7));
        assert_eq!(turn(&with(text.clone(), TurnLength::Cycles(3))), one_play * 3);
        // Nothing to count: static text takes a second per cycle.
        let mut still = with(text, TurnLength::Cycles(3));
        if let ElementKind::Text { mode, .. } = &mut still.kind {
            *mode = TextMode::Static;
        }
        assert_eq!(turn(&still), Duration::from_secs(3));
        assert_eq!(turn(&Element::clock()), Duration::from_secs(crate::model::DEFAULT_TURN_SECONDS.into()));
    }

    /// The first frame of a profile made of `elements`.
    fn frame_of(elements: Vec<crate::model::Element>) -> Vec<u8> {
        let profile = crate::model::DisplayProfile::new("Test", elements);
        first_frame(AppConfig { active_profile: Some(profile.id.clone()), profiles: vec![profile], ..AppConfig::default() })
    }

    fn still_text(outline: u32, invert: bool) -> crate::model::Element {
        let mut text = crate::model::Element::text();
        if let ElementKind::Text { mode, text, outline: o, invert: i, .. } = &mut text.kind {
            (*mode, *text, *o, *i) = (TextMode::Static, "Hi".into(), outline, invert);
        }
        text
    }

    fn black(mut text: crate::model::Element) -> crate::model::Element {
        if let ElementKind::Text { color, .. } = &mut text.kind {
            *color = OverlayColor::Black;
        }
        text
    }

    #[test]
    fn black_text_cuts_itself_out_with_a_lit_outline() {
        let mut light = crate::model::Element::flashlight();
        if let ElementKind::Flashlight { brightness } = &mut light.kind {
            *brightness = 0.4;
        }
        let background = (255.0_f32 * 0.4) as u8;
        let dark = |leds: &[u8]| leds.iter().filter(|&&led| led == 0).count();
        let alone = frame_of(vec![light.clone()]);
        let cut = frame_of(vec![light.clone(), black(still_text(0, false))]);
        // Dark letters in the dim background, nothing brighter than it.
        assert!(dark(&cut) > dark(&alone));
        assert!(cut.iter().all(|&led| led <= background));
        // The outline lights a ring round the letters, which stay dark.
        let outlined = frame_of(vec![light, black(still_text(2, false))]);
        assert!(outlined.iter().any(|&led| led > background));
        assert!(dark(&outlined) > dark(&alone));
    }

    #[test]
    fn text_outline_cuts_out_the_layers_beneath() {
        let light = crate::model::Element::flashlight();
        let lit = |leds: &[u8]| leds.iter().filter(|&&led| led > 0).count();
        let plain = frame_of(vec![light.clone(), still_text(0, false)]);
        let outlined = frame_of(vec![light, still_text(2, false)]);
        // A full flashlight lights everything; the outline darkens a ring.
        assert!(lit(&outlined) < lit(&plain));
        assert!(outlined.contains(&255));
    }

    #[test]
    fn inverted_text_lights_the_panel_around_dark_text() {
        let lit = |leds: &[u8]| leds.iter().filter(|&&led| led > 128).count();
        let (plain, inverted) = (frame_of(vec![still_text(0, false)]), frame_of(vec![still_text(0, true)]));
        assert!(lit(&inverted) > 10 * lit(&plain));
        assert!(inverted.contains(&0));
    }

    #[test]
    fn gifs_can_invert_only_their_own_area() {
        let invert = |own_area: bool| {
            let mut gif = crate::model::Element::gif();
            if let ElementKind::Gif { invert, invert_gif_only, scale, .. } = &mut gif.kind {
                (*invert, *invert_gif_only, *scale) = (true, own_area, 0.3);
            }
            frame_of(vec![gif]).iter().filter(|&&led| led > 128).count()
        };
        let (whole, own) = (invert(false), invert(true));
        // The whole panel lights up around a small GIF, unless only its area inverts.
        assert!(own > 0 && own * 4 < whole, "own {own}, whole {whole}");
    }

    #[test]
    fn png_images_show_on_the_panel() {
        let directory = tempfile::tempdir().unwrap();
        let png = directory.path().join("picture.png");
        image::RgbaImage::from_pixel(10, 10, image::Rgba([255, 255, 255, 255])).save(&png).unwrap();
        let mut picture = crate::model::Element::gif();
        if let ElementKind::Gif { path, .. } = &mut picture.kind {
            *path = png;
        }
        picture.validate().expect("PNG files are accepted");
        // A white 10x10 square, as is: about a hundred lit LEDs.
        let lit = frame_of(vec![picture]).iter().filter(|&&led| led > 200).count();
        assert!((50..=150).contains(&lit), "{lit}");
    }

    #[test]
    fn inverted_gifs_light_their_dark_pixels() {
        let mut gif = crate::model::Element::gif();
        let plain = frame_of(vec![gif.clone()]);
        if let ElementKind::Gif { invert, .. } = &mut gif.kind {
            *invert = true;
        }
        let inverted = frame_of(vec![gif]);
        let total = |leds: &[u8]| leds.iter().map(|&led| u64::from(led)).sum::<u64>();
        assert_ne!(plain, inverted);
        assert!(total(&inverted) > total(&plain));
    }

    #[test]
    fn profile_elements_are_layered_brightest_wins() {
        use crate::model::{DisplayProfile, Element};

        let mut light = Element::flashlight();
        if let ElementKind::Flashlight { brightness } = &mut light.kind {
            *brightness = 0.2;
        }
        let mut text = Element::text();
        if let ElementKind::Text { mode, .. } = &mut text.kind {
            *mode = TextMode::Static;
        }
        let profile = DisplayProfile::new("Layers", vec![light, text]);
        let config = AppConfig {
            active_profile: Some(profile.id.clone()),
            profiles: vec![profile],
            ..AppConfig::default()
        };
        let leds = first_frame(config);
        // Dim flashlight background everywhere, brighter text drawn over it.
        let background = (255.0_f32 * 0.2) as u8;
        assert!(leds.contains(&background));
        assert!(leds.iter().any(|&led| led > background * 2));
    }
}
