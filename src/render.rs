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
    BatteryStyle, ContentArea, Element, ElementKind, GifLayout, MatrixGeometry, OverlayColor, ScrollDirection, TextMode,
};
use crate::sensors::{self, BatteryState};

const WHITE: Rgb<u8> = Rgb([255, 255, 255]);

/// Smallest size text is shrunk to when it does not fit.
const MIN_FIT_SIZE: f32 = 6.0;

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

    /// Draws one element on its own canvas, without tilt compensation. GIFs
    /// are decoded by [`crate::animation`] instead.
    pub fn render(
        element: &Element,
        now: DateTime<Local>,
        elapsed: Duration,
        geometry: MatrixGeometry,
    ) -> Result<RgbImage> {
        Self::render_tilted(element, now, elapsed, geometry, 0.0)
    }

    /// [`Self::render`] with tilt compensation of `tilt_per_row` pixels for
    /// elements that ask for it.
    pub fn render_tilted(
        element: &Element,
        now: DateTime<Local>,
        elapsed: Duration,
        geometry: MatrixGeometry,
        tilt_per_row: f32,
    ) -> Result<RgbImage> {
        element.validate()?;
        let stage = Stage::padded(geometry);
        let image = match &element.kind {
            ElementKind::Clock {
                font, font_size, use_24_hour, show_seconds, show_date, date_format, show_millis,
                ignore_safe_area, date_first, millis_digits, ..
            } => render_clock(
                font, *font_size, *use_24_hour, *show_seconds, show_millis.then_some(*millis_digits), show_date.then_some(*date_first), date_format,
                now, stage.canvas, stage.area(layout_area(geometry, *ignore_safe_area)), element.smooth_text,
                *ignore_safe_area,
            )?,
            ElementKind::Text {
                text, font, font_size, mode, speed, ignore_safe_area, scroll_pause, direction, period, ..
            } => {
                let area = stage.area(layout_area(geometry, *ignore_safe_area));
                let motion = Motion { speed: *speed, pause: *scroll_pause, period: *period, direction: *direction };
                let font = load_font(font)?;
                // Only shrunk to fit when laid out on the whole canvas.
                let glyphs = Glyphs::text(&font, text, *font_size, *mode, *direction, area, element.smooth_text, *ignore_safe_area);
                animate(&glyphs, *mode, motion, elapsed, stage, area)
            }
            ElementKind::Gif { .. } => bail!("GIF elements are decoded, not rendered"),
            // Fills the panel whatever its offset or turn.
            ElementKind::Flashlight { .. } => return Ok(RgbImage::from_pixel(geometry.width, geometry.height, WHITE)),
            // Its scale is applied when placing the drawn gauge, like a turn.
            ElementKind::Battery { font, font_size, style, label, ignore_safe_area, .. } => {
                let area = stage.area(layout_area(geometry, *ignore_safe_area));
                render_battery(font, *font_size, *style, label, sensors::battery(), elapsed, stage.canvas, area, element.smooth_text)?
            }
        };
        Ok(stage.finish_image(&image, element, geometry, tilt_per_row))
    }
}

/// The canvas elements are drawn on: the panel's canvas with a margin all
/// round, so content that an offset or rotation brings onto the panel was
/// drawn and is there to show, instead of cut off at the panel's edge.
#[derive(Clone, Copy)]
pub struct Stage {
    /// The whole drawing canvas, margin included.
    pub canvas: MatrixGeometry,
    pad: u32,
}

impl Stage {
    /// A margin as wide as the panel's longest side, enough for any offset
    /// or turn that still leaves something on the panel.
    pub fn padded(geometry: MatrixGeometry) -> Self {
        Self::with_pad(geometry, geometry.width.max(geometry.height))
    }

    /// No margin: drawing straight onto the panel's canvas.
    pub fn exact(geometry: MatrixGeometry) -> Self {
        Self::with_pad(geometry, 0)
    }

    fn with_pad(geometry: MatrixGeometry, pad: u32) -> Self {
        let canvas = MatrixGeometry { width: geometry.width + 2 * pad, height: geometry.height + 2 * pad, ..geometry };
        Self { canvas, pad }
    }

    /// A panel area in drawing-canvas coordinates.
    pub fn area(&self, area: ContentArea) -> ContentArea {
        ContentArea { x: area.x + self.pad, y: area.y + self.pad, ..area }
    }

    /// Where the panel sits on the drawing canvas.
    fn panel(&self) -> ContentArea {
        let pad = 2 * self.pad;
        self.area(ContentArea { x: 0, y: 0, width: self.canvas.width - pad, height: self.canvas.height - pad })
    }

    /// Where the panel pixel (x, y) is read from on the drawing canvas: the
    /// element is turned and leaned first, then moved by its offset, so the
    /// offset moves it the same way on the panel whatever its rotation.
    fn source(&self, placement: Option<&Placement>, (dx, dy): (i32, i32), x: i32, y: i32) -> (f32, f32) {
        let (x, y) = (x - dx, y - dy);
        let (sx, sy) = placement.map_or((x as f32, y as f32), |placement| placement.source(x, y));
        (sx + self.pad as f32, sy + self.pad as f32)
    }

    /// Crops a drawn element down to the panel, placed as it asks.
    pub fn finish_image(&self, image: &RgbImage, element: &Element, geometry: MatrixGeometry, tilt_per_row: f32) -> RgbImage {
        let (placement, offset) = (Placement::of(element, geometry, tilt_per_row), element_offset(&element.kind));
        RgbImage::from_fn(geometry.width, geometry.height, |x, y| {
            let (sx, sy) = self.source(placement.as_ref(), offset, x as i32, y as i32);
            Rgb(std::array::from_fn(|channel| {
                let value = |px: i32, py: i32| {
                    if px < 0 || py < 0 { return 0.0 }
                    image.get_pixel_checked(px as u32, py as u32).map_or(0.0, |pixel| f32::from(pixel.0[channel]))
                };
                sample(value, sx, sy, placement.as_ref())
            }))
        })
    }

    /// [`Self::finish_image`] for a row-major canvas of LED levels, as GIFs
    /// are drawn.
    pub fn finish_levels(&self, levels: &[u8], element: &Element, geometry: MatrixGeometry, tilt_per_row: f32) -> Vec<u8> {
        let (placement, offset) = (Placement::of(element, geometry, tilt_per_row), element_offset(&element.kind));
        let (width, height) = (self.canvas.width as i32, self.canvas.height as i32);
        let value = |x: i32, y: i32| {
            if (0..width).contains(&x) && (0..height).contains(&y) { f32::from(levels[(y * width + x) as usize]) } else { 0.0 }
        };
        (0..geometry.height as i32)
            .flat_map(|y| (0..geometry.width as i32).map(move |x| (x, y)))
            .map(|(x, y)| {
                let (sx, sy) = self.source(placement.as_ref(), offset, x, y);
                sample(value, sx, sy, placement.as_ref())
            })
            .collect()
    }
}

/// How far an element is moved: (right, down) in pixels. Everything but
/// the flashlight, which fills the panel.
pub fn element_offset(kind: &ElementKind) -> (i32, i32) {
    match kind {
        ElementKind::Clock { x_offset, y_offset, .. }
        | ElementKind::Text { x_offset, y_offset, .. }
        | ElementKind::Battery { x_offset, y_offset, .. }
        | ElementKind::Gif { x_offset, y_offset, .. } => (*x_offset, *y_offset),
        ElementKind::Flashlight { .. } => (0, 0),
    }
}

/// An element's rotation and tilt compensation, applied to its drawn frame
/// before its offset. Both work about the element's middle: the middle of the area it is
/// laid out in, so it stays on the visible part of the panel; scrolling text
/// uses the middle of the whole canvas along the way it scrolls, and placed
/// GIFs the middle of the canvas they are centred on.
struct Placement {
    center: (f32, f32),
    sin: f32,
    cos: f32,
    /// Sideways pixels per row away from the middle row.
    tilt: f32,
    /// Pick the nearest pixel instead of blending: crisp text and pixel-art
    /// GIFs stay crisp at any angle.
    nearest: bool,
    /// Size about the middle (battery scale; 1 = as drawn).
    zoom: f32,
}

impl Placement {
    /// `None` when the element is drawn as is.
    fn of(element: &Element, geometry: MatrixGeometry, tilt_per_row: f32) -> Option<Self> {
        let tilt = if element.tilt_compensation { tilt_per_row } else { 0.0 };
        let degrees = element.rotation.rem_euclid(360.0);
        let zoom = match element.kind {
            ElementKind::Battery { scale, .. } => scale.max(0.01),
            _ => 1.0,
        };
        if matches!(element.kind, ElementKind::Flashlight { .. }) || degrees == 0.0 && tilt == 0.0 && zoom == 1.0 {
            return None;
        }
        let (sin, cos) = turn_sin_cos(degrees);
        let (cx, cy) = turn_center(degrees, Self::center(element, geometry));
        let nearest = match &element.kind {
            ElementKind::Gif { smooth_scaling, .. } => !smooth_scaling,
            _ => !element.smooth_text,
        };
        Some(Self { center: (cx, cy), sin, cos, tilt, nearest, zoom })
    }

