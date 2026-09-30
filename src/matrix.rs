//! Converts rendered frames into the LED buffer asusd's `Write` D-Bus method
//! expects. Ported from asusctl's `rog_anime::AnimeDiagonal` (the code behind
//! `asusctl anime pixel-image`) so frames skip spawning asusctl entirely.

use image::RgbImage;

use crate::model::{MatrixGeometry, MatrixModel};

/// Usable LED bytes per USB pane (640-byte packet minus header and padding).
const PANE_LEN: usize = 627;
const STRIX_LEN: usize = 810;

/// `(buffer offset, x, y, length)` for each diagonal row on the GA401, whose
/// panes have irregular gaps.
const GA401_ROWS: [(usize, usize, usize, usize); 55] = [
    (1, 0, 3, 32), (34, 0, 2, 33), (69, 1, 2, 33), (102, 1, 1, 33), (137, 2, 1, 33),
    (170, 2, 0, 33), (204, 3, 0, 33), (237, 4, 0, 32), (270, 5, 0, 32), (302, 6, 0, 31),
    (334, 7, 0, 31), (365, 8, 0, 30), (396, 9, 0, 30), (426, 10, 0, 29), (456, 11, 0, 29),
    (485, 12, 0, 28), (514, 13, 0, 28), (542, 14, 0, 27), (570, 15, 0, 27), (597, 16, 0, 26),
    (624, 17, 0, 26), (650, 18, 0, 25), (676, 19, 0, 25), (701, 20, 0, 24), (726, 21, 0, 24),
    (750, 22, 0, 23), (774, 23, 0, 23), (797, 24, 0, 22), (820, 25, 0, 22), (842, 26, 0, 21),
    (864, 27, 0, 21), (885, 28, 0, 20), (906, 29, 0, 20), (926, 30, 0, 19), (946, 31, 0, 19),
    (965, 32, 0, 18), (984, 33, 0, 18), (1002, 34, 0, 17), (1020, 35, 0, 17), (1037, 36, 0, 16),
    (1054, 37, 0, 16), (1070, 38, 0, 15), (1086, 39, 0, 15), (1101, 40, 0, 14), (1116, 41, 0, 14),
    (1130, 42, 0, 13), (1144, 43, 0, 13), (1157, 44, 0, 12), (1170, 45, 0, 12), (1182, 46, 0, 11),
    (1194, 47, 0, 11), (1205, 48, 0, 10), (1216, 49, 0, 10), (1226, 50, 0, 9), (1236, 51, 0, 9),
];

/// The `AnimeType` name asusd expects alongside the buffer.
pub fn anime_type(model: MatrixModel) -> &'static str {
    match model {
        MatrixModel::Ga401 => "GA401",
        // rog_anime tags GU604 buffers as GA402 as well; both use three panes.
        MatrixModel::Ga402 | MatrixModel::Gu604 | MatrixModel::Unknown => "GA402",
        // G635L and G835L share the LED layout and asusd's packet path.
        MatrixModel::Strix => "G635L",
    }
}

/// Maps a pixel-image frame onto the LED order of the matrix, scaling every
/// LED by `brightness` (0.0..=1.0) like `asusctl anime pixel-image --bright`.
pub fn led_buffer(frame: &RgbImage, brightness: f32, geometry: MatrixGeometry) -> Vec<u8> {
    map_leds(geometry, |x, y| {
        frame.get_pixel_checked(x as u32, y as u32).map_or(0, |px| {
            let level: u32 = px.0.iter().map(|channel| u32::from(channel / 3)).sum();
            (level as f32 * brightness) as u8
        })
    })
}

/// Like [`led_buffer`] for a row-major canvas of raw LED levels, as decoded
/// from a pixel GIF.
pub fn level_buffer(levels: &[u8], brightness: f32, geometry: MatrixGeometry) -> Vec<u8> {
    let width = geometry.width as usize;
    map_leds(geometry, |x, y| {
        if x >= width {
            return 0;
        }
        levels.get(y * width + x).map_or(0, |&level| (f32::from(level) * brightness) as u8)
    })
}

