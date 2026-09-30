use std::fs;
use std::path::Path;
use std::time::Duration;

use ab_glyph::{FontArc, PxScale};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local};
use image::{Rgb, RgbImage};
use imageproc::drawing::{draw_filled_rect_mut, draw_hollow_rect_mut, draw_text_mut, text_size};
use imageproc::rect::Rect;

use crate::animation::Sprite;
use crate::model::{
    BatteryStyle, ContentArea, Element, ElementKind, MatrixGeometry, OverlayColor, ScrollDirection, TextMode,
};
use crate::sensors::{self, BatteryState};

const WHITE: Rgb<u8> = Rgb([255, 255, 255]);

pub struct MatrixRenderer;

impl MatrixRenderer {
    pub fn render_to(
        element: &Element,
        now: DateTime<Local>,
        elapsed: Duration,
        geometry: MatrixGeometry,
        destination: &Path,
    ) -> Result<()> {
        let image = Self::render(element, now, elapsed, geometry)?;
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        image.save(destination)
            .with_context(|| format!("failed to save {}", destination.display()))
    }

    /// Draws one element on its own canvas. GIFs are decoded by
    /// [`crate::animation`] instead.
    pub fn render(
        element: &Element,
        now: DateTime<Local>,
        elapsed: Duration,
        geometry: MatrixGeometry,
    ) -> Result<RgbImage> {
        element.validate()?;
        let image = match &element.kind {
            ElementKind::Clock {
                font, font_size, use_24_hour, show_seconds, show_date, date_format, show_millis,
                ignore_safe_area, ..
            } => render_clock(
                font, *font_size, *use_24_hour, *show_seconds, *show_millis, *show_date, date_format,
                now, geometry, layout_area(geometry, *ignore_safe_area),
            )?,
            ElementKind::Text {
                text, font, font_size, mode, speed, ignore_safe_area, scroll_pause, direction, period, ..
            } => {
                let area = layout_area(geometry, *ignore_safe_area);
                let motion = Motion { speed: *speed, pause: *scroll_pause, period: *period, direction: *direction };
                let font = load_font(font)?;
                let glyphs = Glyphs::text(&font, text, *font_size, *mode, *direction, area);
                animate(&glyphs, *mode, motion, elapsed, geometry, area)
            }
            ElementKind::Gif { .. } => bail!("GIF elements are decoded, not rendered"),
            ElementKind::Flashlight { .. } => {
                RgbImage::from_pixel(geometry.width, geometry.height, WHITE)
            }
            ElementKind::Battery { font, font_size, style, label, ignore_safe_area, .. } => {
                let area = layout_area(geometry, *ignore_safe_area);
                render_battery(font, *font_size, *style, label, sensors::battery(), elapsed, geometry, area)?
            }
        };
        let y_offset = match &element.kind {
            ElementKind::Clock { y_offset, .. }
            | ElementKind::Text { y_offset, .. }
            | ElementKind::Battery { y_offset, .. } => *y_offset,
            ElementKind::Gif { .. } | ElementKind::Flashlight { .. } => 0,
        };
        Ok(shift_down(image, y_offset))
    }
}

/// The whole canvas, or only the part every panel row is guaranteed to show.
fn layout_area(geometry: MatrixGeometry, ignore_safe_area: bool) -> ContentArea {
    if ignore_safe_area { geometry.full_area() } else { geometry.safe_content_area() }
}

/// Moves the whole frame `dy` pixels down (up when negative); rows pushed
/// past the edge are dropped.
fn shift_down(image: RgbImage, dy: i32) -> RgbImage {
    if dy == 0 {
        return image;
    }
    let mut shifted = RgbImage::new(image.width(), image.height());
    for (x, y, pixel) in image.enumerate_pixels() {
        let target = y as i32 + dy;
        if (0..image.height() as i32).contains(&target) {
            shifted.put_pixel(x, target as u32, *pixel);
        }
    }
    shifted
}

#[allow(clippy::too_many_arguments)]
fn render_clock(
    font_path: &Path,
    font_size: f32,
    use_24_hour: bool,
    show_seconds: bool,
    show_millis: bool,
    show_date: bool,
    date_format: &str,
    now: DateTime<Local>,
    geometry: MatrixGeometry,
    area: ContentArea,
) -> Result<RgbImage> {
    let font = load_font(font_path)?;
    let format = match (use_24_hour, show_seconds, show_millis) {
        (true, _, true) => "%H:%M:%S%.3f",
        (true, true, false) => "%H:%M:%S",
        (true, false, false) => "%H:%M",
        (false, _, true) => "%I:%M:%S%.3f %p",
        (false, true, false) => "%I:%M:%S %p",
        (false, false, false) => "%I:%M %p",
    };
    let time = now.format(format).to_string();
    let mut image = canvas(geometry);
    if show_date {
        let date = now.format(date_format).to_string();
        let (time_scale, date_scale) = fit_clock_lines(&font, &time, &date, font_size, area);
        let (_, time_height) = text_size(time_scale, &font, &time);
        let (_, date_height) = text_size(date_scale, &font, &date);
        let total_height = time_height + date_height + 1;
        let top = area.y + area.height.saturating_sub(total_height) / 2;
        draw_centered(&mut image, &font, time_scale, &time, top as i32, area);
        draw_centered(
            &mut image,
            &font,
            date_scale,
            &date,
            (top + time_height + 1) as i32,
            area,
        );
    } else {
        let scale = fit_scale(&font, &time, font_size, area.width);
        let (_, height) = text_size(scale, &font, &time);
        draw_centered(
            &mut image,
            &font,
            scale,
            &time,
            (area.y + area.height.saturating_sub(height) / 2) as i32,
            area,
        );
    }
    Ok(image)
}

/// What an animated element moves around: a text string, or a GIF frame
/// standing in for a single character.
enum Glyphs<'a> {
    Text { font: &'a FontArc, scale: PxScale, text: &'a str, width: i32, height: i32 },
    Sprite(&'a Sprite),
}

impl<'a> Glyphs<'a> {
    /// Lays out `text` like every text animation does: horizontal movement
    /// may exceed the area width, everything else is shrunk to fit it.
    fn text(
        font: &'a FontArc,
        text: &'a str,
        font_size: f32,
        mode: TextMode,
        direction: ScrollDirection,
        area: ContentArea,
    ) -> Self {
        let scale = match mode {
            TextMode::Scroll | TextMode::Bounce if !direction.is_vertical() => {
                PxScale::from(font_size.min(area.height as f32))
            }
            _ => fit_scale(font, text, font_size, area.width),
        };
        let (width, height) = text_size(scale, font, text);
        Self::Text { font, scale, text, width: width as i32, height: height as i32 }
    }

