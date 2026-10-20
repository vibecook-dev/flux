//! Deterministic frame quality scoring ("AVA-lite", research/05 §4.1).
//!
//! Every feature is computed on the small analysis frame inside its active
//! picture area (letterbox bars excluded):
//!
//! | Feature | Measure | Why |
//! |---|---|---|
//! | exposure | mean luma near mid-grey + 5–95th percentile spread | rejects black, blown-out and faded frames |
//! | sharpness | variance of the 3×3 Laplacian of luma | rejects motion blur and soft focus |
//! | colourfulness | Hasler & Süsstrunk (2003) M metric | colourful frames make better art |
//! | entropy | Shannon entropy of the 64-bin luma histogram | detail and tonal variety |
//! | uniformity | share of 16×16 blocks with σ < 2% | penalises title cards, sky, walls, bars |
//!
//! Hard rejects (score 0, kept for explanation): mean luma < 0.07 (black),
//! > 0.93 (white flash), spread < 0.08 (fade), uniform share > 0.85.

use image::RgbImage;
use serde::{Deserialize, Serialize};

use super::crop::Rect;

/// Scores for one candidate frame (all features in `0..=1`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameScore {
    /// Requested seek time in seconds.
    pub at_s: f64,
    pub mean_luma: f32,
    pub spread: f32,
    pub exposure: f32,
    pub sharpness: f32,
    pub colourfulness: f32,
    pub entropy: f32,
    pub uniform: f32,
    /// Weighted total, `0` when rejected.
    pub score: f32,
    /// Why the frame was rejected, if it was.
    pub reject: Option<String>,
    /// Near-duplicate of a better candidate (same keyframe reached by two seeks).
    pub duplicate: bool,
}

fn luma(p: &[u8; 3]) -> f32 {
    (0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2])) / 255.0
}

/// Luma plane of `img` restricted to `area`.
pub(crate) fn luma_plane(img: &RgbImage, area: Rect) -> (Vec<f32>, usize, usize) {
    let (w, h) = (area.w as usize, area.h as usize);
    let mut out = Vec::with_capacity(w * h);
    for y in area.y..area.y + area.h {
        for x in area.x..area.x + area.w {
            out.push(luma(&img.get_pixel(x, y).0));
        }
    }
    (out, w, h)
}

/// Score one analysis frame within its active area.
pub(crate) fn score_frame(img: &RgbImage, area: Rect, at_s: f64) -> FrameScore {
    let (l, w, h) = luma_plane(img, area);
    let n = l.len().max(1) as f32;
    let mean = l.iter().sum::<f32>() / n;

    let mut hist = [0u32; 64];
    for &v in &l {
        hist[((v * 63.0).round() as usize).min(63)] += 1;
    }
    let pct = |p: f32| -> f32 {
        let target = (p * n) as u32;
        let mut acc = 0;
        for (i, &c) in hist.iter().enumerate() {
            acc += c;
            if acc >= target {
                return i as f32 / 63.0;
            }
        }
        1.0
    };
    let spread = pct(0.95) - pct(0.05);
    let entropy = hist
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f32 / n;
            -p * p.log2()
        })
        .sum::<f32>()
        / 6.0;

    // Laplacian variance on a 0–255 scale. ~100+ is crisp at 480 px; blur sits well below 20.
    let mut lap_sum = 0.0f64;
    let mut lap_sq = 0.0f64;
    let mut count = 0.0f64;
    for y in 1..h.saturating_sub(1) {
        for x in 1..w.saturating_sub(1) {
            let c = l[y * w + x];
            let v = (l[(y - 1) * w + x] + l[(y + 1) * w + x] + l[y * w + x - 1] + l[y * w + x + 1] - 4.0 * c)
                * 255.0;
            lap_sum += f64::from(v);
            lap_sq += f64::from(v * v);
            count += 1.0;
        }
    }
    let lap_var = if count > 0.0 {
        (lap_sq / count - (lap_sum / count).powi(2)) as f32
    } else {
        0.0
    };
    let sharpness = 1.0 - (-lap_var / 150.0).exp();

    // Hasler & Süsstrunk colourfulness.
    let (mut srg, mut srg2, mut syb, mut syb2) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    for y in area.y..area.y + area.h {
        for x in area.x..area.x + area.w {
            let [r, g, b] = img.get_pixel(x, y).0.map(f64::from);
            let rg = r - g;
            let yb = 0.5 * (r + g) - b;
            srg += rg;
            srg2 += rg * rg;
            syb += yb;
            syb2 += yb * yb;
        }
    }
    let nf = f64::from(n);
    let (mrg, myb) = (srg / nf, syb / nf);
    let sd = ((srg2 / nf - mrg * mrg).max(0.0) + (syb2 / nf - myb * myb).max(0.0)).sqrt();
    let colourfulness = ((sd + 0.3 * (mrg * mrg + myb * myb).sqrt()) / 110.0).min(1.0) as f32;

    // Uniform 16×16 blocks.
    let (mut uniform_blocks, mut blocks) = (0u32, 0u32);
    for by in (0..h.saturating_sub(15)).step_by(16) {
        for bx in (0..w.saturating_sub(15)).step_by(16) {
            let (mut s, mut s2) = (0.0f32, 0.0f32);
            for y in by..by + 16 {
                for x in bx..bx + 16 {
                    let v = l[y * w + x];
                    s += v;
                    s2 += v * v;
                }
            }
            let m = s / 256.0;
            if (s2 / 256.0 - m * m).max(0.0).sqrt() < 0.02 {
                uniform_blocks += 1;
            }
            blocks += 1;
        }
    }
    let uniform = if blocks == 0 {
        0.0
    } else {
        uniform_blocks as f32 / blocks as f32
    };

    let mid = 1.0 - ((mean - 0.45).abs() / 0.45).min(1.0);
    let exposure = 0.5 * mid + 0.5 * (spread / 0.6).min(1.0);
    let reject = if mean < 0.07 {
        Some("too dark".to_owned())
    } else if mean > 0.93 {
        Some("blown out".to_owned())
    } else if spread < 0.08 {
        Some("flat (fade or title card)".to_owned())
    } else if uniform > 0.85 {
        Some("mostly uniform".to_owned())
    } else {
        None
    };
    let raw =
        0.25 * exposure + 0.30 * sharpness + 0.20 * colourfulness + 0.15 * entropy + 0.10 * (1.0 - uniform);
    FrameScore {
        at_s,
        mean_luma: mean,
        spread,
        exposure,
        sharpness,
        colourfulness,
        entropy,
        uniform,
        score: if reject.is_some() { 0.0 } else { raw },
        reject,
        duplicate: false,
    }
}