fn map_leds(geometry: MatrixGeometry, pixel: impl Fn(usize, usize) -> u8) -> Vec<u8> {
    let pixel = &pixel;
    let height = geometry.height as usize;
    // A diagonal run of `len` LEDs starting at (x, y), counted from the bottom.
    let row = |x: usize, y: usize, len: usize| (0..len).map(move |i| pixel(x + i, height - y - i - 1));

    match geometry.model {
        MatrixModel::Ga401 => {
            let mut buf = vec![0; PANE_LEN * 2];
            for (start, x, y, len) in GA401_ROWS {
                for (slot, value) in buf[start..start + len].iter_mut().zip(row(x, y, len)) {
                    *slot = value;
                }
            }
            buf
        }
        MatrixModel::Ga402 | MatrixModel::Unknown => {
            let head = [(0, 5), (1, 5), (1, 4), (2, 4), (2, 3), (3, 3), (3, 2), (4, 2), (4, 1), (5, 1), (5, 0), (6, 0)];
            let runs = head.into_iter().map(|(x, y)| (x, y, 34))
                .chain((7..=55).map(|x| (x, 0, 33 - (x - 7) / 2)));
            sequential(runs, PANE_LEN * 3, row)
        }
        MatrixModel::Gu604 => {
            let head = [(0, 4, 38), (0, 3, 39), (1, 3, 38), (1, 2, 39), (2, 2, 38), (2, 1, 39), (3, 1, 38), (3, 0, 39), (4, 0, 39), (5, 0, 39)];
            let runs = head.into_iter().chain((6..=58).map(|x| (x, 0, 38 - (x - 6) / 2)));
            sequential(runs, PANE_LEN * 3, row)
        }
        MatrixModel::Strix => {
            // Rows 0-27 grow from 1 to 14 LEDs in pairs, rows 28-67 hold 15.
            // Even/odd rows interleave on a half-step X grid.
            let mut buf = Vec::with_capacity(STRIX_LEN);
            for led_row in 0..68 {
                let (len, base_x) = if led_row < 28 { (led_row / 2 + 1, 0) } else { (15, (led_row - 28) / 2) };
                buf.extend((0..len).map(|i| pixel((base_x + i) * 2 + led_row % 2, led_row / 2)));
            }
            buf
        }
    }
}

fn sequential<R, I>(runs: impl Iterator<Item = (usize, usize, usize)>, len: usize, row: R) -> Vec<u8>
where
    R: Fn(usize, usize, usize) -> I,
    I: Iterator<Item = u8>,
{
    let mut buf = Vec::with_capacity(len);
    for (x, y, count) in runs {
        buf.extend(row(x, y, count));
    }
    buf.resize(len, 0);
    buf
}

#[cfg(test)]
mod tests {
    use image::Rgb;

    use super::*;

    #[test]
    fn buffers_match_asusd_lengths() {
        for (board, len) in [("GA401IV", 1254), ("GA402RK", 1881), ("GU604VY", 1881), ("G635LX", 810)] {
            let geometry = MatrixGeometry::for_board_name(board);
            let frame = RgbImage::new(geometry.width, geometry.height);
            assert_eq!(led_buffer(&frame, 1.0, geometry).len(), len, "{board}");
        }
    }

    #[test]
    fn boosted_levels_clip_at_full_instead_of_wrapping() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let levels = vec![200u8; (geometry.width * geometry.height) as usize];
        let buffer = level_buffer(&levels, 2.0, geometry);
        assert!(buffer[..1449].iter().all(|&led| led == 255));
    }

    #[test]
    fn white_frame_scales_with_brightness() {
        let geometry = MatrixGeometry::for_board_name("GA402RK");
        let frame = RgbImage::from_pixel(geometry.width, geometry.height, Rgb([255, 255, 255]));
        let buffer = led_buffer(&frame, 0.5, geometry);
        assert!(buffer[..1449].iter().all(|&led| led == 127));
    }
}