    fn size(&self) -> (i32, i32) {
        match self {
            Self::Text { width, height, .. } => (*width, *height),
            Self::Sprite(sprite) => (sprite.width as i32, sprite.height as i32),
        }
    }

    /// Units typed by typewriter and lifted by wave: characters of text, or
    /// pixel columns of a sprite (it wipes in and ripples).
    fn count(&self) -> usize {
        match self {
            Self::Text { text, .. } => text.chars().count(),
            Self::Sprite(sprite) => sprite.width as usize,
        }
    }

    /// Wave phase difference between neighbouring units.
    fn wave_step(&self) -> f32 {
        match self {
            Self::Text { .. } => 0.7,
            Self::Sprite(_) => 0.25,
        }
    }

    /// Draws the first `shown` characters at (x, y) at brightness `level`,
    /// each lifted by `lift(index)` pixels when given (wave).
    fn draw(&self, image: &mut RgbImage, x: i32, y: i32, level: u8, shown: usize, lift: Option<&dyn Fn(usize) -> i32>) {
        match self {
            Self::Text { font, scale, text, .. } => {
                let color = Rgb([level; 3]);
                let Some(lift) = lift else {
                    // Whole string at once keeps the font's kerning.
                    let prefix: String = text.chars().take(shown).collect();
                    if !prefix.is_empty() {
                        draw_text_mut(image, color, x, y, *scale, *font, &prefix);
                    }
                    return;
                };
                for (index, (offset, character)) in text.char_indices().take(shown).enumerate() {
                    let (advance, _) = text_size(*scale, *font, &text[..offset]);
                    let glyph = character.to_string();
                    draw_text_mut(image, color, x + advance as i32, y + lift(index), *scale, *font, &glyph);
                }
            }
            Self::Sprite(sprite) => {
                for (row, pixels) in sprite.levels.chunks(sprite.width as usize).enumerate() {
                    for (column, &value) in pixels.iter().enumerate().take(shown) {
                        let lifted = y + lift.map_or(0, |lift| lift(column));
                        let (px, py) = (x + column as i32, lifted + row as i32);
                        let value = (u32::from(value) * u32::from(level) / 255) as u8;
                        if value > 0 && px >= 0 && py >= 0 && (px as u32) < image.width() && (py as u32) < image.height() {
                            image.put_pixel(px as u32, py as u32, Rgb([value; 3]));
                        }
                    }
                }
            }
        }
    }
}

/// Seconds for one scroll pass plus its pause.
fn scroll_cycle(span: i32, size: i32, motion: Motion) -> f32 {
    (span + size) as f32 / motion.speed + motion.pause
}

/// Where a bounce starts and how far it can travel along its axis.
fn bounce_room(area: ContentArea, w: i32, h: i32, vertical: bool) -> (i32, i32) {
    if vertical {
        (area.y as i32, area.height as i32 - h)
    } else {
        (area.x as i32, area.width as i32 - w)
    }
}

fn typewriter_cycle(count: usize, motion: Motion) -> f32 {
    count as f32 * motion.period + motion.pause
}

/// How long one full play of `animate` takes; `None` for static.
fn motion_cycle(glyphs: &Glyphs, mode: TextMode, motion: Motion, geometry: MatrixGeometry, area: ContentArea) -> Option<Duration> {
    let (w, h) = glyphs.size();
    let vertical = motion.direction.is_vertical();
    let seconds = match mode {
        TextMode::Static => return None,
        TextMode::Scroll => {
            let (span, size) = if vertical { (geometry.height as i32, h) } else { (geometry.width as i32, w) };
            scroll_cycle(span, size, motion)
        }
        TextMode::Bounce => {
            let (_, room) = bounce_room(area, w, h, vertical);
            2.0 * room.abs().max(1) as f32 / motion.speed
        }
        TextMode::Blink | TextMode::Pulse | TextMode::Wave => motion.period,
        TextMode::Typewriter => typewriter_cycle(glyphs.count(), motion),
    };
    Some(Duration::from_secs_f32(seconds.max(0.05)))
}

impl MatrixRenderer {
    /// How long one full play of a text element's animation takes, matching
    /// `render`; `None` for static text and other element kinds.
    pub fn text_cycle(element: &Element, geometry: MatrixGeometry) -> Result<Option<Duration>> {
        let ElementKind::Text {
            text, font, font_size, mode, speed, ignore_safe_area, scroll_pause, direction, period, ..
        } = &element.kind
        else {
            return Ok(None);
        };
        let motion = Motion { speed: *speed, pause: *scroll_pause, period: *period, direction: *direction };
        let area = layout_area(geometry, *ignore_safe_area);
        let font = load_font(font)?;
        let glyphs = Glyphs::text(&font, text, *font_size, *mode, *direction, area);
        Ok(motion_cycle(&glyphs, *mode, motion, geometry, area))
    }

    /// Draws an animated GIF element's current `sprite` moving like text.
    pub fn render_sprite(element: &Element, sprite: &Sprite, elapsed: Duration, geometry: MatrixGeometry) -> Result<RgbImage> {
        let (mode, motion, area, y_offset) = sprite_motion(element, geometry)?;
        let image = animate(&Glyphs::Sprite(sprite), mode, motion, elapsed, geometry, area);
        Ok(shift_down(image, y_offset))
    }

    /// One full play of an animated GIF element's movement (not its frames);
    /// `None` when it does not move.
    pub fn sprite_cycle(element: &Element, sprite: &Sprite, geometry: MatrixGeometry) -> Result<Option<Duration>> {
        let (mode, motion, area, _) = sprite_motion(element, geometry)?;
        Ok(motion_cycle(&Glyphs::Sprite(sprite), mode, motion, geometry, area))
    }
}

fn sprite_motion(element: &Element, geometry: MatrixGeometry) -> Result<(TextMode, Motion, ContentArea, i32)> {
    let ElementKind::Gif { mode, direction, speed, period, scroll_pause, ignore_safe_area, y_offset, .. } = &element.kind
    else {
        bail!("only GIF elements are drawn as sprites");
    };
    element.validate()?;
    let motion = Motion { speed: *speed, pause: *scroll_pause, period: *period, direction: *direction };
    Ok((*mode, motion, layout_area(geometry, *ignore_safe_area), *y_offset))
}

#[derive(Clone, Copy)]
struct Motion {
    /// Pixels per second for scroll and bounce.
    speed: f32,
    /// Seconds between scroll passes / typewriter hold.
    pause: f32,
    /// Seconds per blink, pulse and wave cycle, or per typed character.
    period: f32,
    direction: ScrollDirection,
}

