//! Decoded pixel-GIF animations, played by the engine instead of
//! `asusctl anime pixel-gif` so the frame rate can be overridden.

use std::fs::File;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::model::{GifLayout, GifLoop, MatrixGeometry};

/// Frames shorter than this (including GIFs with no delay) are stretched.
const MIN_FRAME: Duration = Duration::from_millis(20);

pub struct GifAnimation {
    /// The GIF's own canvas size.
    width: usize,
    height: usize,
    /// Per-frame LED levels at the GIF's own size, row-major.
    frames: Vec<Vec<u8>>,
    delays: Vec<Duration>,
    total: Duration,
}

impl GifAnimation {
    /// Decodes like `rog_anime::AnimeGif::from_diagonal_gif`: each frame's
    /// opaque pixels are painted over the previous frame, and the red
    /// channel is the LED level. Unlike it, frame disposal is honoured, so
    /// areas a transparent GIF clears ("background"/"previous") turn off again
    /// instead of keeping stale pixels. Frames keep the GIF's own size; see
    /// [`Self::placed`] and [`Self::sprite`] for fitting them to the panel.
    /// Loads the GIF an element refers to, falling back to the bundled GIFs
    /// when `path` is empty or missing (see [`crate::assets::resolve_gif`]).
    pub fn open(path: &Path) -> Result<Self> {
        let resolved = crate::assets::resolve_gif(path).with_context(|| {
            format!("GIF '{}' not found and no bundled GIFs are installed", path.display())
        })?;
        Self::load(&resolved)
    }

    /// Where [`Self::placed_on`] puts the GIF on `canvas`: left, top,
    /// width, height (it may reach past the edges).
    pub fn placed_rect(&self, layout: GifLayout, geometry: MatrixGeometry, canvas: MatrixGeometry, scale: f32) -> (i64, i64, usize, usize) {
        let (panel_w, panel_h) = (geometry.width as usize, geometry.height as usize);
        let (w, h) = match layout {
            GifLayout::AsIs | GifLayout::Animate => (self.width, self.height),
            GifLayout::Stretch => (panel_w, panel_h),
            GifLayout::Fit => {
                let scale = (panel_w as f64 / self.width as f64).min(panel_h as f64 / self.height as f64);
                let fit = |size: usize| ((size as f64 * scale).round() as usize).max(1);
                (fit(self.width), fit(self.height))
            }
        };
        let scaled = |size: usize| ((size as f64 * f64::from(scale)).round() as usize).max(1);
        let (w, h) = (scaled(w), scaled(h));
        ((canvas.width as i64 - w as i64) / 2, (canvas.height as i64 - h as i64) / 2, w, h)
    }

    /// Loads a GIF, or a PNG or JPEG as a single still frame.
    pub fn load(path: &Path) -> Result<Self> {
        let mut file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
        // By content, not name: a renamed file still opens.
        let mut magic = [0u8; 4];
        let is_gif = std::io::Read::read_exact(&mut file, &mut magic).is_ok() && magic[..3] == *b"GIF";
        if !is_gif {
            return Self::load_still(path);
        }
        let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
        let mut options = gif::DecodeOptions::new();
        options.set_color_output(gif::ColorOutput::RGBA);
        let mut decoder = options.read_info(file)
            .with_context(|| format!("{} is not a readable GIF", path.display()))?;

        let (width, height) = (usize::from(decoder.width()), usize::from(decoder.height()));
        anyhow::ensure!(width > 0 && height > 0, "{} has an empty canvas", path.display());
        let mut canvas = vec![0u8; width * height];
        let mut frames = Vec::new();
        let mut delays = Vec::new();
        // What the previous frame asked to happen to its area once shown.
        let mut disposal: Option<Disposal> = None;
        while let Some(frame) = decoder.read_next_frame()
            .with_context(|| format!("failed to decode {}", path.display()))?
        {
            match disposal.take() {
                // Its area becomes transparent again: LEDs off.
                Some(Disposal::Clear { left, top, w, h }) => {
                    for y in top..(top + h).min(height) {
                        let row = y * width;
                        canvas[row + left.min(width)..row + (left + w).min(width)].fill(0);
                    }
                }
                // Back to how the canvas looked before it was drawn.
                Some(Disposal::Restore(saved)) => canvas = saved,
                None => {}
            }
            let (left, top) = (usize::from(frame.left), usize::from(frame.top));
            let (w, h) = (usize::from(frame.width), usize::from(frame.height));
            disposal = match frame.dispose {
                gif::DisposalMethod::Background => Some(Disposal::Clear { left, top, w, h }),
                gif::DisposalMethod::Previous => Some(Disposal::Restore(canvas.clone())),
                gif::DisposalMethod::Keep | gif::DisposalMethod::Any => None,
            };
            for (row, pixels) in frame.buffer.chunks(usize::from(frame.width) * 4).enumerate() {
                let y = row + usize::from(frame.top);
                for (column, px) in pixels.chunks_exact(4).enumerate() {
                    let x = column + usize::from(frame.left);
                    if px[3] == 255 && x < width && y < height {
                        canvas[y * width + x] = px[0];
                    }
                }
            }
            frames.push(canvas.clone());
            delays.push(Duration::from_millis(u64::from(frame.delay) * 10).max(MIN_FRAME));
        }
        anyhow::ensure!(!frames.is_empty(), "{} has no frames", path.display());
        let total = delays.iter().sum();
        Ok(Self { width, height, frames, delays, total })
    }