/// 64-bit average hash of the luma (8×8 means), for near-duplicate detection.
pub(crate) fn average_hash(img: &RgbImage) -> u64 {
    let (w, h) = img.dimensions();
    let mut cells = [0f32; 64];
    for (i, cell) in cells.iter_mut().enumerate() {
        let (cx, cy) = ((i % 8) as u32, (i / 8) as u32);
        let (x0, x1) = (cx * w / 8, ((cx + 1) * w / 8).max(cx * w / 8 + 1));
        let (y0, y1) = (cy * h / 8, ((cy + 1) * h / 8).max(cy * h / 8 + 1));
        let mut s = 0.0;
        let mut c = 0.0;
        for y in (y0..y1.min(h)).step_by(2) {
            for x in (x0..x1.min(w)).step_by(2) {
                s += luma(&img.get_pixel(x, y).0);
                c += 1.0;
            }
        }
        *cell = if c > 0.0 { s / c } else { 0.0 };
    }
    let mean = cells.iter().sum::<f32>() / 64.0;
    cells
        .iter()
        .enumerate()
        .fold(0u64, |acc, (i, &v)| if v > mean { acc | (1 << i) } else { acc })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full(img: &RgbImage) -> Rect {
        Rect {
            x: 0,
            y: 0,
            w: img.width(),
            h: img.height(),
        }
    }

    #[test]
    fn rejects_black_and_flat_and_prefers_detail() {
        let black = RgbImage::from_pixel(160, 90, image::Rgb([3, 3, 3]));
        assert_eq!(
            score_frame(&black, full(&black), 0.0).reject.as_deref(),
            Some("too dark")
        );
        let grey = RgbImage::from_pixel(160, 90, image::Rgb([128, 128, 128]));
        assert!(score_frame(&grey, full(&grey), 0.0).reject.is_some());

        // A colourful checkerboard beats a soft gradient.
        let checker = RgbImage::from_fn(160, 90, |x, y| {
            if (x / 8 + y / 8) % 2 == 0 {
                image::Rgb([235, 200, 50])
            } else {
                image::Rgb([30, 60, 170])
            }
        });
        // Smooth, grey, low-frequency shading: exposed fine but soft and colourless.
        let gradient = RgbImage::from_fn(160, 90, |x, y| {
            let v = (128.0 + 90.0 * (x as f32 / 10.0).sin() * (y as f32 / 14.0).cos()) as u8;
            image::Rgb([v, v, v])
        });
        let a = score_frame(&checker, full(&checker), 0.0);
        let b = score_frame(&gradient, full(&gradient), 0.0);
        assert!(a.reject.is_none() && b.reject.is_none(), "{a:?} {b:?}");
        assert!(a.score > b.score, "{} vs {}", a.score, b.score);
        assert!(a.sharpness > b.sharpness && a.colourfulness > b.colourfulness);
    }

    #[test]
    fn average_hash_detects_duplicates() {
        let a = RgbImage::from_fn(64, 36, |x, y| image::Rgb([(x * 4) as u8, (y * 7) as u8, 50]));
        let mut b = a.clone();
        b.put_pixel(3, 3, image::Rgb([255, 255, 255]));
        let c = RgbImage::from_fn(64, 36, |x, y| {
            image::Rgb([(255 - x * 4) as u8, (y * 7) as u8, 50])
        });
        assert!((average_hash(&a) ^ average_hash(&b)).count_ones() <= 2);
        assert!((average_hash(&a) ^ average_hash(&c)).count_ones() > 10);
    }
}