/// Positions `glyphs` for `mode` at time `elapsed` and draws them.
fn animate(
    glyphs: &Glyphs,
    mode: TextMode,
    motion: Motion,
    elapsed: Duration,
    geometry: MatrixGeometry,
    area: ContentArea,
) -> RgbImage {
    let t = elapsed.as_secs_f32();
    let vertical = motion.direction.is_vertical();
    let (w, h) = glyphs.size();
    let mut x = area.x as i32 + (area.width as i32 - w) / 2;
    let mut y = area.y as i32 + (area.height as i32 - h) / 2;
    let mut image = canvas(geometry);
    let mut level = 255;
    let mut shown = glyphs.count();
    let mut amplitude = None;

    match mode {
        TextMode::Static => {}
        TextMode::Scroll => {
            // Drawing is not clipped to the area, so a pass runs across the
            // whole canvas: enter at one edge, leave past the opposite one,
            // then the pause keeps it off screen.
            let (span, size) = if vertical { (geometry.height as i32, h) } else { (geometry.width as i32, w) };
            let travel = (span + size) as f32;
            let cycle = scroll_cycle(span, size, motion);
            let offset = ((t % cycle) * motion.speed).min(travel) as i32;
            match motion.direction {
                ScrollDirection::Left => x = span - offset,
                ScrollDirection::Right => x = offset - size,
                ScrollDirection::Up => y = span - offset,
                ScrollDirection::Down => y = offset - size,
            }
        }
        TextMode::Bounce => {
            // Ping-pong between the area edges; content larger than the area
            // swings so each end comes into view.
            let (start, room) = bounce_room(area, w, h, vertical);
            let range = room.abs().max(1) as f32;
            let phase = (t * motion.speed) % (2.0 * range);
            let along = if phase < range { phase } else { 2.0 * range - phase };
            // Left/Up start at the far end so the first move matches the name.
            let along = match motion.direction {
                ScrollDirection::Left | ScrollDirection::Up => range - along,
                ScrollDirection::Right | ScrollDirection::Down => along,
            } as i32;
            let position = start + room.min(0) + along;
            if vertical { y = position } else { x = position }
        }
        TextMode::Blink => {
            if t % motion.period >= motion.period / 2.0 {
                shown = 0;
            }
        }
        TextMode::Pulse => {
            let phase = (t % motion.period) / motion.period;
            level = (255.0 * (1.0 - (2.0 * phase - 1.0).abs())) as u8;
        }
        TextMode::Typewriter => {
            // Positioned for the full content so typed characters never shift.
            let cycle = typewriter_cycle(shown, motion);
            shown = (((t % cycle) / motion.period) as usize + 1).min(shown);
        }
        TextMode::Wave => {
            amplitude = Some(((area.height as i32 - h) / 2).clamp(1, 3) as f32);
        }
    }
    let base = t * std::f32::consts::TAU / motion.period;
    let step = glyphs.wave_step();
    let wave = amplitude.map(|amplitude| {
        move |index: usize| (amplitude * (base - index as f32 * step).sin()).round() as i32
    });
    glyphs.draw(&mut image, x, y, level, shown, wave.as_ref().map(|wave| wave as &dyn Fn(usize) -> i32));
    image
}

/// A battery gauge in `style`, with an optional `label` line underneath,
/// centred in `area`. `elapsed` drives the animated styles.
#[allow(clippy::too_many_arguments)]
fn render_battery(
    font_path: &Path,
    font_size: f32,
    style: BatteryStyle,
    label: &str,
    state: Option<BatteryState>,
    elapsed: Duration,
    geometry: MatrixGeometry,
    area: ContentArea,
) -> Result<RgbImage> {
    let font = load_font(font_path)?;
    let mut image = canvas(geometry);
    let mut gauge = area;
    if !label.is_empty() {
        let scale = fit_scale(&font, label, font_size * 0.8, area.width);
        let (_, label_h) = text_size(scale, &font, label);
        let top = area.y + area.height.saturating_sub(label_h);
        draw_centered(&mut image, &font, scale, label, top as i32, area);
        gauge.height = area.height.saturating_sub(label_h + 1).max(4);
    }
    let t = elapsed.as_secs_f32();
    match style {
        BatteryStyle::Classic => draw_classic(&mut image, &font, font_size, state, gauge),
        BatteryStyle::Liquid => draw_liquid(&mut image, &font, font_size, state, gauge, t),
        BatteryStyle::Segments => draw_segments(&mut image, &font, font_size, state, gauge, t),
        BatteryStyle::Ring => draw_ring(&mut image, &font, font_size, state, gauge, t),
        BatteryStyle::Big => draw_big(&mut image, &font, font_size, state, gauge, t),
    }
    Ok(image)
}

fn percent_text(state: Option<BatteryState>) -> String {
    state.map_or_else(|| "--%".to_owned(), |state| format!("{}%", state.percent))
}

fn percent(state: Option<BatteryState>) -> u32 {
    state.map_or(0, |state| u32::from(state.percent))
}

fn charging(state: Option<BatteryState>) -> bool {
    state.is_some_and(|state| state.charging)
}

/// Sets a pixel if it is on the canvas.
fn plot(image: &mut RgbImage, x: i32, y: i32, level: u8) {
    if x >= 0 && y >= 0 && (x as u32) < image.width() && (y as u32) < image.height() {
        image.put_pixel(x as u32, y as u32, Rgb([level; 3]));
    }
}

fn fill_rect(image: &mut RgbImage, x: i32, y: i32, w: u32, h: u32, level: u8) {
    if w > 0 && h > 0 {
        draw_filled_rect_mut(image, Rect::at(x, y).of_size(w, h), Rgb([level; 3]));
    }
}

/// Draws `icon_w` pixels of gauge on the left of `area` via `draw_icon`, then
/// the percentage to its right; both vertically centred as a group.
fn icon_and_percent(
    image: &mut RgbImage,
    font: &FontArc,
    font_size: f32,
    state: Option<BatteryState>,
    area: ContentArea,
    icon_w: u32,
    draw_icon: impl FnOnce(&mut RgbImage, i32),
) {
    const GAP: u32 = 2;
    let text = percent_text(state);
    let scale = fit_scale(font, &text, font_size.min(area.height as f32), area.width.saturating_sub(icon_w + GAP));
    let (text_w, text_h) = text_size(scale, font, &text);
    let left = area.x + area.width.saturating_sub(icon_w + GAP + text_w) / 2;
    draw_icon(image, left as i32);
    let text_y = area.y + area.height.saturating_sub(text_h) / 2;
    draw_text_mut(image, WHITE, (left + icon_w + GAP) as i32, text_y as i32, scale, font, &text);
}