    fn frame(&self, index: usize) -> &[u8] {
        &self.frames[index % self.frames.len()]
    }

    /// Frame `index` on the pixel-image canvas, row-major, laid out per
    /// `layout` and centred. `Animate` is placed like `AsIs`; use
    /// [`Self::sprite`] for it.
    pub fn placed(&self, index: usize, layout: GifLayout, geometry: MatrixGeometry) -> Vec<u8> {
        self.placed_on(index, layout, geometry, geometry, true, 1.0)
    }

    /// [`Self::placed`], sized for the panel `geometry` but centred on the
    /// larger `canvas` around it, so nothing is cropped at the panel's edge.
    /// `smooth` blends pixels when scaling; off picks the nearest one.
    /// `scale` multiplies the size the layout picks.
    pub fn placed_on(
        &self,
        index: usize,
        layout: GifLayout,
        geometry: MatrixGeometry,
        canvas: MatrixGeometry,
        smooth: bool,
        scale: f32,
    ) -> Vec<u8> {
        let (canvas_w, canvas_h) = (canvas.width as usize, canvas.height as usize);
        let (left, top, w, h) = self.placed_rect(layout, geometry, canvas, scale);
        let frame = resample(self.frame(index), self.width, self.height, w, h, smooth);
        let mut canvas = vec![0u8; canvas_w * canvas_h];
        for (y, row) in frame.chunks(w).enumerate() {
            let cy = top + y as i64;
            if !(0..canvas_h as i64).contains(&cy) {
                continue;
            }
            for (x, &level) in row.iter().enumerate() {
                let cx = left + x as i64;
                if (0..canvas_w as i64).contains(&cx) {
                    canvas[cy as usize * canvas_w + cx as usize] = level;
                }
            }
        }
        canvas
    }

    /// Size of [`Self::sprite`] for a given height, keeping the aspect ratio.
    pub fn sprite_size(&self, height: u32) -> (u32, u32) {
        let width = (self.width as f64 * f64::from(height) / self.height as f64).round().max(1.0);
        (width as u32, height.max(1))
    }

    /// Frame `index` scaled to `height` pixels tall, for `GifLayout::Animate`.
    pub fn sprite(&self, index: usize, height: u32, smooth: bool) -> Sprite {
        let (width, height) = self.sprite_size(height);
        let levels = resample(self.frame(index), self.width, self.height, width as usize, height as usize, smooth);
        Sprite { width, height, levels }
    }

    /// The frame to show `elapsed` into looping playback. `fps` > 0 replaces
    /// the GIF's own timing with a fixed rate.
    pub fn index_at(&self, elapsed: Duration, fps: f32, looping: GifLoop) -> usize {
        let steps = self.steps(looping);
        if fps > 0.0 {
            // The nudge keeps a wake-up a hair short of a boundary in the
            // frame it was meant for, matching the engine's frame grid.
            return self.step_frame((elapsed.as_secs_f64() * f64::from(fps) + 1e-6) as usize % steps);
        }
        let mut remaining = elapsed.as_nanos() % self.cycle_duration(0.0, looping).as_nanos().max(1);
        for step in 0..steps {
            let delay = self.delays[self.step_frame(step)].as_nanos();
            if remaining < delay {
                return self.step_frame(step);
            }
            remaining -= delay;
        }
        self.step_frame(steps - 1)
    }

