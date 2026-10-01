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
    AppConfig, CycleSettings, DevicePolicy, DisplayProfile, Element, ElementKind, GifLayout, MatrixGeometry,
    ProfileTriggers, TextMode, Trigger,
};
use crate::render::{self, MatrixRenderer};
use crate::{matrix, sensors};

#[derive(Clone, Copy, Debug)]
pub enum EngineCommand {
    Refresh,
    ApplyPolicy,
    Shutdown,
}

/// Whoever wants to hear about configuration changes (the tray).
type Listeners = Arc<Mutex<Vec<mpsc::Sender<()>>>>;

#[derive(Clone)]
pub struct EngineHandle {
    sender: mpsc::Sender<EngineCommand>,
    status: Arc<Mutex<Option<String>>>,
    listeners: Listeners,
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
        let (thread_status, thread_listeners) = (Arc::clone(&status), Arc::clone(&listeners));
        thread::spawn(move || run_engine(config, control, receiver, thread_status, thread_listeners));
        Self { sender, status, listeners }
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

/// Loop wake-up interval when nothing needs sub-200ms redraws.
const IDLE_TICK: Duration = Duration::from_millis(200);

/// With the light show off, how often to look at the lid and power for
/// profile triggers. Without triggers the engine sleeps until a command.
const TRIGGER_POLL: Duration = Duration::from_secs(1);

fn run_engine<C: MatrixControl>(
    config: Arc<Mutex<AppConfig>>,
    control: C,
    receiver: mpsc::Receiver<EngineCommand>,
    status: Arc<Mutex<Option<String>>>,
    listeners: Listeners,
) {
    let geometry = MatrixGeometry::detect();
    let mut last_key = None;
    let mut last_enabled = None;
    // How long to wait for a command before the next frame; `None` sleeps
    // until one arrives (nothing on screen and nothing to watch).
    let mut tick = Some(IDLE_TICK);
    let mut playing: Option<(String, Instant)> = None;
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

    loop {
        let command = match tick {
            Some(timeout) => receiver.recv_timeout(timeout),
            None => receiver.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
        };
        let mut changed = false;
        match command {
            Ok(EngineCommand::Refresh) => {
                last_key = None;
                changed = true;
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
        tick = Some(IDLE_TICK);

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
            // Nothing to draw. Keep an eye on the lid if it is what blanked
            // the panel, on lid and power if triggers want them, otherwise
            // sleep until the next command.
            tick = if blanked {
                Some(IDLE_TICK)
            } else {
                (last_watch != (false, false) || revert.is_some()).then_some(TRIGGER_POLL)
            };
            continue;
        }

        let Some(profile) = snapshot.active_profile().cloned() else {
            set_error(&status, "no active profile selected".into());
            continue;
        };
        // Animations restart whenever another profile becomes active.
        let shown_for = match &playing {
            Some((id, since)) if *id == profile.id => since.elapsed(),
            _ => {
                playing = Some((profile.id.clone(), Instant::now()));
                Duration::ZERO
            }
        };

        let now = Local::now();
        if cycle_cache.len() > 256 {
            cycle_cache.clear();
        }
        let visible = visible_elements(&profile, shown_for, |element| {
            turn_length(element, &profile.cycle, geometry, &mut gif_cache, &mut cycle_cache)
        });
        // One refresh period per element; the frame is redrawn when any changes.
        // The soonest any visible element needs redrawing; `None` if nothing
        // on screen changes by itself.
        let mut wake: Option<Duration> = None;
        let mut periods = Vec::with_capacity(visible.len());
        let mut failed = false;
        for &(element, element_time) in &visible {
            let period = match &element.kind {
                ElementKind::Gif { path, fps, looping, layout, mode, motion_fps, .. } => {
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
                        earliest(&mut wake, animation.frame_interval(*fps));
                    }
                    let frame = animation.index_at(element_time, *fps, *looping) as i64;
                    if *layout == GifLayout::Animate && *mode != TextMode::Static {
                        // Redraw for the GIF's next frame or the next movement step.
                        let fps = motion_fps.max(1.0);
                        earliest(&mut wake, Duration::from_secs_f32(1.0 / fps));
                        let step = (element_time.as_secs_f64() * f64::from(fps)) as i64;
                        (frame << 32) | step
                    } else {
                        frame
                    }
                }
                kind => {
                    if let Some(interval) = frame_interval(kind, now) {
                        earliest(&mut wake, interval);
                    }
                    refresh_period(kind, now, element_time)
                }
            };
            periods.push(period);
        }
        if profile.cycle.enabled && profile.elements.len() > 1 {
            earliest(&mut wake, IDLE_TICK);
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
        let shown: Vec<_> = visible.iter().map(|(element, _)| element.id.as_str()).collect();
        let key = format!("{profile_json}:{shown:?}:{periods:?}");
        if last_key.as_ref() == Some(&key) {
            continue;
        }

        let result = (|| {
            profile.validate()?;
            // Layers combine per LED by taking the brightest value.
            let mut leds = matrix::led_buffer(&RgbImage::new(geometry.width, geometry.height), 1.0, geometry);
            for &(element, element_time) in &visible {
                let layer = match &element.kind {
                    ElementKind::Gif {
                        path, brightness, contrast, black_level, fps, looping, layout, size, overlay_text, overlay_color, overlay_font,
                        overlay_size, overlay_bold, overlay_italic, overlay_outline, ..
                    } => {
                        let animation = &gif_cache[path];
                        let frame = animation.index_at(element_time, *fps, *looping);
                        let mut levels = if *layout == GifLayout::Animate {
                            let sprite = animation.sprite(frame, *size);
                            let image = MatrixRenderer::render_sprite(element, &sprite, element_time, geometry)?;
                            image.pixels().map(|pixel| pixel.0[0]).collect()
                        } else {
                            animation.placed(frame, *layout, geometry)
                        };
                        // Tone shapes the GIF only; brightness below also dims the overlay.
                        crate::animation::apply_tone(&mut levels, *black_level, *contrast);
                        if !overlay_text.is_empty() {
                            let style = render::OverlayStyle {
                                color: *overlay_color,
                                bold: *overlay_bold,
                                italic: *overlay_italic,
                                outline: *overlay_outline,
                            };
                            render::overlay_text(&mut levels, geometry, overlay_text, overlay_font, *overlay_size, style)?;
                        }
                        matrix::level_buffer(&levels, *brightness, geometry)
                    }
                    kind => {
                        let brightness = match kind {
                            ElementKind::Flashlight { brightness } => *brightness,
                            _ => 1.0,
                        };
                        let frame = MatrixRenderer::render(element, now, element_time, geometry)?;
                        matrix::led_buffer(&frame, brightness, geometry)
                    }
                };
                for (led, level) in leds.iter_mut().zip(layer) {
                    *led = (*led).max(level);
                }
            }
            control.write_leds(leds, geometry.model)
        })();
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
    let combined = set(&triggers.lid_closed_plugged_in) || set(&triggers.lid_opened_plugged_in);
    let lid = engine_lid || combined || set(&triggers.lid_closed) || set(&triggers.lid_opened);
    let power = engine_lid && policy.lid_stay_on_when_plugged
        || combined
        || set(&triggers.plugged_in)
        || set(&triggers.unplugged);
    (lid, power)
}

fn notify(listeners: &Listeners) {
    if let Ok(mut listeners) = listeners.lock() {
        listeners.retain(|listener| listener.send(()).is_ok());
    }
}

/// The trigger a change from `before` to `now` fires, skipping ones set to
/// "do nothing". The lid-and-power triggers are the most specific, so they
/// win, then the lid, then power.
fn fired_trigger(triggers: &ProfileTriggers, before: LidState, now: LidState) -> Option<&Trigger> {
    let entered = |closed: bool| now.closed == closed && now.on_mains && !(before.closed == closed && before.on_mains);
    let combined = if entered(true) {
        Some(&triggers.lid_closed_plugged_in)
    } else if entered(false) {
        Some(&triggers.lid_opened_plugged_in)
    } else {
        None
    };
    let lid = match (before.closed, now.closed) {
        (false, true) => Some(&triggers.lid_closed),
        (true, false) => Some(&triggers.lid_opened),
        _ => None,
    };
    let power = match (before.on_mains, now.on_mains) {
        (false, true) => Some(&triggers.plugged_in),
        (true, false) => Some(&triggers.unplugged),
        _ => None,
    };
    [combined, lid, power].into_iter().flatten().find(|trigger| trigger.profile.is_some())
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

/// The elements to draw and how long each has been on screen. Normally all
/// elements, layered, for as long as the profile has been active; with element
/// cycling, one valid element at a time, each for `turn_length` of it.
fn visible_elements<'a>(
    profile: &'a DisplayProfile,
    shown_for: Duration,
    mut turn_length: impl FnMut(&Element) -> Duration,
) -> Vec<(&'a Element, Duration)> {
    if !profile.cycle.enabled || profile.elements.len() < 2 {
        return profile.elements.iter().map(|element| (element, shown_for)).collect();
    }
    let valid: Vec<_> = profile.elements.iter().filter(|element| element.validate().is_ok()).collect();
    // With nothing valid, cycle anyway so the errors are reported.
    let pool: Vec<_> = if valid.is_empty() { profile.elements.iter().collect() } else { valid };
    let turns: Vec<u128> = pool.iter()
        .map(|element| turn_length(element).as_nanos().max(MIN_TURN.as_nanos()))
        .collect();
    let mut into_round = shown_for.as_nanos() % turns.iter().sum::<u128>();
    for (element, turn) in pool.into_iter().zip(turns) {
        if into_round < turn {
            return vec![(element, Duration::from_nanos(into_round as u64))];
        }
        into_round -= turn;
    }
    unreachable!("position is taken modulo the round length")
}

/// Shortest time any element is shown when cycling.
const MIN_TURN: Duration = Duration::from_millis(100);

/// How long an element stays on screen when its profile cycles: the profile's
/// interval, or with "after animation cycles finish" its play count times one
/// animation cycle.
fn turn_length(
    element: &Element,
    cycle: &CycleSettings,
    geometry: MatrixGeometry,
    gif_cache: &mut HashMap<PathBuf, GifAnimation>,
    cycle_cache: &mut HashMap<String, Option<Duration>>,
) -> Duration {
    let interval = Duration::from_secs(cycle.seconds.max(1).into());
    let Some(plays) = element.kind.play_limit().filter(|_| cycle.after_animations) else {
        return interval;
    };
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
                .then(|| MatrixRenderer::sprite_cycle(element, &animation.sprite(0, *size), geometry).ok().flatten())
                .flatten();
            Some(movement.unwrap_or_else(|| animation.cycle_duration(*fps, *looping)))
        }
        _ => MatrixRenderer::text_cycle(element, geometry).ok().flatten(),
    });
    one_play.map_or(interval, |one_play| one_play * plays)
}