/// Inverts the bolt glyph centred at (cx, cy).
fn draw_bolt(image: &mut RgbImage, cx: i32, cy: i32) {
    let (left, top) = (cx - BOLT[0].len() as i32 / 2, cy - BOLT.len() as i32 / 2);
    for (dy, row) in BOLT.iter().enumerate() {
        for (dx, cell) in row.bytes().enumerate() {
            let (x, y) = (left + dx as i32, top + dy as i32);
            if cell == b'#' && x >= 0 && y >= 0 && (x as u32) < image.width() && (y as u32) < image.height() {
                let pixel = image.get_pixel_mut(x as u32, y as u32);
                pixel.0 = pixel.0.map(|channel| 255 - channel);
            }
        }
    }
}

/// Outline, tip, fill and bolt, followed by the percentage.
fn draw_classic(image: &mut RgbImage, font: &FontArc, font_size: f32, state: Option<BatteryState>, area: ContentArea) {
    const BODY_W: u32 = 16;
    const TIP_W: u32 = 2;
    let body_h = area.height.min(11);
    let top = (area.y + area.height.saturating_sub(body_h) / 2) as i32;
    icon_and_percent(image, font, font_size, state, area, BODY_W + TIP_W, |image, left| {
        draw_hollow_rect_mut(image, Rect::at(left, top).of_size(BODY_W, body_h), WHITE);
        fill_rect(image, left + BODY_W as i32, top + body_h as i32 / 4 + 1, TIP_W, body_h.saturating_sub(body_h / 2 + 2).max(1), 255);
        // Inner fill area leaves a one pixel gap inside the outline.
        let (inner_w, inner_h) = (BODY_W - 4, body_h.saturating_sub(4));
        fill_rect(image, left + 2, top + 2, (inner_w * percent(state)).div_ceil(100), inner_h, 255);
        if charging(state) {
            draw_bolt(image, left + 2 + inner_w as i32 / 2, top + 2 + inner_h as i32 / 2);
        }
    });
}

/// Upright battery filled with liquid whose surface sloshes; bubbles rise
/// while charging.
fn draw_liquid(image: &mut RgbImage, font: &FontArc, font_size: f32, state: Option<BatteryState>, area: ContentArea, t: f32) {
    let body_h = area.height.clamp(6, 22);
    let body_w = (body_h * 3 / 5).max(6);
    let top = (area.y + area.height.saturating_sub(body_h) / 2) as i32 + 1;
    icon_and_percent(image, font, font_size, state, area, body_w, |image, left| {
        fill_rect(image, left + body_w as i32 / 2 - 2, top - 1, 4, 1, 255);
        draw_hollow_rect_mut(image, Rect::at(left, top).of_size(body_w, body_h - 1), WHITE);
        let (inner_x, inner_top, inner_w, inner_h) = (left + 2, top + 2, body_w as i32 - 4, body_h as i32 - 5);
        let level = inner_h as f32 * percent(state) as f32 / 100.0;
        let bottom = inner_top + inner_h;
        for dx in 0..inner_w {
            // Two travelling waves make the surface look alive.
            let wave = (t * 3.0 + dx as f32 * 0.9).sin() * 0.8 + (t * 1.7 - dx as f32 * 0.5).sin() * 0.5;
            let surface = (bottom as f32 - level + wave).round() as i32;
            for y in surface.max(inner_top)..bottom {
                plot(image, inner_x + dx, y, 255);
            }
        }
        if charging(state) && inner_w > 2 {
            // Bubbles: dark pixels rising through the liquid at their own pace.
            for bubble in 0..3 {
                let speed = 4.0 + bubble as f32 * 1.5;
                let rise = (t * speed + bubble as f32 * 3.7) % level.max(1.0);
                let x = inner_x + 1 + (bubble * 2 + (t * 0.7) as i32) % (inner_w - 2).max(1);
                plot(image, x, bottom - 1 - rise as i32, 0);
            }
        }
    });
}

/// Five cells; the empty ones fill one by one while charging, and the last
/// cell blinks when the charge is low.
fn draw_segments(image: &mut RgbImage, font: &FontArc, font_size: f32, state: Option<BatteryState>, area: ContentArea, t: f32) {
    const CELLS: u32 = 5;
    const CELL_W: u32 = 2;
    let body_w = CELLS * (CELL_W + 1) + 3;
    let body_h = area.height.min(11);
    let top = (area.y + area.height.saturating_sub(body_h) / 2) as i32;
    let lit = percent(state).div_ceil(100 / CELLS).min(CELLS);
    icon_and_percent(image, font, font_size, state, area, body_w + 2, |image, left| {
        draw_hollow_rect_mut(image, Rect::at(left, top).of_size(body_w, body_h), WHITE);
        fill_rect(image, left + body_w as i32, top + body_h as i32 / 4 + 1, 2, body_h.saturating_sub(body_h / 2 + 2).max(1), 255);
        let shown = if charging(state) {
            // Chaser: empty cells light up one after another, then restart.
            lit + (t * 2.5) as u32 % (CELLS - lit + 1)
        } else if lit <= 1 && t % 1.0 >= 0.5 {
            0
        } else {
            lit
        };
        for cell in 0..shown.min(CELLS) {
            fill_rect(image, left + 2 + (cell * (CELL_W + 1)) as i32, top + 2, CELL_W, body_h.saturating_sub(4), 255);
        }
    });
}

/// A circular gauge filled clockwise from the top, with a dim track; a
/// comet orbits the ring while charging.
fn draw_ring(image: &mut RgbImage, font: &FontArc, font_size: f32, state: Option<BatteryState>, area: ContentArea, t: f32) {
    let radius = (area.height.min(area.width / 2) as i32 / 2 - 1).max(3);
    let size = (radius * 2 + 1) as u32;
    let cy = (area.y + area.height / 2) as i32;
    let filled = percent(state) as f32 / 100.0;
    icon_and_percent(image, font, font_size, state, area, size, |image, left| {
        let cx = left + radius;
        let steps = (radius * 16).max(64);
        for step in 0..steps {
            let turn = step as f32 / steps as f32;
            let angle = turn * std::f32::consts::TAU;
            let (x, y) = (cx + (radius as f32 * angle.sin()).round() as i32, cy - (radius as f32 * angle.cos()).round() as i32);
            plot(image, x, y, if turn <= filled { 255 } else { 50 });
        }
        if charging(state) {
            let head = (t * 0.6).fract();
            for tail in 0..6 {
                let angle = (head - tail as f32 * 0.02) * std::f32::consts::TAU;
                let inner = radius as f32 - 2.0;
                let (x, y) = (cx + (inner * angle.sin()).round() as i32, cy - (inner * angle.cos()).round() as i32);
                plot(image, x, y, 255 - tail * 40);
            }
        }
    });
}