    fn center(element: &Element, geometry: MatrixGeometry) -> (f32, f32) {
        let middle = |area: ContentArea| {
            (area.x as f32 + (area.width as f32 - 1.0) / 2.0, area.y as f32 + (area.height as f32 - 1.0) / 2.0)
        };
        let canvas = middle(geometry.full_area());
        let along_travel = |ignore_safe_area: bool, mode: TextMode, direction: ScrollDirection| {
            let area = middle(layout_area(geometry, ignore_safe_area));
            match (mode, direction.is_vertical()) {
                (TextMode::Scroll | TextMode::InfiniteScroll, false) => (canvas.0, area.1),
                (TextMode::Scroll | TextMode::InfiniteScroll, true) => (area.0, canvas.1),
                _ => area,
            }
        };
        match &element.kind {
            ElementKind::Clock { ignore_safe_area, .. } | ElementKind::Battery { ignore_safe_area, .. } => {
                middle(layout_area(geometry, *ignore_safe_area))
            }
            ElementKind::Text { ignore_safe_area, mode, direction, .. } => along_travel(*ignore_safe_area, *mode, *direction),
            ElementKind::Gif { layout: GifLayout::Animate, ignore_safe_area, mode, direction, .. } => {
                along_travel(*ignore_safe_area, *mode, *direction)
            }
            ElementKind::Gif { .. } => canvas,
            ElementKind::Flashlight { .. } => canvas,
        }
    }

    /// Where the pixel drawn at (x, y) comes from in the unplaced frame.
    fn source(&self, x: i32, y: i32) -> (f32, f32) {
        let (cx, cy) = self.center;
        // Undo the lean (rows above the middle moved left for positive
        // tilt), then the clockwise turn.
        let x = x as f32 - (self.tilt * (y as f32 - cy)).round();
        let (x, y) = unturn((self.sin, self.cos), (cx, cy), x, y as f32);
        // Undo the zoom about the middle too.
        (cx + (x - cx) / self.zoom, cy + (y - cy) / self.zoom)
    }
}

/// Sine and cosine of a clockwise turn by `degrees` (0..360), exact for
/// quarter turns so they move whole pixels without blur.
fn turn_sin_cos(degrees: f32) -> (f32, f32) {
    match degrees {
        0.0 => (0.0, 1.0),
        90.0 => (1.0, 0.0),
        180.0 => (0.0, -1.0),
        270.0 => (-1.0, 0.0),
        _ => degrees.to_radians().sin_cos(),
    }
}

/// The middle to turn about. A quarter turn lands on whole pixels only when
/// the middle's x and y are both whole or both halves; otherwise it moves
/// half a pixel rather than blur every pixel.
fn turn_center(degrees: f32, (cx, cy): (f32, f32)) -> (f32, f32) {
    if (degrees == 90.0 || degrees == 270.0) && (cx - cy).fract() != 0.0 { (cx - 0.5, cy) } else { (cx, cy) }
}

/// Where (x, y) comes from before a clockwise turn about `center`.
fn unturn((sin, cos): (f32, f32), (cx, cy): (f32, f32), x: f32, y: f32) -> (f32, f32) {
    let (dx, dy) = (x - cx, y - cy);
    (cx + dx * cos + dy * sin, cy - dx * sin + dy * cos)
}

/// The level at (x, y): the nearest pixel if the placement asks for it,
/// otherwise blended.
fn sample(value: impl Fn(i32, i32) -> f32, x: f32, y: f32, placement: Option<&Placement>) -> u8 {
    if placement.is_some_and(|placement| placement.nearest) {
        return value(x.round() as i32, y.round() as i32).round().clamp(0.0, 255.0) as u8;
    }
    bilinear(value, x, y)
}

/// Blends the four pixels around (x, y); whole coordinates read one pixel.
fn bilinear(value: impl Fn(i32, i32) -> f32, x: f32, y: f32) -> u8 {
    let (left, top) = (x.floor(), y.floor());
    let (fx, fy) = (x - left, y - top);
    let (left, top) = (left as i32, top as i32);
    let row = |y| value(left, y) * (1.0 - fx) + value(left + 1, y) * fx;
    (row(top) * (1.0 - fy) + row(top + 1) * fy).round().clamp(0.0, 255.0) as u8
}

/// The whole canvas, or only the part every panel row is guaranteed to show.
fn layout_area(geometry: MatrixGeometry, ignore_safe_area: bool) -> ContentArea {
    if ignore_safe_area { geometry.full_area() } else { geometry.safe_content_area() }
}

/// The time as the clock shows it. Milliseconds (`Some(digits)`, 1–3) imply
/// seconds; chrono has no 1- or 2-digit fractions, so they are cut down
/// here, truncated like a stopwatch.
fn clock_text(now: DateTime<Local>, use_24_hour: bool, show_seconds: bool, show_millis: Option<u8>) -> String {
    let fraction = show_millis.map_or(String::new(), |digits| {
        let digits = u32::from(digits.clamp(1, 3));
        format!(".{:0width$}", now.timestamp_subsec_millis() / 10_u32.pow(3 - digits), width = digits as usize)
    });
    let seconds = if show_seconds || show_millis.is_some() { ":%S" } else { "" };
    if use_24_hour {
        format!("{}{fraction}", now.format(&format!("%H:%M{seconds}")))
    } else {
        format!("{}{fraction} {}", now.format(&format!("%I:%M{seconds}")), now.format("%p"))
    }
}

#[allow(clippy::too_many_arguments)]
fn render_clock(
    font_path: &Path,
    font_size: f32,
    use_24_hour: bool,
    show_seconds: bool,
    // `Some(digits)` when milliseconds are shown.
    show_millis: Option<u8>,
    // `Some(date_first)` when the date is shown.
    show_date: Option<bool>,
    date_format: &str,
    now: DateTime<Local>,
    geometry: MatrixGeometry,
    area: ContentArea,
    smooth: bool,
    fit: bool,
) -> Result<RgbImage> {
    let font = load_font(font_path)?;
    let time = clock_text(now, use_24_hour, show_seconds, show_millis);
    let mut image = canvas(geometry);
    if let Some(date_first) = show_date {
        let date = now.format(date_format).to_string();
        let (time_scale, date_scale) = if fit {
            largest_clock_lines(&font, &time, &date, area)
        } else {
            // Unfitted, the date keeps its usual size relative to the time.
            (PxScale::from(font_size), PxScale::from((font_size * 0.55).max(MIN_FIT_SIZE.min(font_size))))
        };
        let (_, time_height) = text_size(time_scale, &font, &time);
        let (_, date_height) = text_size(date_scale, &font, &date);
        let total_height = time_height + date_height + 1;
        let top = area.y + area.height.saturating_sub(total_height) / 2;
        let lines = [(time_scale, &time, time_height), (date_scale, &date, date_height)];
        let [first, second] = if date_first { [lines[1], lines[0]] } else { lines };
        draw_centered(&mut image, &font, first.0, first.1, top as i32, area, smooth);
        draw_centered(&mut image, &font, second.0, second.1, (top + first.2 + 1) as i32, area, smooth);
    } else {
        let scale = if fit { largest_fit(&font, &time, area.width, area.height) } else { PxScale::from(font_size) };
        let (_, height) = text_size(scale, &font, &time);
        draw_centered(
            &mut image,
            &font,
            scale,
            &time,
            (area.y + area.height.saturating_sub(height) / 2) as i32,
            area,
            smooth,
        );
    }
    Ok(image)
}

/// What an animated element moves around: a text string, or a GIF frame
/// standing in for a single character.
enum Glyphs<'a> {
    Text { font: &'a FontArc, scale: PxScale, text: &'a str, width: i32, height: i32, smooth: bool },
    Sprite(&'a Sprite),
}