/// Changes whenever the frame for a non-GIF profile needs redrawing.
fn refresh_period(kind: &ElementKind, now: DateTime<Local>, elapsed: Duration) -> i64 {
    match kind {
        ElementKind::Clock { show_millis: true, fps, .. } => {
            (now.timestamp_millis() as f64 * f64::from(fps.max(1.0)) / 1000.0) as i64
        }
        ElementKind::Clock { show_seconds: true, .. } => now.timestamp(),
        ElementKind::Clock { .. } => now.timestamp() - i64::from(now.second()),
        ElementKind::Text { mode, fps, .. } if *mode != TextMode::Static => {
            (elapsed.as_secs_f64() * f64::from(fps.max(1.0))) as i64
        }
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

/// How long until a non-GIF element may look different; `None` if it only
/// changes when its settings do.
fn frame_interval(kind: &ElementKind, now: DateTime<Local>) -> Option<Duration> {
    // Just past the boundary, so the new second/minute is already showing.
    const SETTLE: Duration = Duration::from_millis(5);
    let into_second = Duration::from_nanos(now.timestamp_subsec_nanos().into());
    match kind {
        ElementKind::Text { mode, fps, .. } if *mode != TextMode::Static => {
            Some(Duration::from_secs_f64(1.0 / f64::from(fps.max(1.0))))
        }
        ElementKind::Text { .. } | ElementKind::Flashlight { .. } => None,
        ElementKind::Battery { style, .. } if style.is_animated() => Some(Duration::from_secs_f64(1.0 / BATTERY_FPS)),
        ElementKind::Battery { .. } => Some(BATTERY_POLL),
        ElementKind::Clock { show_millis: true, fps, .. } => Some(Duration::from_secs_f64(1.0 / f64::from(fps.max(1.0)))),
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
    fn elements_only_wake_the_engine_when_they_can_change() {
        use chrono::TimeZone;
        use crate::model::Element;

        let now = Local.with_ymd_and_hms(2026, 10, 1, 12, 34, 50).unwrap() + chrono::Duration::milliseconds(250);
        let clock = |seconds| {
            let mut element = Element::clock();
            if let ElementKind::Clock { show_seconds, .. } = &mut element.kind {
                *show_seconds = seconds;
            }
            frame_interval(&element.kind, now).unwrap()
        };
        // 12:34:50.250 -> next minute in 9.75 s, next second in 0.75 s (plus a few ms).
        assert!((Duration::from_millis(9_750)..Duration::from_millis(9_800)).contains(&clock(false)));
        assert!((Duration::from_millis(750)..Duration::from_millis(800)).contains(&clock(true)));

        let mut still = Element::text();
        if let ElementKind::Text { mode, .. } = &mut still.kind {
            *mode = TextMode::Static;
        }
        assert_eq!(frame_interval(&still.kind, now), None);
        assert_eq!(frame_interval(&Element::flashlight().kind, now), None);
        // Animations run at their FPS (5 by default for text).
        assert_eq!(frame_interval(&Element::text().kind, now), Some(Duration::from_millis(200)));
    }

    fn fired<'a>(triggers: &'a ProfileTriggers, before: LidState, now: LidState) -> Option<&'a str> {
        fired_trigger(triggers, before, now).and_then(|trigger| trigger.profile.as_deref())
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
    fn lid_and_power_triggers_win_when_set() {
        let mut triggers = ProfileTriggers {
            plugged_in: Trigger::to("ac"),
            lid_closed: Trigger::to("closed"),
            lid_closed_plugged_in: Trigger::to("docked"),
            lid_opened_plugged_in: Trigger::to("desk"),
            ..ProfileTriggers::default()
        };
        let state = |closed, on_mains| LidState { closed, on_mains };
        // Entered by closing the lid while plugged in, or plugging in with it closed.
        assert_eq!(fired(&triggers, state(false, true), state(true, true)), Some("docked"));
        assert_eq!(fired(&triggers, state(true, false), state(true, true)), Some("docked"));
        assert_eq!(fired(&triggers, state(true, true), state(false, true)), Some("desk"));
        assert_eq!(fired(&triggers, state(false, false), state(false, true)), Some("desk"));
        // Closing on battery is the plain lid trigger.
        assert_eq!(fired(&triggers, state(false, false), state(true, false)), Some("closed"));
        // Left at "do nothing", the plain triggers still apply.
        triggers.lid_opened_plugged_in = Trigger::default();
        assert_eq!(fired(&triggers, state(false, false), state(false, true)), Some("ac"));
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
            *path = "picture.png".into();
        }
        let mut profile = DisplayProfile::new("Cycle", vec![clock.clone(), broken_gif, text.clone()]);
        let shown = |profile: &DisplayProfile, seconds: u64| -> Vec<(String, Duration)> {
            let interval = Duration::from_secs(profile.cycle.seconds.into());
            visible_elements(profile, Duration::from_secs(seconds), |_| interval)
                .into_iter().map(|(element, time)| (element.id.clone(), time)).collect()
        };

        // Without cycling every element is layered for the whole time.
        assert_eq!(shown(&profile, 7).len(), 3);

        profile.cycle.enabled = true;
        profile.cycle.seconds = 5;
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
            let shown = visible_elements(&profile, Duration::from_millis(millis), turn);
            (shown[0].0.id.clone(), shown[0].1)
        };
        assert_eq!(at(1_000), (short.id.clone(), Duration::from_secs(1)));
        assert_eq!(at(3_000), (long.id.clone(), Duration::from_secs(1)));
        assert_eq!(at(7_500), (short.id.clone(), Duration::from_millis(500)));
    }

    #[test]
    fn play_limits_set_the_turn_only_after_animations() {
        use crate::model::Element;

        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let mut text = Element::text();
        if let ElementKind::Text { limit_plays, plays, .. } = &mut text.kind {
            (*limit_plays, *plays) = (true, 3);
        }
        let one_play = MatrixRenderer::text_cycle(&text, geometry).unwrap().unwrap();
        let mut cycle = CycleSettings { enabled: true, seconds: 7, after_animations: false };
        let turn = |cycle: &CycleSettings, element: &Element| {
            turn_length(element, cycle, geometry, &mut HashMap::new(), &mut HashMap::new())
        };
        assert_eq!(turn(&cycle, &text), Duration::from_secs(7));
        cycle.after_animations = true;
        assert_eq!(turn(&cycle, &text), one_play * 3);
        // Elements without a play count keep the interval.
        assert_eq!(turn(&cycle, &Element::clock()), Duration::from_secs(7));
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