/// The percentage in large digits, lit up to the charge level with a wavy
/// tide line; the rest stays dim.
fn draw_big(image: &mut RgbImage, font: &FontArc, font_size: f32, state: Option<BatteryState>, area: ContentArea, t: f32) {
    let text = percent_text(state);
    let scale = fit_scale(font, &text, (area.height as f32 * 1.3).max(font_size), area.width);
    let (text_w, text_h) = text_size(scale, font, &text);
    let mut digits = RgbImage::new(image.width(), image.height());
    let (x, y) = (area.x as i32 + (area.width as i32 - text_w as i32) / 2, area.y as i32 + (area.height as i32 - text_h as i32) / 2);
    draw_text_mut(&mut digits, WHITE, x, y, scale, font, &text);
    let tide = y as f32 + text_h as f32 * (1.0 - percent(state) as f32 / 100.0);
    let rise = if charging(state) { (t * 2.0).sin() * 1.5 } else { 0.0 };
    for (px, py, pixel) in digits.enumerate_pixels() {
        if pixel.0[0] == 0 {
            continue;
        }
        let line = tide + rise + (t * 4.0 + px as f32 * 0.6).sin();
        let level = if py as f32 >= line { pixel.0[0] } else { pixel.0[0] / 5 };
        image.put_pixel(px, py, Rgb([level; 3]));
    }
}

/// How overlay text is drawn.
#[derive(Clone, Copy, Debug)]
pub struct OverlayStyle {
    pub color: OverlayColor,
    pub bold: bool,
    pub italic: bool,
    /// Width in pixels of a border in the opposite color; 0 for none.
    pub outline: u32,
}

/// Draws `text` over row-major LED `levels`, centred in the safe area:
/// white lights LEDs up, black cuts them out. An outline in the opposite
/// color is drawn first so the text stands out from busy GIFs.
pub fn overlay_text(
    levels: &mut [u8],
    geometry: MatrixGeometry,
    text: &str,
    font_path: &Path,
    font_size: f32,
    style: OverlayStyle,
) -> Result<()> {
    let (font, fake_bold, fake_italic) = styled_font(font_path, style.bold, style.italic)?;
    let area = geometry.safe_content_area();
    let scale = fit_scale(&font, text, font_size, area.width);
    let (_, height) = text_size(scale, &font, text);
    let mut mask = canvas(geometry);
    draw_centered(&mut mask, &font, scale, text, (area.y + area.height.saturating_sub(height) / 2) as i32, area);
    let mut mask: Vec<u8> = mask.pixels().map(|pixel| pixel.0[0]).collect();
    let (width, rows) = (geometry.width as usize, geometry.height as usize);
    if fake_bold {
        // Drawn again one pixel to the right.
        for row in mask.chunks_mut(width) {
            for x in (1..width).rev() {
                row[x] = row[x].max(row[x - 1]);
            }
        }
    }
    if fake_italic {
        // Rows above the middle lean right, rows below lean left.
        let middle = rows as i32 / 2;
        for (y, row) in mask.chunks_mut(width).enumerate() {
            let shift = ((middle - y as i32) as f32 * 0.25).round() as i32;
            let original = row.to_vec();
            for (x, value) in row.iter_mut().enumerate() {
                let source = x as i32 - shift;
                *value = if (0..width as i32).contains(&source) { original[source as usize] } else { 0 };
            }
        }
    }

    let (outline_on, text_on): (fn(u8, u8) -> u8, fn(u8, u8) -> u8) = match style.color {
        OverlayColor::White => (cut_out, light_up),
        OverlayColor::Black => (light_up, cut_out),
    };
    if style.outline > 0 {
        let ring = dilate(&mask, width, rows, style.outline);
        for (level, coverage) in levels.iter_mut().zip(ring) {
            // Solid, even where small anti-aliased strokes never reach full
            // coverage; a faint border would not help readability.
            *level = outline_on(*level, coverage.saturating_mul(3));
        }
    }
    for (level, &coverage) in levels.iter_mut().zip(&mask) {
        *level = text_on(*level, coverage);
    }
    Ok(())
}

fn light_up(level: u8, coverage: u8) -> u8 {
    level.max(coverage)
}

fn cut_out(level: u8, coverage: u8) -> u8 {
    (u32::from(level) * (255 - u32::from(coverage)) / 255) as u8
}

/// Grows a coverage mask by `radius` pixels (round brush): each pixel takes
/// the strongest coverage within that distance.
fn dilate(mask: &[u8], width: usize, rows: usize, radius: u32) -> Vec<u8> {
    let radius = radius as i32;
    let offsets: Vec<(i32, i32)> = (-radius..=radius)
        .flat_map(|dy| (-radius..=radius).map(move |dx| (dx, dy)))
        .filter(|(dx, dy)| dx * dx + dy * dy <= radius * radius + radius)
        .collect();
    let mut grown = vec![0u8; mask.len()];
    for y in 0..rows as i32 {
        for x in 0..width as i32 {
            grown[y as usize * width + x as usize] = offsets.iter()
                .filter_map(|(dx, dy)| {
                    let (sx, sy) = (x + dx, y + dy);
                    ((0..width as i32).contains(&sx) && (0..rows as i32).contains(&sy))
                        .then(|| mask[sy as usize * width + sx as usize])
                })
                .max()
                .unwrap_or(0);
        }
    }
    grown
}

/// The bold/italic variant of a font file when one sits next to it (e.g.
/// `DejaVuSans-Bold.ttf`, `DejaVuSerif-Italic.ttf`), with flags for the
/// styles that were not found and must be faked.
fn styled_font(path: &Path, bold: bool, italic: bool) -> Result<(FontArc, bool, bool)> {
    let variant = font_variant(path, bold, italic).or_else(|| {
        // Settle for one real style and fake the other.
        (bold && italic).then(|| font_variant(path, true, false).or_else(|| font_variant(path, false, true))).flatten()
    });
    let (file, real_bold, real_italic) = variant.unwrap_or((path.to_path_buf(), false, false));
    Ok((load_font(&file)?, bold && !real_bold, italic && !real_italic))
}