    /// Frames shown per loop: forward only, or forward then back without
    /// repeating the first and last frame.
    fn steps(&self, looping: GifLoop) -> usize {
        let frames = self.frames.len();
        match looping {
            GifLoop::Bounce if frames > 2 => 2 * frames - 2,
            _ => frames,
        }
    }

    /// The frame shown at `step` of a loop (see [`Self::steps`]).
    fn step_frame(&self, step: usize) -> usize {
        let frames = self.frames.len();
        if step < frames { step } else { 2 * frames - 2 - step }
    }

    /// One full play of the animation.
    pub fn cycle_duration(&self, fps: f32, looping: GifLoop) -> Duration {
        let steps = self.steps(looping);
        if fps > 0.0 {
            Duration::from_secs_f64(steps as f64 / f64::from(fps))
        } else if steps == self.frames.len() {
            self.total
        } else {
            (0..steps).map(|step| self.delays[self.step_frame(step)]).sum()
        }
    }

    /// How often a GIF's overlay animation steps: at the FPS override when
    /// set, otherwise as fast as the GIF's quickest frame, at most 30 times
    /// a second.
    pub fn overlay_fps(&self, fps: f32) -> f64 {
        if fps > 0.0 {
            return f64::from(fps);
        }
        let quickest = self.delays.iter().copied().min().unwrap_or(MIN_FRAME);
        (1.0 / quickest.as_secs_f64()).min(f64::from(crate::model::MAX_FPS))
    }

    /// A PNG or JPEG: its lightness is the LED brightness and transparent
    /// areas stay dark.
    fn load_still(path: &Path) -> Result<Self> {
        let unreadable = || format!("{} is not a readable GIF, PNG, or JPEG", path.display());
        // By content, like GIFs: the extension may say otherwise.
        let picture = image::ImageReader::open(path)
            .with_context(|| format!("failed to open {}", path.display()))?
            .with_guessed_format()
            .with_context(unreadable)?
            .decode()
            .with_context(unreadable)?
            .to_luma_alpha8();
        let (width, height) = (picture.width() as usize, picture.height() as usize);
        anyhow::ensure!(width > 0 && height > 0, "{} is empty", path.display());
        let levels = picture.pixels().map(|pixel| (u16::from(pixel.0[0]) * u16::from(pixel.0[1]) / 255) as u8).collect();
        Ok(Self { width, height, frames: vec![levels], delays: vec![MIN_FRAME], total: MIN_FRAME })
    }

    /// Whether there is more than one frame to show.
    pub fn is_animated(&self) -> bool {
        self.frames.len() > 1
    }

    /// Time from `elapsed` until the next frame starts, plus a moment so the
    /// wake-up lands inside it. Measured from the GIF's own timeline, so
    /// frames keep their timing however long drawing the last one took.
    pub fn until_next_frame(&self, elapsed: Duration, fps: f32, looping: GifLoop) -> Duration {
        const SETTLE: Duration = Duration::from_millis(1);
        if fps > 0.0 {
            let fps = f64::from(fps);
            let next = ((elapsed.as_secs_f64() * fps + 1e-6).floor() + 1.0) / fps;
            return Duration::from_secs_f64(next).saturating_sub(elapsed) + SETTLE;
        }
        let cycle = self.cycle_duration(0.0, looping).as_nanos().max(1);
        let into = elapsed.as_nanos() % cycle;
        let mut boundary = 0;
        for step in 0..self.steps(looping) {
            boundary += self.delays[self.step_frame(step)].as_nanos();
            if boundary > into {
                break;
            }
        }
        // The steps add up to the cycle, so a boundary past `into` is found.
        Duration::from_nanos(u64::try_from(boundary - into).unwrap_or(u64::MAX)) + SETTLE
    }
}

/// Tone-maps LED levels. First the black level: everything at or below
/// `black_level` percent turns off and the rest is stretched back to the full
/// range. Then a contrast S-curve, where 1 is the identity. 0 and 255 are
/// fixed points of both, so unlit LEDs around a GIF stay unlit.
pub fn apply_tone(levels: &mut [u8], black_level: f32, contrast: f32) {
    if black_level <= 0.0 && (contrast - 1.0).abs() < f32::EPSILON {
        return;
    }
    let black = (black_level.clamp(0.0, 99.0) / 100.0).max(0.0);
    let table: Vec<u8> = (0..=255u8)
        .map(|level| {
            let x = ((f32::from(level) / 255.0 - black) / (1.0 - black)).max(0.0);
            let (rise, fall) = (x.powf(contrast), (1.0 - x).powf(contrast));
            (255.0 * rise / (rise + fall)).round() as u8
        })
        .collect();
    for level in levels {
        *level = table[usize::from(*level)];
    }
}

