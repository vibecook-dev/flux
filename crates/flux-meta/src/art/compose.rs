//! Turning a chosen frame (or a generated image) into a poster, backdrop or thumbnail.

use image::imageops::FilterType;
use image::RgbImage;

use super::crop::Rect;
use super::typeset::Typesetter;

/// Crop `src` to `crop` and resize to exactly `w×h`.
pub(crate) fn crop_resize(src: &RgbImage, crop: Rect, w: u32, h: u32) -> RgbImage {
    let view = image::imageops::crop_imm(src, crop.x, crop.y, crop.w, crop.h).to_image();
    if view.dimensions() == (w, h) {
        return view;
    }
    // Catmull-Rom: sharp enough for downscales, no Lanczos ringing on upscales of small sources.
    image::imageops::resize(&view, w, h, FilterType::CatmullRom)
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Darken toward a near-black, slightly cool tone: a vignette everywhere, a
/// gentle top scrim, and a strong bottom scrim from `text_top` down so the
/// title reads on any frame.
pub(crate) fn grade_for_title(img: &mut RgbImage, text_top: f32) {
    let (w, h) = img.dimensions();
    let (wf, hf) = (w as f32, h as f32);
    let tint = [8.0f32, 8.0, 13.0];
    // The scrim starts above the text block and is fully dense well before the bottom.
    let g0 = (text_top - hf * 0.16).clamp(hf * 0.30, hf * 0.75);
    let g1 = (text_top + hf * 0.12).min(hf * 0.92).max(g0 + 1.0);
    for y in 0..h {
        let yf = y as f32 + 0.5;
        let bottom = 0.90 * smoothstep(g0, g1, yf);
        let top = 0.28 * (1.0 - smoothstep(0.0, hf * 0.16, yf));
        for x in 0..w {
            let dx = (x as f32 + 0.5) / wf - 0.5;
            let dy = yf / hf - 0.5;
            let vignette = 0.22 * smoothstep(0.25, 0.75, (dx * dx * 1.6 + dy * dy).sqrt() * 1.25);
            let a = 1.0 - (1.0 - bottom) * (1.0 - top) * (1.0 - vignette);
            let p = img.get_pixel_mut(x, y);
            for (c, t) in tint.iter().enumerate() {
                p.0[c] = (f32::from(p.0[c]) * (1.0 - a) + t * a).round() as u8;
            }
        }
    }
}

/// Poster: crop, resize, grade, typeset.
pub(crate) fn render_poster(
    src: &RgbImage,
    crop: Rect,
    w: u32,
    h: u32,
    title: &str,
    year: Option<u16>,
    ts: &Typesetter,
) -> RgbImage {
    let mut img = crop_resize(src, crop, w, h);
    let title = super::typeset::display_text(title);
    let text = ts.layout_poster(w, h, &title, year);
    grade_for_title(&mut img, text.top);
    ts.draw(&mut img, &text, &title);
    img
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grade_darkens_the_bottom_more_than_the_middle() {
        let mut img = RgbImage::from_pixel(200, 300, image::Rgb([200, 200, 200]));
        grade_for_title(&mut img, 220.0);
        let mid = img.get_pixel(100, 120).0[0];
        let bottom = img.get_pixel(100, 295).0[0];
        assert!(bottom < 50 && mid > 150, "mid {mid} bottom {bottom}");
    }
}