/// Looks for `<name>-Bold`, `-Italic`/`-Oblique`, or `-BoldItalic`/
/// `-BoldOblique` next to `path`, whatever the capitalization of the files
/// on disk (`DejaVuSans-Bold.ttf`, `dejavusans-bold.ttf`, `Foo-BOLD.TTF`);
/// returns it with the styles it provides.
fn font_variant(path: &Path, bold: bool, italic: bool) -> Option<(std::path::PathBuf, bool, bool)> {
    if !bold && !italic {
        return None;
    }
    let stem = path.file_stem()?.to_str()?.to_lowercase();
    let base = stem.strip_suffix("-regular").unwrap_or(&stem);
    let extension = path.extension().and_then(|value| value.to_str()).unwrap_or("ttf").to_lowercase();
    let suffixes: &[&str] = match (bold, italic) {
        (true, true) => &["bolditalic", "boldoblique"],
        (true, false) => &["bold"],
        _ => &["italic", "oblique"],
    };
    let mut siblings: Vec<_> = fs::read_dir(path.parent()?).ok()?.flatten().map(|entry| entry.path()).collect();
    siblings.sort();
    suffixes.iter().find_map(|suffix| {
        let wanted = format!("{base}-{suffix}.{extension}");
        siblings.iter().find(|candidate| {
            candidate.is_file()
                && candidate.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.to_lowercase() == wanted)
        })
    })
    .map(|file| (file.clone(), bold, italic))
}

/// Charging bolt, drawn inverted over the battery fill.
const BOLT: [&str; 7] = ["...##", "..##.", ".##..", "#####", "..##.", ".##..", "##..."];

fn canvas(geometry: MatrixGeometry) -> RgbImage {
    RgbImage::from_pixel(geometry.width, geometry.height, Rgb([0, 0, 0]))
}

/// Loads a font once per path; frames are redrawn many times per second.
fn load_font(path: &Path) -> Result<FontArc> {
    thread_local! {
        static FONTS: std::cell::RefCell<std::collections::HashMap<std::path::PathBuf, FontArc>> =
            Default::default();
    }
    if let Some(font) = FONTS.with(|fonts| fonts.borrow().get(path).cloned()) {
        return Ok(font);
    }
    let bytes = fs::read(path).with_context(|| format!("failed to read font {}", path.display()))?;
    let font = FontArc::try_from_vec(bytes).context("font is not a supported TrueType/OpenType font")?;
    FONTS.with(|fonts| fonts.borrow_mut().insert(path.to_path_buf(), font.clone()));
    Ok(font)
}

fn fit_scale(font: &FontArc, text: &str, requested: f32, max_width: u32) -> PxScale {
    let mut size = requested;
    loop {
        let scale = PxScale::from(size);
        if text_size(scale, font, text).0 <= max_width || size <= 6.0 {
            return scale;
        }
        size -= 0.5;
    }
}

fn fit_clock_lines(
    font: &FontArc,
    time: &str,
    date: &str,
    requested: f32,
    area: ContentArea,
) -> (PxScale, PxScale) {
    let mut time_size = requested;
    loop {
        let time_scale = fit_scale(font, time, time_size, area.width);
        let date_scale = fit_scale(font, date, (time_size * 0.55).max(6.0), area.width);
        let (_, time_height) = text_size(time_scale, font, time);
        let (_, date_height) = text_size(date_scale, font, date);
        if time_height + date_height + 1 <= area.height || time_size <= 6.0 {
            return (time_scale, date_scale);
        }
        time_size -= 0.5;
    }
}