/// How a GIF frame's area is treated before the next frame is drawn.
enum Disposal {
    /// Cleared to transparent (off).
    Clear { left: usize, top: usize, w: usize, h: usize },
    /// Restored to the saved canvas.
    Restore(Vec<u8>),
}

/// A GIF frame used like a text character: row-major LED levels.
pub struct Sprite {
    pub width: u32,
    pub height: u32,
    pub levels: Vec<u8>,
}

/// Rescales row-major levels. Each target pixel averages the source pixels it
/// covers, so shrinking keeps detail and enlarging stays crisp (pixel art).
/// Scales a row-major frame. `smooth` averages the source pixels each
/// target pixel covers; otherwise each takes the source pixel at its middle.
fn resample(source: &[u8], from_w: usize, from_h: usize, to_w: usize, to_h: usize, smooth: bool) -> Vec<u8> {
    if (from_w, from_h) == (to_w, to_h) {
        return source.to_vec();
    }
    if !smooth {
        let middle = |target: usize, from: usize, to: usize| ((2 * target + 1) * from / (2 * to)).min(from - 1);
        return (0..to_h)
            .flat_map(|y| (0..to_w).map(move |x| (x, y)))
            .map(|(x, y)| source[middle(y, from_h, to_h) * from_w + middle(x, from_w, to_w)])
            .collect();
    }
    let span = |target: usize, from: usize, to: usize| {
        let start = target * from / to;
        (start, ((target + 1) * from / to).max(start + 1).min(from))
    };
    let mut out = Vec::with_capacity(to_w * to_h);
    for y in 0..to_h {
        let (y0, y1) = span(y, from_h, to_h);
        for x in 0..to_w {
            let (x0, x1) = span(x, from_w, to_w);
            let mut sum = 0u32;
            for sy in y0..y1 {
                sum += source[sy * from_w + x0..sy * from_w + x1].iter().map(|&level| u32::from(level)).sum::<u32>();
            }
            out.push((sum / ((y1 - y0) * (x1 - x0)) as u32) as u8);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn animation(delays_ms: &[u64]) -> GifAnimation {
        let delays: Vec<_> = delays_ms.iter().map(|&ms| Duration::from_millis(ms)).collect();
        GifAnimation {
            width: 1,
            height: 1,
            frames: vec![vec![0]; delays.len()],
            total: delays.iter().sum(),
            delays,
        }
    }

    #[test]
    fn native_timing_follows_frame_delays_and_loops() {
        let gif = animation(&[100, 300]);
        assert_eq!(gif.index_at(Duration::from_millis(50), 0.0, GifLoop::Restart), 0);
        assert_eq!(gif.index_at(Duration::from_millis(150), 0.0, GifLoop::Restart), 1);
        assert_eq!(gif.index_at(Duration::from_millis(450), 0.0, GifLoop::Restart), 0);
    }

    #[test]
    fn next_frame_is_timed_from_the_gif_timeline() {
        let ms = Duration::from_millis;
        let gif = animation(&[100, 300, 50]);
        // Own delays: frames start at 0, 100, 400 and the loop restarts at 450.
        assert_eq!(gif.until_next_frame(ms(30), 0.0, GifLoop::Restart), ms(71));
        assert_eq!(gif.until_next_frame(ms(250), 0.0, GifLoop::Restart), ms(151));
        assert_eq!(gif.until_next_frame(ms(420), 0.0, GifLoop::Restart), ms(31));
        assert_eq!(gif.until_next_frame(ms(450 * 1000 + 30), 0.0, GifLoop::Restart), ms(71));
        // FPS override: a fixed grid, and the wake-up lands in the next frame.
        assert_eq!(gif.until_next_frame(ms(130), 10.0, GifLoop::Restart), ms(71));
        let woke = ms(130) + gif.until_next_frame(ms(130), 10.0, GifLoop::Restart);
        assert_eq!(gif.index_at(woke, 10.0, GifLoop::Restart), 2);
    }

    #[test]
    fn one_cycle_uses_delays_or_the_fps_override() {
        let gif = animation(&[100, 300, 500]);
        assert_eq!(gif.cycle_duration(0.0, GifLoop::Restart), Duration::from_millis(900));
        assert_eq!(gif.cycle_duration(10.0, GifLoop::Restart), Duration::from_millis(300));
        // Bounce: 0 1 2 1 -> the middle frame plays twice per loop.
        assert_eq!(gif.cycle_duration(0.0, GifLoop::Bounce), Duration::from_millis(1200));
        assert_eq!(gif.cycle_duration(10.0, GifLoop::Bounce), Duration::from_millis(400));
    }

    #[test]
    fn bounce_plays_forward_then_back_without_repeating_ends() {
        let gif = animation(&[100, 100, 100, 100]);
        let order: Vec<_> = (0..8).map(|step| gif.index_at(Duration::from_millis(step * 100 + 50), 0.0, GifLoop::Bounce)).collect();
        assert_eq!(order, [0, 1, 2, 3, 2, 1, 0, 1]);
        let fixed: Vec<_> = (0..7).map(|step| gif.index_at(Duration::from_millis(step * 100 + 50), 10.0, GifLoop::Bounce)).collect();
        assert_eq!(fixed, [0, 1, 2, 3, 2, 1, 0]);
    }

    /// A single-frame 2x1 GIF: left pixel 200, right pixel 100.
    fn two_pixels() -> GifAnimation {
        GifAnimation {
            width: 2,
            height: 1,
            frames: vec![vec![200, 100]],
            delays: vec![MIN_FRAME],
            total: MIN_FRAME,
        }
    }

    #[test]
    fn pngs_and_jpegs_load_as_one_still_frame() {
        let directory = tempfile::tempdir().unwrap();
        // White, mid grey, and white but half transparent.
        let png = directory.path().join("picture.png");
        image::RgbaImage::from_vec(3, 1, vec![255, 255, 255, 255, 128, 128, 128, 255, 255, 255, 255, 128])
            .unwrap().save(&png).unwrap();
        let still = GifAnimation::load(&png).unwrap();
        assert_eq!((still.width, still.height, still.frames.len()), (3, 1, 1));
        assert!(!still.is_animated());
        assert_eq!(still.frames[0], [255, 128, 128]);

        // Found by content, whatever the extension says.
        let jpeg = directory.path().join("photo.gif");
        image::RgbImage::from_pixel(4, 4, image::Rgb([200, 200, 200]))
            .save_with_format(&jpeg, image::ImageFormat::Jpeg).unwrap();
        let photo = GifAnimation::load(&jpeg).unwrap();
        assert_eq!((photo.width, photo.height), (4, 4));
        assert!(photo.frames[0].iter().all(|&level| level.abs_diff(200) < 6));
    }

    #[test]
    fn scale_multiplies_the_layouts_size_and_stays_centred() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let (w, h) = (geometry.width as usize, geometry.height as usize);
        let gif = two_pixels();
        let lit = |layout, scale| {
            let canvas = gif.placed_on(0, layout, geometry, geometry, false, scale);
            let spots: Vec<_> = (0..w * h).filter(|&i| canvas[i] > 0).map(|i| (i % w, i / w)).collect();
            (spots.len(), *spots.first().unwrap())
        };
        // 2x1 pixels at x4 -> 8x4, still centred.
        assert_eq!(lit(GifLayout::AsIs, 4.0), (8 * 4, (33, 17)));
        // Fit (74x37) at half size -> 37x19 (rounded).
        assert_eq!(lit(GifLayout::Fit, 0.5).0, 37 * 19);
        // Nearest-pixel scaling keeps the two source levels, nothing blended.
        let canvas = gif.placed_on(0, GifLayout::AsIs, geometry, geometry, false, 4.0);
        assert!(canvas.iter().all(|&level| [0, 100, 200].contains(&level)));
    }

    #[test]
    fn layouts_place_centre_fit_and_stretch() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let (w, h) = (geometry.width as usize, geometry.height as usize);
        let gif = two_pixels();
        let lit = |layout| {
            let canvas = gif.placed(0, layout, geometry);
            let spots: Vec<_> = (0..w * h).filter(|&i| canvas[i] > 0).map(|i| (i % w, i / w)).collect();
            (spots.len(), *spots.first().unwrap())
        };
        // 1:1 in the middle of the 74x39 canvas.
        assert_eq!(lit(GifLayout::AsIs), (2, (36, 19)));
        // 2:1 aspect fitted to 74 wide -> 74x37, centred vertically.
        assert_eq!(lit(GifLayout::Fit), (74 * 37, (0, 1)));
        assert_eq!(lit(GifLayout::Stretch), (w * h, (0, 0)));
    }

    #[test]
    fn sprites_keep_aspect_and_average_when_shrinking() {
        let gif = two_pixels();
        let big = gif.sprite(0, 4, true);
        assert_eq!((big.width, big.height), (8, 4));
        assert_eq!(&big.levels[..8], &[200, 200, 200, 200, 100, 100, 100, 100]);
        assert_eq!(resample(&[200, 100], 2, 1, 1, 1, true), [150]);
        assert_eq!(resample(&[200, 100, 50, 25], 4, 1, 2, 1, false), [100, 25]);
    }

    #[test]
    fn missing_gifs_fall_back_to_the_bundled_default() {
        // Development builds find the bundled GIFs in data/gifs.
        let bundled = GifAnimation::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("data/gifs")
            .join(crate::assets::DEFAULT_GIF)).unwrap();
        for path in ["", "/nowhere/missing.gif", crate::assets::DEFAULT_GIF] {
            let opened = GifAnimation::open(Path::new(path)).unwrap();
            assert_eq!(opened.frames, bundled.frames, "{path:?}");
        }
    }

    #[test]
    fn contrast_keeps_black_and_white_and_bends_the_middle() {
        let original = [0u8, 64, 128, 192, 255];
        let mut sharper = original;
        apply_tone(&mut sharper, 0.0, 2.0);
        assert_eq!((sharper[0], sharper[2], sharper[4]), (0, 128, 255));
        assert!(sharper[1] < 64 && sharper[3] > 192);
        let mut flatter = original;
        apply_tone(&mut flatter, 0.0, 0.5);
        assert_eq!((flatter[0], flatter[4]), (0, 255));
        assert!(flatter[1] > 64 && flatter[3] < 192);
        let mut same = original;
        apply_tone(&mut same, 0.0, 1.0);
        assert_eq!(same, original);
    }

    #[test]
    fn black_level_cuts_haze_and_restretches_the_rest() {
        let mut levels = [0u8, 20, 51, 60, 153, 255];
        apply_tone(&mut levels, 20.0, 1.0);
        // 20 % of 255 = 51: that and anything darker turns off.
        assert_eq!(&levels[..3], &[0, 0, 0]);
        assert!(levels[3] > 0 && levels[3] < 20);
        assert_eq!(levels[4], 128);
        assert_eq!(levels[5], 255);
    }

    /// Writes a 4x1 GIF: frame 1 lights everything, frame 2 only pixel 0
    /// (the rest transparent). Frame 1 uses `dispose`.
    fn two_frame_gif(dispose: gif::DisposalMethod) -> tempfile::NamedTempFile {
        let file = tempfile::Builder::new().suffix(".gif").tempfile().unwrap();
        let palette = [0, 0, 0, 255, 255, 255];
        let mut encoder = gif::Encoder::new(file.reopen().unwrap(), 4, 1, &palette).unwrap();
        let mut first = gif::Frame::from_indexed_pixels(4, 1, vec![1, 1, 1, 1], None);
        first.dispose = dispose;
        encoder.write_frame(&first).unwrap();
        // Index 0 is transparent in the second frame.
        let second = gif::Frame::from_indexed_pixels(4, 1, vec![1, 0, 0, 0], Some(0));
        encoder.write_frame(&second).unwrap();
        drop(encoder);
        file
    }

    #[test]
    fn disposal_turns_transparent_areas_off_again() {
        let second_frame = |dispose| {
            let file = two_frame_gif(dispose);
            GifAnimation::load(file.path()).unwrap().frames[1].clone()
        };
        // "Keep" (ASUS GIFs, asusctl behaviour): transparent pixels stay lit.
        assert_eq!(second_frame(gif::DisposalMethod::Keep), [255, 255, 255, 255]);
        // "Background": the first frame's area is cleared first.
        assert_eq!(second_frame(gif::DisposalMethod::Background), [255, 0, 0, 0]);
        // "Previous": back to the empty canvas from before the first frame.
        assert_eq!(second_frame(gif::DisposalMethod::Previous), [255, 0, 0, 0]);
    }

    #[test]
    fn fps_override_ignores_frame_delays() {
        let gif = animation(&[100, 300, 500]);
        assert_eq!(gif.index_at(Duration::from_millis(250), 10.0, GifLoop::Restart), 2);
        assert_eq!(gif.index_at(Duration::from_millis(350), 10.0, GifLoop::Restart), 0);
    }
}