impl<'a> Glyphs<'a> {
    /// Lays out `text` like every text animation does: with `fit`,
    /// horizontal movement may exceed the area width and everything else is
    /// shrunk to fit it; without, the font keeps the size asked for.
    #[allow(clippy::too_many_arguments)]
    fn text(
        font: &'a FontArc,
        text: &'a str,
        font_size: f32,
        mode: TextMode,
        direction: ScrollDirection,
        area: ContentArea,
        smooth: bool,
        fit: bool,
    ) -> Self {
        let moving = matches!(mode, TextMode::Scroll | TextMode::InfiniteScroll | TextMode::Bounce);
        let scale = match mode {
            // Laid out on the safe area at the size asked for, even where
            // that runs past it.
            _ if !fit => PxScale::from(font_size),
            // Filling the canvas: as large as fits, across the way it moves.
            _ if moving && !direction.is_vertical() => largest_fit(font, text, u32::MAX, area.height),
            _ if moving => largest_fit(font, text, area.width, u32::MAX),
            _ => largest_fit(font, text, area.width, area.height),
        };
        let (width, height) = block_size(scale, font, text);
        Self::Text { font, scale, text, width: width as i32, height: height as i32, smooth }
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
            Self::Text { text, .. } => text_lines(text).map(|line| line.chars().count()).sum(),
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
            Self::Text { font, scale, text, smooth, width, .. } => {
                let color = Rgb([level; 3]);
                let line_height = line_height(*scale, font) as i32;
                // Characters are counted across lines, for typewriter and wave.
                let mut index = 0;
                for (row, line) in text_lines(text).enumerate() {
                    if index >= shown {
                        break;
                    }
                    // Each line centred in the block; one line fills it.
                    let line_x = x + (*width - text_size(*scale, *font, line).0 as i32) / 2;
                    let line_y = y + row as i32 * line_height;
                    let count = line.chars().count().min(shown - index);
                    match lift {
                        // Whole line at once keeps the font's kerning.
                        None => {
                            let prefix: String = line.chars().take(count).collect();
                            if !prefix.is_empty() {
                                put_text(image, color, line_x, line_y, *scale, font, &prefix, *smooth);
                            }
                        }
                        Some(lift) => {
                            for (column, (offset, character)) in line.char_indices().take(count).enumerate() {
                                let (advance, _) = text_size(*scale, *font, &line[..offset]);
                                let glyph = character.to_string();
                                put_text(image, color, line_x + advance as i32, line_y + lift(index + column), *scale, font, &glyph, *smooth);
                            }
                        }
                    }
                    index += line.chars().count();
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

/// Distance from one infinite-scroll copy to the next: the text's `size`
/// along the way it moves, plus the pause's worth of travel as a gap.
fn infinite_tile(size: i32, motion: Motion) -> i32 {
    (size + (motion.pause * motion.speed).round() as i32).max(1)
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
        // One copy's worth of travel; after the first, a copy passes every tile.
        TextMode::InfiniteScroll => {
            let size = if vertical { h } else { w };
            infinite_tile(size, motion) as f32 / motion.speed
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
        // Edges do not change the size, so smoothing does not matter here.
        let glyphs = Glyphs::text(&font, text, *font_size, *mode, *direction, area, true, *ignore_safe_area);
        Ok(motion_cycle(&glyphs, *mode, motion, geometry, area))
    }

    /// Draws an animated GIF element's current `sprite` moving like text,
    /// moved by its offset but not turned or leaned.
    pub fn render_sprite(element: &Element, sprite: &Sprite, elapsed: Duration, geometry: MatrixGeometry) -> Result<RgbImage> {
        let stage = Stage::padded(geometry);
        let image = Self::render_sprite_on(element, sprite, elapsed, geometry, stage)?;
        let unplaced = Element { rotation: 0.0, tilt_compensation: false, ..element.clone() };
        Ok(stage.finish_image(&image, &unplaced, geometry, 0.0))
    }

    /// [`Self::render_sprite`] onto `stage`'s whole canvas, unmoved; finish
    /// with [`Stage::finish_levels`].
    pub fn render_sprite_on(element: &Element, sprite: &Sprite, elapsed: Duration, geometry: MatrixGeometry, stage: Stage) -> Result<RgbImage> {
        let (mode, motion, area) = sprite_motion(element, geometry)?;
        Ok(animate(&Glyphs::Sprite(sprite), mode, motion, elapsed, stage, stage.area(area)))
    }

    /// One full play of an animated GIF element's movement (not its frames);
    /// `None` when it does not move.
    pub fn sprite_cycle(element: &Element, sprite: &Sprite, geometry: MatrixGeometry) -> Result<Option<Duration>> {
        let (mode, motion, area) = sprite_motion(element, geometry)?;
        Ok(motion_cycle(&Glyphs::Sprite(sprite), mode, motion, geometry, area))
    }
}

fn sprite_motion(element: &Element, geometry: MatrixGeometry) -> Result<(TextMode, Motion, ContentArea)> {
    let ElementKind::Gif { mode, direction, speed, period, scroll_pause, ignore_safe_area, .. } = &element.kind
    else {
        bail!("only GIF elements are drawn as sprites");
    };
    element.validate()?;
    let motion = Motion { speed: *speed, pause: *scroll_pause, period: *period, direction: *direction };
    Ok((*mode, motion, layout_area(geometry, *ignore_safe_area)))
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
    stage: Stage,
    area: ContentArea,
) -> RgbImage {
    // Seconds as f64, wrapped into the current cycle before narrowing: an
    // f32 of the whole time since the profile started loses whole frames of
    // precision after a few days of uptime.
    let t = elapsed.as_secs_f64();
    let within = |period: f32| (t % f64::from(period)) as f32;
    let vertical = motion.direction.is_vertical();
    let (w, h) = glyphs.size();
    let mut x = area.x as i32 + (area.width as i32 - w) / 2;
    let mut y = area.y as i32 + (area.height as i32 - h) / 2;
    let mut image = canvas(stage.canvas);
    let mut level = 255;
    let mut shown = glyphs.count();
    let mut amplitude = None;
    // Further copies drawn besides (x, y), for infinite scroll.
    let mut copies = Vec::new();

    match mode {
        TextMode::Static => {}
        TextMode::InfiniteScroll => {
            let panel = stage.panel();
            let (span, size) = if vertical { (panel.height as i32, h) } else { (panel.width as i32, w) };
            let tile = i64::from(infinite_tile(size, motion));
            // Never wrapped: every copy keeps its place behind the one before.
            let offset = (t * f64::from(motion.speed) + 1e-4) as i64;
            let (left, top) = (panel.x as i32, panel.y as i32);
            // Copy k has travelled `offset - k * tile`; draw those on screen.
            let last = offset / tile;
            let first = ((offset - i64::from(span + size)) / tile).max(0);
            for k in first..=last {
                let travelled = (offset - k * tile) as i32;
                copies.push(match motion.direction {
                    ScrollDirection::Left => (left + span - travelled, y),
                    ScrollDirection::Right => (left + travelled - size, y),
                    ScrollDirection::Up => (x, top + span - travelled),
                    ScrollDirection::Down => (x, top + travelled - size),
                });
            }
            // Drawn as copies only.
            shown = 0;
        }
        TextMode::Scroll => {
            // Drawing is not clipped to the area, so a pass runs across the
            // whole canvas: enter at one edge, leave past the opposite one,
            // then the pause keeps it off screen.
            // Across the panel itself, not the margin drawn around it.
            let panel = stage.panel();
            let (span, size) = if vertical { (panel.height as i32, h) } else { (panel.width as i32, w) };
            let travel = (span + size) as f32;
            let cycle = scroll_cycle(span, size, motion);
            // Frame times sit exactly on pixel steps when FPS and speed
            // match; the nudge stops float error from landing one short.
            let offset = (f64::from(within(cycle)) * f64::from(motion.speed) + 1e-4).min(f64::from(travel)) as i32;
            let (left, top) = (panel.x as i32, panel.y as i32);
            match motion.direction {
                ScrollDirection::Left => x = left + span - offset,
                ScrollDirection::Right => x = left + offset - size,
                ScrollDirection::Up => y = top + span - offset,
                ScrollDirection::Down => y = top + offset - size,
            }
        }
        TextMode::Bounce => {
            // Ping-pong between the area edges; content larger than the area
            // swings so each end comes into view.
            let (start, room) = bounce_room(area, w, h, vertical);
            let range = room.abs().max(1) as f32;
            let phase = ((t * f64::from(motion.speed) + 1e-4) % f64::from(2.0 * range)) as f32;
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
            if within(motion.period) >= motion.period / 2.0 {
                shown = 0;
            }
        }
        TextMode::Pulse => {
            let phase = within(motion.period) / motion.period;
            level = (255.0 * (1.0 - (2.0 * phase - 1.0).abs())) as u8;
        }
        TextMode::Typewriter => {
            // Positioned for the full content so typed characters never shift.
            let cycle = typewriter_cycle(shown, motion);
            shown = ((within(cycle) / motion.period) as usize + 1).min(shown);
        }
        TextMode::Wave => {
            amplitude = Some(((area.height as i32 - h) / 2).clamp(1, 3) as f32);
        }
    }
    let base = within(motion.period) * std::f32::consts::TAU / motion.period;
    let step = glyphs.wave_step();
    let wave = amplitude.map(|amplitude| {
        move |index: usize| (amplitude * (base - index as f32 * step).sin()).round() as i32
    });
    glyphs.draw(&mut image, x, y, level, shown, wave.as_ref().map(|wave| wave as &dyn Fn(usize) -> i32));
    for (x, y) in copies {
        glyphs.draw(&mut image, x, y, level, glyphs.count(), None);
    }
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
    smooth: bool,
) -> Result<RgbImage> {
    let font = load_font(font_path)?;
    let mut image = canvas(geometry);
    let mut gauge = area;
    if !label.is_empty() {
        let scale = fit_scale(&font, label, font_size * 0.8, area.width);
        let (_, label_h) = text_size(scale, &font, label);
        let top = area.y + area.height.saturating_sub(label_h);
        draw_centered(&mut image, &font, scale, label, top as i32, area, smooth);
        gauge.height = area.height.saturating_sub(label_h + 1).max(4);
    }
    // f64 seconds: an f32 of days of uptime would make the motion choppy.
    let t = elapsed.as_secs_f64();
    match style {
        BatteryStyle::Classic => draw_classic(&mut image, &font, font_size, state, gauge, smooth),
        BatteryStyle::Liquid => draw_liquid(&mut image, &font, font_size, state, gauge, t, smooth),
        BatteryStyle::Segments => draw_segments(&mut image, &font, font_size, state, gauge, t, smooth),
        BatteryStyle::Ring => draw_ring(&mut image, &font, font_size, state, gauge, t, smooth),
        BatteryStyle::Big => draw_big(&mut image, &font, font_size, state, gauge, t, smooth),
    }
    Ok(image)
}

/// `t * rate` radians, wrapped to one turn before narrowing to f32.
fn angle(t: f64, rate: f64) -> f32 {
    ((t * rate) % std::f64::consts::TAU) as f32
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
#[allow(clippy::too_many_arguments)]
fn icon_and_percent(
    image: &mut RgbImage,
    font: &FontArc,
    font_size: f32,
    state: Option<BatteryState>,
    area: ContentArea,
    smooth: bool,
    icon_w: u32,
    draw_icon: impl FnOnce(&mut RgbImage, i32),
) {
    const GAP: u32 = 2;
    // Anti-aliased glyphs such as "%" reach a pixel past their measured
    // width; leave room for it so a full "100%" stays inside the area.
    const OVERHANG: u32 = 1;
    let text = percent_text(state);
    let scale = fit_scale(font, &text, font_size.min(area.height as f32), area.width.saturating_sub(icon_w + GAP + OVERHANG));
    let (text_w, text_h) = text_size(scale, font, &text);
    let left = area.x + area.width.saturating_sub(icon_w + GAP + text_w + OVERHANG) / 2;
    draw_icon(image, left as i32);
    let text_y = area.y + area.height.saturating_sub(text_h) / 2;
    put_text(image, WHITE, (left + icon_w + GAP) as i32, text_y as i32, scale, font, &text, smooth);
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
fn draw_classic(image: &mut RgbImage, font: &FontArc, font_size: f32, state: Option<BatteryState>, area: ContentArea, smooth: bool) {
    const BODY_W: u32 = 16;
    const TIP_W: u32 = 2;
    let body_h = area.height.min(11);
    let top = (area.y + area.height.saturating_sub(body_h) / 2) as i32;
    icon_and_percent(image, font, font_size, state, area, smooth, BODY_W + TIP_W, |image, left| {
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
fn draw_liquid(image: &mut RgbImage, font: &FontArc, font_size: f32, state: Option<BatteryState>, area: ContentArea, t: f64, smooth: bool) {
    let body_h = area.height.clamp(6, 22);
    let body_w = (body_h * 3 / 5).max(6);
    let top = (area.y + area.height.saturating_sub(body_h) / 2) as i32 + 1;
    icon_and_percent(image, font, font_size, state, area, smooth, body_w, |image, left| {
        fill_rect(image, left + body_w as i32 / 2 - 2, top - 1, 4, 1, 255);
        draw_hollow_rect_mut(image, Rect::at(left, top).of_size(body_w, body_h - 1), WHITE);
        let (inner_x, inner_top, inner_w, inner_h) = (left + 2, top + 2, body_w as i32 - 4, body_h as i32 - 5);
        let level = inner_h as f32 * percent(state) as f32 / 100.0;
        let bottom = inner_top + inner_h;
        for dx in 0..inner_w {
            // Two travelling waves make the surface look alive.
            let wave = (angle(t, 3.0) + dx as f32 * 0.9).sin() * 0.8 + (angle(t, 1.7) - dx as f32 * 0.5).sin() * 0.5;
            let surface = (bottom as f32 - level + wave).round() as i32;
            for y in surface.max(inner_top)..bottom {
                plot(image, inner_x + dx, y, 255);
            }
        }
        if charging(state) && inner_w > 2 {
            // Bubbles: dark pixels rising through the liquid at their own pace.
            for bubble in 0..3 {
                let speed = 4.0 + bubble as f32 * 1.5;
                let rise = (((t * f64::from(speed)) % f64::from(level.max(1.0))) as f32 + bubble as f32 * 3.7) % level.max(1.0);
                let x = inner_x + 1 + (bubble * 2 + (t * 0.7) as i32) % (inner_w - 2).max(1);
                plot(image, x, bottom - 1 - rise as i32, 0);
            }
        }
    });
}

/// Five cells; the empty ones fill one by one while charging, and the last
/// cell blinks when the charge is low.
fn draw_segments(image: &mut RgbImage, font: &FontArc, font_size: f32, state: Option<BatteryState>, area: ContentArea, t: f64, smooth: bool) {
    const CELLS: u32 = 5;
    const CELL_W: u32 = 2;
    let body_w = CELLS * (CELL_W + 1) + 3;
    let body_h = area.height.min(11);
    let top = (area.y + area.height.saturating_sub(body_h) / 2) as i32;
    let lit = percent(state).div_ceil(100 / CELLS).min(CELLS);
    icon_and_percent(image, font, font_size, state, area, smooth, body_w + 2, |image, left| {
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
fn draw_ring(image: &mut RgbImage, font: &FontArc, font_size: f32, state: Option<BatteryState>, area: ContentArea, t: f64, smooth: bool) {
    let radius = (area.height.min(area.width / 2) as i32 / 2 - 1).max(3);
    let size = (radius * 2 + 1) as u32;
    let cy = (area.y + area.height / 2) as i32;
    let filled = percent(state) as f32 / 100.0;
    icon_and_percent(image, font, font_size, state, area, smooth, size, |image, left| {
        let cx = left + radius;
        let steps = (radius * 16).max(64);
        for step in 0..steps {
            let turn = step as f32 / steps as f32;
            let angle = turn * std::f32::consts::TAU;
            let (x, y) = (cx + (radius as f32 * angle.sin()).round() as i32, cy - (radius as f32 * angle.cos()).round() as i32);
            plot(image, x, y, if turn <= filled { 255 } else { 50 });
        }
        if charging(state) {
            let head = (t * 0.6).fract() as f32;
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
fn draw_big(image: &mut RgbImage, font: &FontArc, font_size: f32, state: Option<BatteryState>, area: ContentArea, t: f64, smooth: bool) {
    let text = percent_text(state);
    let scale = fit_scale(font, &text, (area.height as f32 * 1.3).max(font_size), area.width);
    let (text_w, text_h) = text_size(scale, font, &text);
    let mut digits = RgbImage::new(image.width(), image.height());
    let (x, y) = (area.x as i32 + (area.width as i32 - text_w as i32) / 2, area.y as i32 + (area.height as i32 - text_h as i32) / 2);
    put_text(&mut digits, WHITE, x, y, scale, font, &text, smooth);
    let tide = y as f32 + text_h as f32 * (1.0 - percent(state) as f32 / 100.0);
    let rise = if charging(state) { angle(t, 2.0).sin() * 1.5 } else { 0.0 };
    for (px, py, pixel) in digits.enumerate_pixels() {
        if pixel.0[0] == 0 {
            continue;
        }
        let line = tide + rise + (angle(t, 4.0) + px as f32 * 0.6).sin();
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
    /// Anti-aliased edges; see [`put_text`].
    pub smooth: bool,
    /// Pixels right and down from centred (negative: left, up).
    pub offset: (i32, i32),
    /// Degrees to turn the text clockwise about its middle, before `offset`.
    pub rotation: f32,
    /// How strongly it shows (1 = fully), for fading with its element.
    pub opacity: f32,
    pub animation: OverlayAnimation,
}

/// A GIF overlay's text animation, as a text element's.
#[derive(Clone, Copy, Debug)]
pub struct OverlayAnimation {
    pub mode: TextMode,
    pub direction: ScrollDirection,
    pub speed: f32,
    pub period: f32,
    pub pause: f32,
    /// How far into the animation this frame is.
    pub elapsed: Duration,
}

impl Default for OverlayAnimation {
    fn default() -> Self {
        Self { mode: TextMode::Static, direction: ScrollDirection::Left, speed: 18.0, period: 1.0, pause: 0.0, elapsed: Duration::ZERO }
    }
}

/// Draws a GIF's overlay `text` on top of a finished panel frame in LED
/// order, as a text element would be, after every element is layered: white
/// lights LEDs up, black cuts them out. An outline in the opposite color is
/// drawn first so the text stands out from busy GIFs.
pub fn overlay_text_on_panel(
    leds: &mut [u8],
    geometry: MatrixGeometry,
    text: &str,
    font_path: &Path,
    font_size: f32,
    style: OverlayStyle,
) -> Result<()> {
    let (mask, ring) = overlay_masks(geometry, Stage::exact(geometry), text, font_path, font_size, style)?;
    let to_leds = |mask: &[u8]| crate::matrix::level_buffer(mask, style.opacity.clamp(0.0, 1.0), geometry);
    apply_overlay(leds, &to_leds(&mask), ring.as_deref().map(to_leds).as_deref(), style.color);
    Ok(())
}

/// Lights up or cuts out `levels` by the text `mask`, its outline `ring`
/// first in the opposite color.
fn apply_overlay(levels: &mut [u8], mask: &[u8], ring: Option<&[u8]>, color: OverlayColor) {
    let (outline_on, text_on): (fn(u8, u8) -> u8, fn(u8, u8) -> u8) = match color {
        OverlayColor::White => (cut_out, light_up),
        OverlayColor::Black => (light_up, cut_out),
    };
    for (level, &coverage) in levels.iter_mut().zip(ring.unwrap_or_default()) {
        // Solid, even where small anti-aliased strokes never reach full
        // coverage; a faint border would not help readability.
        *level = outline_on(*level, coverage.saturating_mul(3));
    }
    for (level, &coverage) in levels.iter_mut().zip(mask) {
        *level = text_on(*level, coverage);
    }
}

/// The overlay's text coverage over `stage`'s canvas, and its outline's
/// when it has one.
fn overlay_masks(
    geometry: MatrixGeometry,
    stage: Stage,
    text: &str,
    font_path: &Path,
    font_size: f32,
    style: OverlayStyle,
) -> Result<(Vec<u8>, Option<Vec<u8>>)> {
    let (font, fake_bold, fake_italic) = styled_font(font_path, style.bold, style.italic)?;
    let area = stage.area(geometry.safe_content_area());
    let geometry = stage.canvas;
    // Laid out and animated exactly like a text element.
    let animation = style.animation;
    // At the size asked for, centred on the safe area even where it runs past.
    let glyphs = Glyphs::text(&font, text, font_size, animation.mode, animation.direction, area, style.smooth, false);
    let motion = Motion { speed: animation.speed, pause: animation.pause, period: animation.period, direction: animation.direction };
    let drawn = animate(&glyphs, animation.mode, motion, animation.elapsed, stage, area);
    let (width, rows) = (geometry.width as usize, geometry.height as usize);
    let mut mask: Vec<u8> = drawn.pixels().map(|pixel| pixel.0[0]).collect();
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
    // Turned about the text's middle, then moved as a whole, so text moved
    // past an edge of the area still draws.
    let degrees = style.rotation.rem_euclid(360.0);
    if degrees != 0.0 || style.offset != (0, 0) {
        let turn = turn_sin_cos(degrees);
        let middle = turn_center(degrees, (area.x as f32 + (area.width as f32 - 1.0) / 2.0, area.y as f32 + (area.height as f32 - 1.0) / 2.0));
        let (dx, dy) = style.offset;
        let value = |x: i32, y: i32| {
            if (0..width as i32).contains(&x) && (0..rows as i32).contains(&y) { f32::from(mask[y as usize * width + x as usize]) } else { 0.0 }
        };
        mask = (0..rows as i32)
            .flat_map(|y| (0..width as i32).map(move |x| (x, y)))
            .map(|(x, y)| {
                let (sx, sy) = unturn(turn, middle, (x - dx) as f32, (y - dy) as f32);
                if style.smooth { bilinear(value, sx, sy) } else { value(sx.round() as i32, sy.round() as i32) as u8 }
            })
            .collect();
    }

    let ring = (style.outline > 0).then(|| dilate(&mask, width, rows, style.outline));
    Ok((mask, ring))
}

/// A text element's outline: its lit pixels grown by `outline` pixels,
/// solid even where thin anti-aliased strokes never reach full coverage, as
/// overlay outlines are.
pub fn outline_ring(levels: &[u8], geometry: MatrixGeometry, outline: u32) -> Vec<u8> {
    let ring = dilate(levels, geometry.width as usize, geometry.height as usize, outline);
    ring.into_iter().map(|coverage| coverage.saturating_mul(3)).collect()
}

/// Darkens `levels` by `coverage`: fully where it is 255.
pub fn cut_under(levels: &mut [u8], coverage: &[u8]) {
    for (level, &coverage) in levels.iter_mut().zip(coverage) {
        *level = cut_out(*level, coverage);
    }
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
    fit_block(font, text, requested, max_width, None)
}

/// [`fit_scale`] that also keeps the lines within `max_height` when given.
fn fit_block(font: &FontArc, text: &str, requested: f32, max_width: u32, max_height: Option<u32>) -> PxScale {
    let mut size = requested;
    loop {
        let scale = PxScale::from(size);
        let (width, height) = block_size(scale, font, text);
        // Shrinking to fit stops at 6 px, where text stops being readable;
        // a size set smaller than that is used as is.
        if width <= max_width && max_height.is_none_or(|max| height <= max) || size <= MIN_FIT_SIZE {
            return scale;
        }
        size -= 0.5;
    }
}

/// The lines of `text`, split at line breaks.
fn text_lines(text: &str) -> impl Iterator<Item = &str> {
    text.split('\n').map(|line| line.trim_end_matches('\r'))
}

/// Size of `text` laid out as lines one font height apart: the widest
/// line's width, and the line spacing for every line but the last plus the
/// tallest line's own height. One line measures as [`text_size`].
fn block_size(scale: PxScale, font: &FontArc, text: &str) -> (u32, u32) {
    if !text.contains('\n') {
        return text_size(scale, font, text);
    }
    let sizes: Vec<_> = text_lines(text).map(|line| text_size(scale, font, line)).collect();
    let width = sizes.iter().map(|size| size.0).max().unwrap_or(0);
    let tallest = sizes.iter().map(|size| size.1).max().unwrap_or(0);
    (width, line_height(scale, font) * (sizes.len() as u32 - 1) + tallest)
}

/// Distance between lines: the font's ascent to descent. [`text_size`]
/// only measures the glyphs' ink, so it cannot space lines.
fn line_height(scale: PxScale, font: &FontArc) -> u32 {
    use ab_glyph::{Font, ScaleFont};
    font.as_scaled(scale).height().ceil() as u32
}

/// The largest size at which `text` fits within `max_width` by `max_height`
/// (either may be unbounded), down to 1.
fn largest_fit(font: &FontArc, text: &str, max_width: u32, max_height: u32) -> PxScale {
    largest_size(max_width.min(max_height), |scale| {
        let (width, height) = block_size(scale, font, text);
        width <= max_width && height <= max_height
    })
}

/// The largest size for which `fits` holds, searched up to about twice
/// `bound` pixels (a font's ink is shorter than its size), down to 1.
fn largest_size(bound: u32, fits: impl Fn(PxScale) -> bool) -> PxScale {
    let (mut low, mut high) = (1.0_f32, (bound.min(512) as f32 * 2.0).max(2.0));
    if !fits(PxScale::from(low)) {
        return PxScale::from(low);
    }
    for _ in 0..20 {
        let middle = (low + high) / 2.0;
        if fits(PxScale::from(middle)) { low = middle } else { high = middle }
    }
    PxScale::from(low)
}

/// Time and date as large as they fit `area` together, the date at its
/// usual 0.55 of the time.
fn largest_clock_lines(font: &FontArc, time: &str, date: &str, area: ContentArea) -> (PxScale, PxScale) {
    let date_scale = |time: PxScale| PxScale::from(time.y * 0.55);
    let time_scale = largest_size(area.width.min(area.height), |time_scale| {
        let (time_w, time_h) = text_size(time_scale, font, time);
        let (date_w, date_h) = text_size(date_scale(time_scale), font, date);
        time_w.max(date_w) <= area.width && time_h + date_h < area.height
    });
    (time_scale, date_scale(time_scale))
}

fn draw_centered(
    image: &mut RgbImage,
    font: &FontArc,
    scale: PxScale,
    text: &str,
    y: i32,
    area: ContentArea,
    smooth: bool,
) {
    let line_height = line_height(scale, font) as i32;
    for (row, line) in text_lines(text).enumerate() {
        let (width, _) = text_size(scale, font, line);
        let x = area.x as i32 + (area.width as i32 - width as i32) / 2;
        put_text(image, WHITE, x, y + row as i32 * line_height, scale, font, line, smooth);
    }
}

/// Draws `text` with its top left at (x, y). `smooth` anti-aliases the
/// edges; without it each pixel is fully on (at least half covered by the
/// glyph) or off, which looks crisper on the panel's coarse LEDs.
#[allow(clippy::too_many_arguments)]
fn put_text(image: &mut RgbImage, color: Rgb<u8>, x: i32, y: i32, scale: PxScale, font: &FontArc, text: &str, smooth: bool) {
    if smooth {
        draw_text_mut(image, color, x, y, scale, font, text);
        return;
    }
    // Drawn into a small mask around the text with room for glyphs that
    // reach past their measured box, then copied where at least half lit.
    let (width, height) = text_size(scale, font, text);
    let margin = scale.y.ceil() as u32 + 2;
    let mut mask = image::GrayImage::new(width + 2 * margin, height + 2 * margin);
    draw_text_mut(&mut mask, image::Luma([255]), margin as i32, margin as i32, scale, font, text);
    for (mx, my, coverage) in mask.enumerate_pixels() {
        let (px, py) = (x + mx as i32 - margin as i32, y + my as i32 - margin as i32);
        if coverage.0[0] >= 128 && px >= 0 && py >= 0 && (px as u32) < image.width() && (py as u32) < image.height() {
            image.put_pixel(px as u32, py as u32, color);
        }
    }
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
    fn half_turned_clock_stays_inside_the_safe_area() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let now = Local.with_ymd_and_hms(2026, 9, 30, 12, 34, 56).unwrap();
        let mut clock = Element::clock();
        let upright = MatrixRenderer::render(&clock, now, Duration::ZERO, geometry).unwrap();
        clock.rotation = 180.0;
        let turned = MatrixRenderer::render(&clock, now, Duration::ZERO, geometry).unwrap();
        assert_ne!(upright, turned);
        // Every lit pixel lands exactly on its partner across the safe area's middle.
        let area = geometry.safe_content_area();
        let (cx, cy) = (2 * area.x + area.width - 1, 2 * area.y + area.height - 1);
        for (x, y, pixel) in upright.enumerate_pixels().filter(|(_, _, pixel)| pixel.0 != [0, 0, 0]) {
            assert_eq!(turned.get_pixel(cx - x, cy - y), pixel);
        }
        // -180 and the old `flipped` switch mean the same.
        clock.rotation = -180.0;
        assert_eq!(MatrixRenderer::render(&clock, now, Duration::ZERO, geometry).unwrap(), turned);
        let stored = serde_json::to_string(&Element { rotation: 0.0, ..Element::clock() }).unwrap().replace("\"rotation\":0.0", "\"flipped\":true");
        assert_eq!(serde_json::from_str::<Element>(&stored).unwrap().rotation, 180.0);
    }

    #[test]
    fn quarter_turns_move_whole_pixels() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let levels: Vec<u8> = (0..geometry.width * geometry.height).map(|index| (index % 251) as u8).collect();
        let mut gif = Element::gif();
        let turn = |gif: &Element, levels: &[u8]| Stage::exact(geometry).finish_levels(levels, gif, geometry, 0.0);
        gif.rotation = 180.0;
        let half = turn(&gif, &levels);
        assert_ne!(half, levels);
        assert_eq!(turn(&gif, &half), levels);
        // Quarter turns copy pixels unchanged: a checkerboard stays fully
        // on or off, where blending would leave in-between levels.
        let width = geometry.width as usize;
        let board: Vec<u8> = (0..levels.len()).map(|index| if (index % width + index / width) % 2 == 0 { 255 } else { 0 }).collect();
        for degrees in [90.0, 270.0] {
            gif.rotation = degrees;
            let quarter = turn(&gif, &board);
            assert!(quarter.iter().all(|&level| level == 0 || level == 255), "{degrees}");
            assert!(quarter.contains(&255));
        }
        // Anything else blends.
        gif.rotation = 30.0;
        assert!(turn(&gif, &board).iter().any(|&level| level != 0 && level != 255));
    }

    #[test]
    fn unsmoothed_elements_turn_without_blending() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let width = geometry.width as usize;
        let board: Vec<u8> = (0..width * geometry.height as usize).map(|index| if (index % width + index / width) % 2 == 0 { 255 } else { 0 }).collect();
        let mut gif = Element { rotation: 30.0, ..Element::gif() };
        let turned = |gif: &Element| Stage::exact(geometry).finish_levels(&board, gif, geometry, 0.0);
        assert!(turned(&gif).iter().any(|&level| level != 0 && level != 255));
        if let ElementKind::Gif { smooth_scaling, .. } = &mut gif.kind {
            *smooth_scaling = false;
        }
        assert!(turned(&gif).iter().all(|&level| level == 0 || level == 255));

        // Crisp text stays crisp when turned.
        let now = Local.with_ymd_and_hms(2026, 9, 30, 12, 34, 56).unwrap();
        let clock = Element { rotation: 30.0, smooth_text: false, ..Element::clock() };
        let image = MatrixRenderer::render(&clock, now, Duration::ZERO, geometry).unwrap();
        assert!(image.pixels().all(|pixel| pixel.0[0] == 0 || pixel.0[0] == 255));
    }

    #[test]
    fn tilt_compensation_leans_rows_about_the_middle() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let (width, height) = (geometry.width as usize, geometry.height as usize);
        // A vertical line down the middle column.
        let mut levels = vec![0u8; width * height];
        for y in 0..height {
            levels[y * width + 36] = 255;
        }
        let mut gif = Element::gif();
        assert!(Placement::of(&gif, geometry, 0.5).is_none());
        gif.tilt_compensation = true;
        let leaned = Stage::exact(geometry).finish_levels(&levels, &gif, geometry, 0.5);
        let column = |y: usize| leaned[y * width..(y + 1) * width].iter().position(|&level| level == 255);
        // Top rows move left, bottom rows right, the middle row stays.
        assert_eq!(column(19), Some(36));
        assert_eq!(column(0), Some(36 - 10));
        assert_eq!(column(38), Some(36 + 10));
    }

    #[test]
    fn offsets_bring_in_what_lies_past_the_panel_edge() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let stage = Stage::padded(geometry);
        let width = stage.canvas.width as usize;
        // One lit pixel five columns left of the panel, in the margin.
        let mut levels = vec![0u8; width * stage.canvas.height as usize];
        let edge = stage.area(geometry.full_area());
        levels[(edge.y as usize + 10) * width + edge.x as usize - 5] = 255;
        let mut gif = Element::gif();
        let lit = |gif: &Element| {
            let panel = stage.finish_levels(&levels, gif, geometry, 0.0);
            panel.iter().position(|&level| level == 255).map(|index| (index % geometry.width as usize, index / geometry.width as usize))
        };
        assert_eq!(lit(&gif), None);
        if let ElementKind::Gif { x_offset, y_offset, .. } = &mut gif.kind {
            (*x_offset, *y_offset) = (8, -2);
        }
        assert_eq!(lit(&gif), Some((3, 8)));
    }

    #[test]
    fn offsets_move_both_ways() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let now = Local.with_ymd_and_hms(2026, 9, 30, 12, 34, 56).unwrap();
        let mut clock = Element::clock();
        let lit = |image: &RgbImage| image.enumerate_pixels().find(|(_, _, pixel)| pixel.0 != [0, 0, 0]).map(|(x, y, _)| (x, y));
        let before = lit(&MatrixRenderer::render(&clock, now, Duration::ZERO, geometry).unwrap()).unwrap();
        if let ElementKind::Clock { x_offset, y_offset, .. } = &mut clock.kind {
            (*x_offset, *y_offset) = (3, -2);
        }
        let after = lit(&MatrixRenderer::render(&clock, now, Duration::ZERO, geometry).unwrap()).unwrap();
        assert_eq!(after, (before.0 + 3, before.1 - 2));
    }

    #[test]
    fn scrolling_text_moves_one_pixel_per_frame_after_days() {
        // FPS equal to speed: each frame must move exactly one pixel, also
        // ten days after the profile started.
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let mut element = Element::text();
        if let ElementKind::Text { text, speed, fps, scroll_pause, .. } = &mut element.kind {
            (*text, *speed, *fps, *scroll_pause) = ("I".into(), 20.0, 20.0, 0.0);
        }
        let leftmost = |frame: u64| {
            let elapsed = Duration::from_secs_f64(frame as f64 / 20.0);
            let image = MatrixRenderer::render(&element, Local::now(), elapsed, geometry).unwrap();
            image.enumerate_pixels().filter(|(_, _, pixel)| pixel.0 != [0, 0, 0]).map(|(x, _, _)| x).min()
        };
        let start = 10 * 24 * 3600 * 20;
        let positions: Vec<_> = (start..start + 120).filter_map(leftmost).collect();
        let steps: Vec<i64> = positions.windows(2).map(|pair| pair[0] as i64 - pair[1] as i64).collect();
        // Moving left one pixel a frame, apart from leaving at column 0 and
        // wrapping round to the start; never a skipped pixel.
        assert!(steps.iter().all(|&step| step <= 1), "{steps:?}");
        assert!(steps.iter().filter(|&&step| step == 1).count() > 50);
    }

    #[test]
    fn crisp_text_lights_leds_fully_or_not_at_all() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let now = Local.with_ymd_and_hms(2026, 9, 30, 12, 34, 56).unwrap();
        for element in [Element::clock(), Element::battery()] {
            let levels = |smooth| {
                let element = Element { smooth_text: smooth, ..element.clone() };
                let image = MatrixRenderer::render(&element, now, Duration::ZERO, geometry).unwrap();
                image.pixels().map(|pixel| pixel.0[0]).filter(|&level| level != 0).collect::<Vec<_>>()
            };
            let (smooth, crisp) = (levels(true), levels(false));
            assert!(smooth.iter().any(|&level| level != 255), "{}", element.kind.label());
            assert!(!crisp.is_empty() && crisp.iter().all(|&level| level == 255), "{}", element.kind.label());
        }
    }

    #[test]
    fn multi_line_text_stacks_centred_lines_inside_the_area() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let area = geometry.safe_content_area();
        let mut element = Element::text();
        if let ElementKind::Text { text, mode, .. } = &mut element.kind {
            (*text, *mode) = ("AB\nCD".into(), TextMode::Static);
        }
        let image = MatrixRenderer::render(&element, Local::now(), Duration::ZERO, geometry).unwrap();
        let lit: Vec<_> = image.enumerate_pixels().filter(|(_, _, pixel)| pixel.0 != [0, 0, 0]).map(|(x, y, _)| (x, y)).collect();
        assert!(lit.iter().all(|&(x, y)| (area.x..area.x + area.width).contains(&x) && (area.y..area.y + area.height).contains(&y)));
        // Two bands of rows with a gap between them, not one line.
        let rows: std::collections::BTreeSet<_> = lit.iter().map(|&(_, y)| y).collect();
        let gaps = rows.iter().zip(rows.iter().skip(1)).filter(|(a, b)| **b > **a + 1).count();
        assert!(gaps >= 1, "{rows:?}");
        // No box drawn for the line break.
        let font = load_font(&default_font_path()).unwrap();
        let glyphs = Glyphs::text(&font, "AB\nCD", 15.0, TextMode::Typewriter, ScrollDirection::Left, area, true, true);
        assert_eq!(glyphs.count(), 4);
    }

    #[test]
    fn overlay_moves_by_its_offset() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let font = default_font_path();
        let width = geometry.width as usize;
        let first_lit = |offset| {
            let mut levels = vec![0u8; width * geometry.height as usize];
            overlay_text(&mut levels, geometry, "HI", &font, 10.0, OverlayStyle { offset, ..plain(OverlayColor::White) }).unwrap();
            let index = levels.iter().position(|&level| level > 0).unwrap();
            (index % width, index / width)
        };
        let (x, y) = first_lit((0, 0));
        assert_eq!(first_lit((5, -3)), (x + 5, y - 3));
    }

    #[test]
    fn overlay_text_turns_about_its_middle() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let (font, width) = (default_font_path(), geometry.width as usize);
        let draw = |style: OverlayStyle| {
            let mut levels = vec![0u8; width * geometry.height as usize];
            overlay_text(&mut levels, geometry, "Hi", &font, 10.0, style).unwrap();
            levels
        };
        let upright = draw(plain(OverlayColor::White));
        let turned = draw(OverlayStyle { rotation: 180.0, ..plain(OverlayColor::White) });
        assert_ne!(upright, turned);
        // Every lit pixel lands on its partner across the safe area's middle.
        let area = geometry.safe_content_area();
        let (cx, cy) = ((2 * area.x + area.width - 1) as usize, (2 * area.y + area.height - 1) as usize);
        for (index, &level) in upright.iter().enumerate().filter(|(_, level)| **level > 0) {
            let (x, y) = (index % width, index / width);
            assert_eq!(turned[(cy - y) * width + cx - x], level);
        }
        // Without smoothing an odd angle stays fully on or off.
        let crisp = draw(OverlayStyle { rotation: 30.0, smooth: false, ..plain(OverlayColor::White) });
        assert!(crisp.iter().all(|&level| level == 0 || level == 255) && crisp.contains(&255));
    }

    #[test]
    fn panel_overlay_sits_on_top_of_the_finished_frame() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let font = default_font_path();
        let frame = crate::matrix::level_buffer(&vec![100u8; (geometry.width * geometry.height) as usize], 1.0, geometry);
        let draw = |color| {
            let mut leds = frame.clone();
            overlay_text_on_panel(&mut leds, geometry, "Hi", &font, 10.0, OverlayStyle { smooth: false, ..plain(color) }).unwrap();
            leds
        };
        // White text at full brightness over the dimmer frame, the rest untouched.
        let white = draw(OverlayColor::White);
        assert_eq!(white.len(), frame.len());
        assert!(white.contains(&255));
        assert!(white.iter().zip(&frame).filter(|(after, before)| after != before).count() < frame.len() / 4);
        // Black text cuts LEDs out of it.
        assert!(draw(OverlayColor::Black).contains(&0));
    }

    #[test]
    fn overlay_text_animates_like_a_text_element() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let font = default_font_path();
        let draw = |mode, seconds| {
            let mut levels = vec![0u8; (geometry.width * geometry.height) as usize];
            let animation = OverlayAnimation { mode, elapsed: Duration::from_secs_f32(seconds), ..OverlayAnimation::default() };
            overlay_text(&mut levels, geometry, "Hi", &font, 10.0, OverlayStyle { animation, ..plain(OverlayColor::White) }).unwrap();
            levels
        };
        assert_eq!(draw(TextMode::Static, 0.0), draw(TextMode::Static, 2.0));
        assert_ne!(draw(TextMode::Scroll, 0.5), draw(TextMode::Scroll, 1.5));
        // Blinking: shown for the first half of each period, gone in the second.
        assert!(draw(TextMode::Blink, 0.1).iter().any(|&level| level > 0));
        assert!(draw(TextMode::Blink, 0.6).iter().all(|&level| level == 0));
    }

    #[test]
    fn overlay_text_takes_several_lines() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let font = default_font_path();
        let rows = |text: &str| {
            let mut levels = vec![0u8; (geometry.width * geometry.height) as usize];
            overlay_text(&mut levels, geometry, text, &font, 12.0, plain(OverlayColor::White)).unwrap();
            let width = geometry.width as usize;
            levels.chunks(width).filter(|row| row.iter().any(|&level| level > 0)).count()
        };
        assert!(rows("HI\nYO") > rows("HI") + 3);
    }

    #[test]
    fn font_sizes_below_six_are_used_as_set() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let now = Local.with_ymd_and_hms(2026, 9, 30, 12, 34, 56).unwrap();
        let lit = |size: f32, date: bool| {
            let mut clock = Element::clock();
            if let ElementKind::Clock { font_size, show_date, .. } = &mut clock.kind {
                (*font_size, *show_date) = (size, date);
            }
            let image = MatrixRenderer::render(&clock, now, Duration::ZERO, geometry).unwrap();
            image.pixels().filter(|pixel| pixel.0 != [0, 0, 0]).count()
        };
        assert!(lit(4.0, false) > 0);
        assert!(lit(4.0, false) < lit(6.0, false));
        // The date line stays below the time's size instead of jumping to 6.
        assert!(lit(4.0, true) < lit(6.0, true));
    }

    #[test]
    fn infinite_scroll_never_leaves_the_panel_empty_once_started() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        for direction in ScrollDirection::ALL {
            let frame_lit = |mode, seconds: f64| {
                let mut element = Element::text();
                if let ElementKind::Text { text, mode: m, direction: d, speed, scroll_pause, .. } = &mut element.kind {
                    (*text, *m, *d, *speed, *scroll_pause) = ("Hello".into(), mode, direction, 40.0, 0.0);
                }
                let image = MatrixRenderer::render(&element, Local::now(), Duration::from_secs_f64(seconds), geometry).unwrap();
                image.pixels().any(|pixel| pixel.0 != [0, 0, 0])
            };
            // The first copy comes in exactly as a plain scroll's does: from
            // an empty panel (going down, the top of the font box peeks in
            // for both, as text is measured by its ink).
            assert_eq!(frame_lit(TextMode::InfiniteScroll, 0.0), frame_lit(TextMode::Scroll, 0.0), "{direction:?}");
            if direction == ScrollDirection::Left {
                assert!(!frame_lit(TextMode::InfiniteScroll, 0.0));
            }
            // Plain scroll empties the panel between passes; infinite scroll
            // never does once going, also days later.
            let after = |start: f64| (0..120).map(move |frame| start + frame as f64 / 30.0);
            assert!(after(2.0).any(|seconds| !frame_lit(TextMode::Scroll, seconds)), "{direction:?}");
            for start in [2.0, 9.0 * 24.0 * 3600.0] {
                assert!(after(start).all(|seconds| frame_lit(TextMode::InfiniteScroll, seconds)), "{direction:?} {start}");
            }
        }
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
                // Infinite scroll repeats only once the first copy has crossed.
                let warm_up = if mode == TextMode::InfiniteScroll { 10.0 } else { 0.0 };
                for start in [0.13 + warm_up, 0.61 + warm_up] {
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
    fn battery_scales_about_its_middle() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let span = |factor: f32| {
            let mut battery = Element::battery();
            if let ElementKind::Battery { scale, .. } = &mut battery.kind {
                *scale = factor;
            }
            let image = MatrixRenderer::render(&battery, Local::now(), Duration::ZERO, geometry).unwrap();
            let columns: Vec<u32> = image.enumerate_pixels().filter(|(_, _, pixel)| pixel.0 != [0, 0, 0]).map(|(x, _, _)| x).collect();
            let (left, right) = (*columns.iter().min().unwrap(), *columns.iter().max().unwrap());
            (right - left + 1, left + right)
        };
        let (normal, middle) = span(1.0);
        let (double, double_middle) = span(2.0);
        let (half, half_middle) = span(0.5);
        assert!(double > normal * 3 / 2 && half < normal * 2 / 3, "{half} {normal} {double}");
        // Grown and shrunk about the middle of its area (fixed, so it does not
        // jump as the percentage changes width); the gauge sits a pixel or two
        // off that middle, which doubling doubles.
        for other in [double_middle, half_middle] {
            assert!(other.abs_diff(middle) <= 8, "{middle} vs {other}");
        }
    }

    #[test]
    fn every_battery_style_and_charge_stays_inside_the_safe_area() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let area = geometry.safe_content_area();
        let font = Element::battery();
        let ElementKind::Battery { font, font_size, .. } = font.kind else { unreachable!() };
        for style in BatteryStyle::ALL {
            for state in [None, Some((5, true)), Some((42, false)), Some((99, true)), Some((100, false)), Some((100, true))] {
                let state = state.map(|(percent, charging)| BatteryState { percent, charging });
                for label in ["", "Battery"] {
                    let image = render_battery(&font, font_size, style, label, state, Duration::from_secs(3), geometry, area, true).unwrap();
                    let leaks: Vec<_> = image.enumerate_pixels()
                        .filter(|(x, y, pixel)| pixel.0 != [0, 0, 0]
                            && !((area.x..area.x + area.width).contains(x) && (area.y..area.y + area.height).contains(y)))
                        .map(|(x, y, _)| (x, y))
                        .collect();
                    assert!(leaks.is_empty(), "{style:?} {state:?} {label:?}: {leaks:?}");
                }
            }
        }
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
                let start = if mode == TextMode::InfiniteScroll { 10.13 } else { 0.13 };
                assert!(at(start) == at(start + cycle.as_secs_f64()), "{mode:?} {direction:?} cycle is off");
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
            geometry.safe_content_area(), true).unwrap()
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
        OverlayStyle {
            color, bold: false, italic: false, outline: 0, smooth: true, offset: (0, 0), rotation: 0.0, opacity: 1.0,
            animation: OverlayAnimation::default(),
        }
    }

    /// Overlay drawn onto a canvas of LED levels, for checking its shape.
    fn overlay_text(levels: &mut [u8], geometry: MatrixGeometry, text: &str, font: &Path, size: f32, style: OverlayStyle) -> Result<()> {
        let (mask, ring) = overlay_masks(geometry, Stage::exact(geometry), text, font, size, style)?;
        apply_overlay(levels, &mask, ring.as_deref(), style.color);
        Ok(())
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
                geometry.safe_content_area(), true).unwrap()
                .pixels().filter(|pixel| pixel.0 != [0, 0, 0]).count()
        };
        assert!(lit(90) > lit(20));
    }

    #[test]
    fn ignoring_the_safe_area_scales_text_to_fill_the_canvas() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let now = Local.with_ymd_and_hms(2026, 9, 30, 12, 34, 56).unwrap();
        let lit_span = |element: &Element| {
            let image = MatrixRenderer::render(element, now, Duration::ZERO, geometry).unwrap();
            let lit: Vec<_> = image.enumerate_pixels().filter(|(_, _, pixel)| pixel.0 != [0, 0, 0]).map(|(x, y, _)| (x, y)).collect();
            let columns = lit.iter().map(|p| p.0).max().unwrap() - lit.iter().map(|p| p.0).min().unwrap() + 1;
            let rows = lit.iter().map(|p| p.1).max().unwrap() - lit.iter().map(|p| p.1).min().unwrap() + 1;
            (image, columns, rows)
        };
        for kind in [Element::clock(), Element::text()] {
            let sized = |size: f32, ignore: bool| {
                let mut element = kind.clone();
                match &mut element.kind {
                    ElementKind::Clock { font_size, ignore_safe_area, .. } => (*font_size, *ignore_safe_area) = (size, ignore),
                    ElementKind::Text { font_size, ignore_safe_area, mode, text, .. } => {
                        (*font_size, *ignore_safe_area, *mode, *text) = (size, ignore, TextMode::Static, "Hi".into());
                    }
                    _ => unreachable!(),
                }
                lit_span(&element)
            };
            // Filling the canvas: the chosen size does not matter, and the
            // text reaches nearly across or down it.
            let (small, columns, rows) = sized(6.0, true);
            assert_eq!(small, sized(30.0, true).0, "{}", kind.kind.label());
            assert!(columns > geometry.width * 3 / 4 || rows > geometry.height * 3 / 4, "{} {columns}x{rows}", kind.kind.label());
            // On the safe area the chosen size is used as is.
            assert!(sized(6.0, false).1 < sized(12.0, false).1, "{}", kind.kind.label());
        }
    }

    #[test]
    fn clock_shows_one_to_three_millisecond_digits() {
        let now = Local.with_ymd_and_hms(2026, 9, 30, 13, 4, 5).unwrap() + chrono::Duration::milliseconds(87);
        assert_eq!(clock_text(now, true, false, None), "13:04");
        assert_eq!(clock_text(now, true, true, None), "13:04:05");
        assert_eq!(clock_text(now, true, false, Some(3)), "13:04:05.087");
        assert_eq!(clock_text(now, true, false, Some(2)), "13:04:05.08");
        assert_eq!(clock_text(now, true, false, Some(1)), "13:04:05.0");
        assert_eq!(clock_text(now, false, false, Some(2)), "01:04:05.08 PM");
        assert_eq!(clock_text(now, false, false, None), "01:04 PM");
    }

    #[test]
    fn the_date_can_go_above_the_time() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let now = Local.with_ymd_and_hms(2026, 9, 30, 12, 34, 56).unwrap();
        // Height of the first band of lit rows from the top.
        let top_line = |date_first: bool| {
            let mut clock = Element::clock();
            if let ElementKind::Clock { show_date, date_first: first, font_size, .. } = &mut clock.kind {
                (*show_date, *first, *font_size) = (true, date_first, 12.0);
            }
            let image = MatrixRenderer::render(&clock, now, Duration::ZERO, geometry).unwrap();
            let lit = |y: u32| (0..geometry.width).any(|x| image.get_pixel(x, y).0 != [0, 0, 0]);
            let top = (0..geometry.height).find(|&y| lit(y)).unwrap();
            (top..geometry.height).take_while(|&y| lit(y)).count()
        };
        // The date is drawn smaller than the time, so it makes the shorter top line.
        assert!(top_line(true) < top_line(false));
    }

    #[test]
    fn clocks_keep_their_size_centred_on_the_safe_area() {
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
            let columns: Vec<u32> = image.enumerate_pixels().filter(|(_, _, pixel)| pixel.0 != [0, 0, 0]).map(|(x, _, _)| x).collect();
            let (left, right) = (*columns.iter().min().unwrap(), *columns.iter().max().unwrap());
            // Not shrunk to the safe area: the long time runs past it...
            assert!(left < area.x && right >= area.x + area.width, "millis={millis} {left}..={right}");
            // ...centred on it all the same, unless wider than the canvas.
            if left > 0 && right + 1 < geometry.width {
                let middle = area.x as i32 * 2 + area.width as i32 - 1;
                assert!((left as i32 + right as i32 - middle).abs() <= 3, "millis={millis} {left}..={right}");
            }
        }
    }
}
