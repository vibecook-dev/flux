//! Letterbox detection and energy-based ("saliency-ish") crop placement.
//!
//! The energy map is Sobel gradient magnitude plus a chroma term (distance
//! from the frame's mean colour), so edges, faces, text-free detail and
//! saturated subjects attract the crop while flat sky and walls do not. The
//! crop window slides along the free axis; its score is the energy inside,
//! times a gentle centre bias (subjects are usually framed near the centre),
//! computed in O(width) with column prefix sums.

use image::RgbImage;
use serde::{Deserialize, Serialize};

/// A pixel rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    /// Scale a rectangle from an `from_w`-wide image to a `to_w`-wide one (same aspect).
    pub(crate) fn scale(self, sx: f64, sy: f64, max_w: u32, max_h: u32) -> Rect {
        let x = ((f64::from(self.x) * sx).round() as u32).min(max_w.saturating_sub(1));
        let y = ((f64::from(self.y) * sy).round() as u32).min(max_h.saturating_sub(1));
        let w = ((f64::from(self.w) * sx).round() as u32).clamp(1, max_w - x);
        let h = ((f64::from(self.h) * sy).round() as u32).clamp(1, max_h - y);
        Rect { x, y, w, h }
    }
}

fn luma(p: [u8; 3]) -> f32 {
    (0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2])) / 255.0
}

/// The picture area inside black letterbox/pillarbox bars. Bars are rows or
/// columns with mean luma < 0.06 and almost no variation; at most 25% per side
/// is removed so dark scenes are not mistaken for bars.
pub(crate) fn active_area(img: &RgbImage) -> Rect {
    let (w, h) = img.dimensions();
    let row_dark = |y: u32| {
        let (mut s, mut s2) = (0.0f32, 0.0f32);
        for x in (0..w).step_by(2) {
            let v = luma(img.get_pixel(x, y).0);
            s += v;
            s2 += v * v;
        }
        let n = w.div_ceil(2) as f32;
        let m = s / n;
        m < 0.06 && (s2 / n - m * m) < 0.0004
    };
    let col_dark = |x: u32| {
        let (mut s, mut s2) = (0.0f32, 0.0f32);
        for y in (0..h).step_by(2) {
            let v = luma(img.get_pixel(x, y).0);
            s += v;
            s2 += v * v;
        }
        let n = h.div_ceil(2) as f32;
        let m = s / n;
        m < 0.06 && (s2 / n - m * m) < 0.0004
    };
    let mut top = 0;
    while top < h / 4 && row_dark(top) {
        top += 1;
    }
    let mut bottom = 0;
    while bottom < h / 4 && row_dark(h - 1 - bottom) {
        bottom += 1;
    }
    let mut left = 0;
    while left < w / 4 && col_dark(left) {
        left += 1;
    }
    let mut right = 0;
    while right < w / 4 && col_dark(w - 1 - right) {
        right += 1;
    }
    // A whole-frame "bar" means a black frame, not a letterbox.
    if top + bottom >= h / 2 || left + right >= w / 2 {
        return Rect { x: 0, y: 0, w, h };
    }
    Rect {
        x: left,
        y: top,
        w: w - left - right,
        h: h - top - bottom,
    }
}

/// Per-pixel energy over `area`: Sobel magnitude of luma + 0.5 × chroma distance from the mean colour.
pub(crate) fn energy_map(img: &RgbImage, area: Rect) -> (Vec<f32>, usize, usize) {
    let (w, h) = (area.w as usize, area.h as usize);
    let mut l = vec![0f32; w * h];
    let (mut mr, mut mg, mut mb) = (0f64, 0f64, 0f64);
    for y in 0..h {
        for x in 0..w {
            let p = img.get_pixel(area.x + x as u32, area.y + y as u32).0;
            l[y * w + x] = luma(p);
            mr += f64::from(p[0]);
            mg += f64::from(p[1]);
            mb += f64::from(p[2]);
        }
    }
    let n = (w * h).max(1) as f64;
    let mean = [(mr / n) as f32, (mg / n) as f32, (mb / n) as f32];
    let mut e = vec![0f32; w * h];
    for y in 1..h.saturating_sub(1) {
        for x in 1..w.saturating_sub(1) {
            let at = |dx: isize, dy: isize| l[(y as isize + dy) as usize * w + (x as isize + dx) as usize];
            let gx = at(1, -1) + 2.0 * at(1, 0) + at(1, 1) - at(-1, -1) - 2.0 * at(-1, 0) - at(-1, 1);
            let gy = at(-1, 1) + 2.0 * at(0, 1) + at(1, 1) - at(-1, -1) - 2.0 * at(0, -1) - at(1, -1);
            let p = img.get_pixel(area.x + x as u32, area.y + y as u32).0;
            let chroma = ((f32::from(p[0]) - mean[0]).powi(2)
                + (f32::from(p[1]) - mean[1]).powi(2)
                + (f32::from(p[2]) - mean[2]).powi(2))
            .sqrt()
                / 441.0;
            e[y * w + x] = (gx * gx + gy * gy).sqrt() + 0.5 * chroma;
        }
    }
    (e, w, h)
}