fn draw_centered(
    image: &mut RgbImage,
    font: &FontArc,
    scale: PxScale,
    text: &str,
    y: i32,
    area: ContentArea,
) {
    let (width, _) = text_size(scale, font, text);
    let x = area.x as i32 + (area.width as i32 - width as i32) / 2;
    draw_text_mut(image, Rgb([255, 255, 255]), x, y, scale, font, text);
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use tempfile::tempdir;
    use super::*;

    #[test]
    fn clock_renders_matrix_sized_png() {
        let directory = tempdir().unwrap();
        let output = directory.path().join("clock.png");
        let profile = Element::clock();
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let now = Local.with_ymd_and_hms(2026, 9, 30, 12, 34, 56).unwrap();
        MatrixRenderer::render_to(&profile, now, Duration::ZERO, geometry, &output).unwrap();
        let image = image::open(output).unwrap();
        assert_eq!((image.width(), image.height()), (74, 39));
    }

    #[test]
    fn scrolling_text_changes_over_time() {
        let profile = Element::text();
        let directory = tempdir().unwrap();
        let first = directory.path().join("first.png");
        let second = directory.path().join("second.png");
        let now = Local::now();
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        MatrixRenderer::render_to(&profile, now, Duration::ZERO, geometry, &first).unwrap();
        MatrixRenderer::render_to(
            &profile,
            now,
            Duration::from_secs(1),
            geometry,
            &second,
        )
        .unwrap();
        assert_ne!(fs::read(first).unwrap(), fs::read(second).unwrap());
    }

    #[test]
    fn scroll_pause_blanks_between_passes() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let mut profile = Element::text();
        if let ElementKind::Text { text, speed, scroll_pause, .. } = &mut profile.kind {
            *text = "Hi".into();
            *speed = 100.0;
            *scroll_pause = 5.0;
        }
        let lit = |seconds: f32| {
            MatrixRenderer::render(&profile, Local::now(), Duration::from_secs_f32(seconds), geometry)
                .unwrap().pixels().any(|pixel| pixel.0 != [0, 0, 0])
        };
        // A pass over the 74px canvas takes about a second at 100px/s.
        assert!(lit(0.3));
        assert!(!lit(3.0));
    }

    fn text_frame(mode: TextMode, direction: ScrollDirection, seconds: f32) -> RgbImage {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let mut profile = Element::text();
        if let ElementKind::Text { text, mode: m, direction: d, speed, period, .. } = &mut profile.kind {
            *text = "Animate".into();
            *m = mode;
            *d = direction;
            *speed = 20.0;
            *period = 0.4;
        }
        MatrixRenderer::render(&profile, Local::now(), Duration::from_secs_f32(seconds), geometry).unwrap()
    }

    fn lit_columns(image: &RgbImage) -> Option<(u32, u32)> {
        let xs = image.enumerate_pixels().filter(|(_, _, pixel)| pixel.0 != [0, 0, 0]).map(|(x, _, _)| x);
        xs.clone().min().zip(xs.max())
    }

    #[test]
    fn every_animation_moves_in_every_direction() {
        for mode in TextMode::ALL.into_iter().filter(|mode| *mode != TextMode::Static) {
            for direction in ScrollDirection::ALL {
                let frames: Vec<_> = [0.05, 0.25, 0.45, 0.65, 0.85]
                    .map(|seconds| text_frame(mode, direction, seconds)).into();
                assert!(frames.windows(2).any(|pair| pair[0] != pair[1]), "{mode:?} {direction:?} is frozen");
            }
        }
        // Blink is off for the second half of each period.
        assert!(lit_columns(&text_frame(TextMode::Blink, ScrollDirection::Left, 0.3)).is_none());
    }

    #[test]
    fn text_cycle_matches_the_animation_period() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        for mode in TextMode::ALL.into_iter().filter(|mode| *mode != TextMode::Static) {
            for direction in ScrollDirection::ALL {
                let mut element = Element::text();
                if let ElementKind::Text { text, mode: m, direction: d, speed, period, scroll_pause, .. } = &mut element.kind {
                    *text = "Cycle".into();
                    (*m, *d, *speed, *period, *scroll_pause) = (mode, direction, 25.0, 0.5, 0.75);
                }
                let cycle = MatrixRenderer::text_cycle(&element, geometry).unwrap().unwrap();
                for start in [0.13, 0.61] {
                    let at = |seconds: f64| {
                        MatrixRenderer::render(&element, Local::now(), Duration::from_secs_f64(seconds), geometry).unwrap()
                    };
                    assert!(
                        at(start) == at(start + cycle.as_secs_f64()),
                        "{mode:?} {direction:?}: frame differs one cycle ({cycle:?}) later",
                    );
                }
            }
        }
        assert_eq!(MatrixRenderer::text_cycle(&Element::clock(), geometry).unwrap(), None);
    }

    #[test]
    fn scroll_directions_move_opposite_ways() {
        let left = |seconds| lit_columns(&text_frame(TextMode::Scroll, ScrollDirection::Left, seconds)).unwrap().0;
        let right = |seconds| lit_columns(&text_frame(TextMode::Scroll, ScrollDirection::Right, seconds)).unwrap().1;
        assert!(left(2.0) < left(1.5));
        assert!(right(2.0) > right(1.5));
    }

    #[test]
    fn ignoring_safe_area_uses_whole_canvas() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let area = geometry.safe_content_area();
        let mut profile = Element::text();
        if let ElementKind::Text { text, mode, ignore_safe_area, .. } = &mut profile.kind {
            *text = "WIDE TEXT".into();
            *mode = TextMode::Static;
            *ignore_safe_area = true;
        }
        let image = MatrixRenderer::render(&profile, Local::now(), Duration::ZERO, geometry).unwrap();
        assert!(image.enumerate_pixels().any(|(x, _, pixel)| {
            pixel.0 != [0, 0, 0] && (x < area.x || x >= area.x + area.width)
        }));
    }

    #[test]
    fn y_offset_moves_text_vertically() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let lit_rows = |offset| {
            let mut element = Element::text();
            if let ElementKind::Text { mode, y_offset, .. } = &mut element.kind {
                *mode = TextMode::Static;
                *y_offset = offset;
            }
            let image = MatrixRenderer::render(&element, Local::now(), Duration::ZERO, geometry).unwrap();
            let ys: Vec<_> = image.enumerate_pixels().filter(|(_, _, px)| px.0 != [0, 0, 0]).map(|(_, y, _)| y).collect();
            (*ys.iter().min().unwrap(), *ys.iter().max().unwrap())
        };
        let (top, bottom) = lit_rows(0);
        assert_eq!(lit_rows(-5), (top - 5, bottom - 5));
    }

    #[test]
    fn clock_and_battery_can_ignore_the_safe_area() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let area = geometry.safe_content_area();
        let outside = |element: &Element| {
            MatrixRenderer::render(element, Local::now(), Duration::ZERO, geometry).unwrap()
                .enumerate_pixels()
                .any(|(x, y, px)| px.0 != [0, 0, 0] && !(area.x..area.x + area.width).contains(&x)
                    || px.0 != [0, 0, 0] && !(area.y..area.y + area.height).contains(&y))
        };
        for mut element in [Element::clock(), Element::battery()] {
            assert!(!outside(&element), "{} leaks out of the safe area", element.kind.label());
            if let ElementKind::Clock { ignore_safe_area, font_size, .. }
            | ElementKind::Battery { ignore_safe_area, font_size, .. } = &mut element.kind
            {
                (*ignore_safe_area, *font_size) = (true, 30.0);
            }
            assert!(outside(&element), "{} stays inside with ignore_safe_area", element.kind.label());
        }
    }

    fn animated_gif(mode: TextMode, direction: ScrollDirection) -> Element {
        let mut element = Element::gif();
        if let ElementKind::Gif { path, layout, mode: m, direction: d, speed, period, scroll_pause, .. } = &mut element.kind {
            *path = "sprite.gif".into();
            *layout = crate::model::GifLayout::Animate;
            (*m, *d, *speed, *period, *scroll_pause) = (mode, direction, 25.0, 0.5, 0.75);
        }
        element
    }

    #[test]
    fn gif_sprites_animate_like_text() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let sprite = Sprite { width: 6, height: 4, levels: vec![200; 24] };
        for mode in TextMode::ALL.into_iter().filter(|mode| *mode != TextMode::Static) {
            for direction in ScrollDirection::ALL {
                let element = animated_gif(mode, direction);
                let at = |seconds: f64| {
                    MatrixRenderer::render_sprite(&element, &sprite, Duration::from_secs_f64(seconds), geometry).unwrap()
                };
                let cycle = MatrixRenderer::sprite_cycle(&element, &sprite, geometry).unwrap().unwrap();
                let frames: Vec<_> = [0.05, 0.3, 0.55, 0.8].map(at).into();
                assert!(frames.windows(2).any(|pair| pair[0] != pair[1]), "{mode:?} {direction:?} is frozen");
                assert!(at(0.13) == at(0.13 + cycle.as_secs_f64()), "{mode:?} {direction:?} cycle is off");
            }
        }
        // Pulse dims the sprite's own levels.
        let dimmed = MatrixRenderer::render_sprite(&animated_gif(TextMode::Pulse, ScrollDirection::Left), &sprite,
            Duration::from_secs_f64(0.125), geometry).unwrap();
        assert!(dimmed.pixels().all(|px| px.0[0] < 200));
    }

    fn battery_frame(style: BatteryStyle, label: &str, percent: u8, charging: bool, seconds: f32) -> RgbImage {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let font = crate::model::Element::clock();
        let ElementKind::Clock { font, .. } = font.kind else { unreachable!() };
        let state = BatteryState { percent, charging };
        render_battery(&font, 11.0, style, label, Some(state), Duration::from_secs_f32(seconds), geometry,
            geometry.safe_content_area()).unwrap()
    }

    fn brightness(image: &RgbImage) -> u64 {
        image.pixels().map(|pixel| u64::from(pixel.0[0])).sum()
    }

    #[test]
    fn every_battery_style_shows_more_charge_as_brighter() {
        for style in BatteryStyle::ALL {
            let (low, high) = (battery_frame(style, "", 15, false, 0.3), battery_frame(style, "", 95, false, 0.3));
            assert!(brightness(&high) > brightness(&low), "{style:?} does not track the charge");
        }
    }

    #[test]
    fn animated_battery_styles_move_while_charging() {
        for style in BatteryStyle::ALL.into_iter().filter(|style| style.is_animated()) {
            let frames: Vec<_> = [0.1, 0.4, 0.7, 1.0].map(|t| battery_frame(style, "", 60, true, t)).into();
            assert!(frames.windows(2).any(|pair| pair[0] != pair[1]), "{style:?} is frozen while charging");
        }
    }

    #[test]
    fn battery_label_sits_below_the_gauge() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let area = geometry.safe_content_area();
        let with = battery_frame(BatteryStyle::Classic, "HOME", 50, false, 0.0);
        let bottom_rows = |image: &RgbImage| image.enumerate_pixels()
            .filter(|(_, y, pixel)| *y >= area.y + area.height - 5 && pixel.0[0] > 0).count();
        assert!(bottom_rows(&with) > bottom_rows(&battery_frame(BatteryStyle::Classic, "", 50, false, 0.0)));
    }

    fn plain(color: OverlayColor) -> OverlayStyle {
        OverlayStyle { color, bold: false, italic: false, outline: 0 }
    }

    fn default_font_path() -> std::path::PathBuf {
        let ElementKind::Clock { font, .. } = Element::clock().kind else { unreachable!() };
        font
    }

    #[test]
    fn overlay_text_lights_or_cuts_out() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let font = default_font_path();
        let size = (geometry.width * geometry.height) as usize;
        let mut dark = vec![0u8; size];
        overlay_text(&mut dark, geometry, "HI", &font, 12.0, plain(OverlayColor::White)).unwrap();
        assert!(dark.iter().any(|&level| level > 200));
        let mut lit = vec![180u8; size];
        overlay_text(&mut lit, geometry, "HI", &font, 12.0, plain(OverlayColor::Black)).unwrap();
        assert!(lit.iter().any(|&level| level < 40));
        assert!(lit.iter().filter(|&&level| level == 180).count() > size / 2);
    }

    #[test]
    fn outline_uses_the_opposite_color_and_grows_with_width() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let font = default_font_path();
        let size = (geometry.width * geometry.height) as usize;
        let count = |background: u8, style: OverlayStyle, level: u8| {
            let mut levels = vec![background; size];
            overlay_text(&mut levels, geometry, "HI", &font, 12.0, style).unwrap();
            levels.iter().filter(|&&value| value == level).count()
        };
        // White text on a lit GIF: the outline cuts a dark border around it.
        let white = |outline| OverlayStyle { outline, ..plain(OverlayColor::White) };
        assert_eq!(count(150, white(0), 0), 0);
        assert!(count(150, white(1), 0) > 0);
        assert!(count(150, white(3), 0) > count(150, white(1), 0));
        // Black text on a dark GIF: the outline lights a border around it.
        let black = |outline| OverlayStyle { outline, ..plain(OverlayColor::Black) };
        assert_eq!(count(0, black(0), 255), 0);
        assert!(count(0, black(2), 255) > 0);
    }

    #[test]
    fn bold_and_italic_change_the_text() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let font = default_font_path();
        let render = |bold, italic| {
            let mut levels = vec![0u8; (geometry.width * geometry.height) as usize];
            let style = OverlayStyle { bold, italic, ..plain(OverlayColor::White) };
            overlay_text(&mut levels, geometry, "Hi", &font, 12.0, style).unwrap();
            levels
        };
        let lit = |levels: &Vec<u8>| levels.iter().map(|&value| u32::from(value)).sum::<u32>();
        let regular = render(false, false);
        assert!(lit(&render(true, false)) > lit(&regular));
        assert_ne!(render(false, true), regular);
        assert_ne!(render(true, true), render(true, false));
    }

    #[test]
    fn styled_fonts_prefer_sibling_files_and_fake_the_rest() {
        let root = tempdir().unwrap();
        let regular = root.path().join("Face-Regular.ttf");
        std::fs::copy(default_font_path(), &regular).unwrap();
        std::fs::copy(default_font_path(), root.path().join("Face-Bold.ttf")).unwrap();
        assert_eq!(font_variant(&regular, true, false).unwrap().0, root.path().join("Face-Bold.ttf"));
        assert_eq!(font_variant(&regular, false, true), None);
        // Capitalization of the files on disk does not matter.
        std::fs::copy(default_font_path(), root.path().join("face-oblique.TTF")).unwrap();
        assert_eq!(font_variant(&regular, false, true).unwrap().0, root.path().join("face-oblique.TTF"));
        std::fs::copy(default_font_path(), root.path().join("FACE-BOLDOBLIQUE.ttf")).unwrap();
        assert_eq!(font_variant(&regular, true, true).unwrap().0, root.path().join("FACE-BOLDOBLIQUE.ttf"));
        // With only a bold file, bold is real and italic has to be faked.
        let other = tempdir().unwrap();
        let lone = other.path().join("Lone.ttf");
        std::fs::copy(default_font_path(), &lone).unwrap();
        std::fs::copy(default_font_path(), other.path().join("Lone-Bold.ttf")).unwrap();
        let (_, fake_bold, fake_italic) = styled_font(&lone, true, true).unwrap();
        assert_eq!((fake_bold, fake_italic), (false, true));
    }

    #[test]
    fn battery_fill_tracks_percentage() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let font = Element::clock().kind;
        let ElementKind::Clock { font, .. } = font else { unreachable!() };
        let lit = |percent| {
            let state = BatteryState { percent, charging: false };
            render_battery(&font, 11.0, BatteryStyle::Classic, "", Some(state), Duration::ZERO, geometry,
                geometry.safe_content_area()).unwrap()
                .pixels().filter(|pixel| pixel.0 != [0, 0, 0]).count()
        };
        assert!(lit(90) > lit(20));
    }

    #[test]
    fn dated_ga402_clock_stays_inside_safe_area() {
        let directory = tempdir().unwrap();
        let output = directory.path().join("dated-clock.png");
        for millis in [false, true] {
            let mut profile = Element::clock();
            if let ElementKind::Clock { show_seconds, show_date, show_millis, .. } = &mut profile.kind {
                *show_seconds = true;
                *show_date = true;
                *show_millis = millis;
            }
            let geometry = MatrixGeometry::for_board_name("GA402RK");
            let area = geometry.safe_content_area();
            let now = Local.with_ymd_and_hms(2026, 9, 30, 12, 34, 56).unwrap();
            MatrixRenderer::render_to(&profile, now, Duration::ZERO, geometry, &output).unwrap();
            let image = image::open(&output).unwrap().into_rgb8();
            for (x, y, pixel) in image.enumerate_pixels() {
                if pixel.0 != [0, 0, 0] {
                    assert!(x >= area.x && x < area.x + area.width, "millis={millis} x={x}");
                    assert!(y >= area.y && y < area.y + area.height, "millis={millis} y={y}");
                }
            }
        }
    }
}