/// The best window of aspect `aspect` (width / height) inside `area`, as large
/// as possible. Returns coordinates in `img` space.
pub(crate) fn best_crop(img: &RgbImage, area: Rect, aspect: f64) -> Rect {
    let area_aspect = f64::from(area.w) / f64::from(area.h);
    if (area_aspect - aspect).abs() < 0.01 {
        return area;
    }
    let (e, w, h) = energy_map(img, area);
    if area_aspect > aspect {
        // Wider than the target: full height, slide horizontally.
        let cw = ((f64::from(area.h) * aspect).round() as usize).clamp(1, w);
        let mut col = vec![0f64; w + 1];
        for x in 0..w {
            col[x + 1] = col[x] + (0..h).map(|y| f64::from(e[y * w + x])).sum::<f64>();
        }
        let x = best_offset(&col, w, cw);
        Rect {
            x: area.x + x as u32,
            y: area.y,
            w: cw as u32,
            h: area.h,
        }
    } else {
        // Taller than the target: full width, slide vertically.
        let ch = ((f64::from(area.w) / aspect).round() as usize).clamp(1, h);
        let mut row = vec![0f64; h + 1];
        for y in 0..h {
            row[y + 1] = row[y] + e[y * w..(y + 1) * w].iter().map(|&v| f64::from(v)).sum::<f64>();
        }
        let y = best_offset(&row, h, ch);
        Rect {
            x: area.x,
            y: area.y + y as u32,
            w: area.w,
            h: ch as u32,
        }
    }
}

/// Offset of the window of length `win` (over `len` cells with prefix sums
/// `pre`) maximising energy × centre bias (≤ 30% penalty at the edges).
fn best_offset(pre: &[f64], len: usize, win: usize) -> usize {
    let total = pre[len].max(1e-9);
    let span = (len - win).max(1) as f64;
    (0..=len - win)
        .map(|o| {
            let energy = (pre[o + win] - pre[o]) / total;
            let off_centre = ((o as f64) - (len - win) as f64 / 2.0).abs() / (span / 2.0);
            (o, energy * (1.0 - 0.3 * off_centre.min(1.0)))
        })
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map_or(0, |(o, _)| o)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_letterbox() {
        // 2.39:1 picture in a 16:9 frame: 30-row bars top and bottom of 270.
        let img = RgbImage::from_fn(480, 270, |x, y| {
            if !(33..237).contains(&y) {
                image::Rgb([2, 2, 2])
            } else {
                image::Rgb([(x % 255) as u8, 120, (y % 200) as u8])
            }
        });
        let a = active_area(&img);
        assert_eq!((a.y, a.h), (33, 204));
        assert_eq!((a.x, a.w), (0, 480));
        // An all-black frame is not a letterbox.
        let black = RgbImage::from_pixel(100, 60, image::Rgb([0, 0, 0]));
        assert_eq!(
            active_area(&black),
            Rect {
                x: 0,
                y: 0,
                w: 100,
                h: 60
            }
        );
    }

    #[test]
    fn crop_follows_the_subject() {
        // Flat background with a detailed subject on the right third.
        let img = RgbImage::from_fn(480, 270, |x, y| {
            if x > 330 && x < 450 && ((x / 6 + y / 6) % 2 == 0) {
                image::Rgb([240, 200, 30])
            } else {
                image::Rgb([40, 50, 60])
            }
        });
        let area = Rect {
            x: 0,
            y: 0,
            w: 480,
            h: 270,
        };
        let c = best_crop(&img, area, 2.0 / 3.0);
        assert_eq!((c.w, c.h), (180, 270));
        assert!(
            c.x + c.w / 2 > 300,
            "crop centre {} should be near the subject",
            c.x + c.w / 2
        );
        // 16:9 from 16:9 is the whole frame.
        assert_eq!(best_crop(&img, area, 16.0 / 9.0), area);
    }
}
