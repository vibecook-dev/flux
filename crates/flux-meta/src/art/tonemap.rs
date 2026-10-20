//! HDR → SDR tone mapping for stills (PQ and HLG, BT.2020 → BT.709/sRGB).
//!
//! Input is 16-bit full-range R'G'B' as produced by
//! `scale=…:in_color_matrix=bt2020,format=rgb48le`. Steps:
//! 1. EOTF to absolute luminance: SMPTE ST 2084 (PQ) or the BT.2100 HLG
//!    inverse OETF + OOTF (γ = 1.2 at 1000 nits).
//! 2. Normalise to SDR reference white, 203 nits (ITU-R BT.2408).
//! 3. Extended Reinhard on luminance (white point = content peak) and
//!    scale RGB by the luminance ratio, which preserves hue.
//! 4. BT.2020 → BT.709 primaries, soft clip, sRGB OETF.
//!
//! This is a still-image grade for posters, not a reference tone mapper.

use image::RgbImage;

use super::frames::Transfer;

const REF_WHITE_NITS: f32 = 203.0;
const DEFAULT_PEAK_NITS: f32 = 1000.0;

/// BT.2020 → BT.709 linear RGB (ITU-R BT.2087).
const M2020_TO_709: [[f32; 3]; 3] = [
    [1.6605, -0.5876, -0.0728],
    [-0.1246, 1.1329, -0.0083],
    [-0.0182, -0.1006, 1.1187],
];

fn pq_eotf_nits(e: f32) -> f32 {
    const M1: f32 = 2610.0 / 16384.0;
    const M2: f32 = 2523.0 / 4096.0 * 128.0;
    const C1: f32 = 3424.0 / 4096.0;
    const C2: f32 = 2413.0 / 4096.0 * 32.0;
    const C3: f32 = 2392.0 / 4096.0 * 32.0;
    let p = e.clamp(0.0, 1.0).powf(1.0 / M2);
    let y = ((p - C1).max(0.0) / (C2 - C3 * p)).powf(1.0 / M1);
    10_000.0 * y
}

fn hlg_scene_linear(e: f32) -> f32 {
    const A: f32 = 0.178_832_77;
    const B: f32 = 0.284_668_92;
    const C: f32 = 0.559_910_7;
    let e = e.clamp(0.0, 1.0);
    if e <= 0.5 {
        e * e / 3.0
    } else {
        (((e - C) / A).exp() + B) / 12.0
    }
}

fn srgb_oetf(l: f32) -> f32 {
    let l = l.clamp(0.0, 1.0);
    if l <= 0.003_130_8 {
        12.92 * l
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    }
}

/// Tone-map a `w×h` 16-bit R'G'B' buffer to 8-bit sRGB.
pub(crate) fn to_sdr(px: &[u16], w: u32, h: u32, transfer: Transfer, peak_nits: Option<f32>) -> RgbImage {
    let peak = peak_nits
        .filter(|p| p.is_finite() && *p > REF_WHITE_NITS)
        .unwrap_or(DEFAULT_PEAK_NITS);
    // Per-channel EOTF as a 1024-entry LUT over the top 10 bits (sources are ≤ 12-bit).
    let lut: Vec<f32> = (0..1024u32)
        .map(|i| {
            let e = i as f32 / 1023.0;
            match transfer {
                Transfer::Pq => pq_eotf_nits(e),
                // HLG system gamma 1.2 for a 1000-nit display, applied per channel (approximation).
                Transfer::Hlg => 1000.0 * hlg_scene_linear(e).powf(1.2),
                Transfer::Sdr => e.powf(2.4) * REF_WHITE_NITS,
            }
        })
        .collect();
    let oetf: Vec<u8> = (0..4096u32)
        .map(|i| (srgb_oetf(i as f32 / 4095.0) * 255.0).round() as u8)
        .collect();
    let lw = peak / REF_WHITE_NITS;
    let lw2 = lw * lw;
    let mut out = RgbImage::new(w, h);
    for (dst, src) in out.pixels_mut().zip(px.as_chunks::<3>().0) {
        let r = lut[(src[0] >> 6) as usize] / REF_WHITE_NITS;
        let g = lut[(src[1] >> 6) as usize] / REF_WHITE_NITS;
        let b = lut[(src[2] >> 6) as usize] / REF_WHITE_NITS;
        let y = 0.2627 * r + 0.6780 * g + 0.0593 * b;
        let scale = if y > 1e-6 {
            (y * (1.0 + y / lw2) / (1.0 + y)) / y
        } else {
            1.0
        };
        let (r, g, b) = (r * scale, g * scale, b * scale);
        let m = &M2020_TO_709;
        let mut c = [
            m[0][0] * r + m[0][1] * g + m[0][2] * b,
            m[1][0] * r + m[1][1] * g + m[1][2] * b,
            m[2][0] * r + m[2][1] * g + m[2][2] * b,
        ];
        // Soft clip: out-of-gamut negatives to 0; overs desaturate toward luminance.
        let max = c.iter().copied().fold(0.0f32, f32::max);
        if max > 1.0 {
            for v in &mut c {
                *v /= max;
            }
        }
        for (i, v) in c.iter().enumerate() {
            dst.0[i] = oetf[((v.max(0.0) * 4095.0) as usize).min(4095)];
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pq_reference_points() {
        // PQ code value 0.5081 ≈ 100 nits; 0.7518 ≈ 1000 nits (ST 2084 tables).
        assert!((pq_eotf_nits(0.5081) - 100.0).abs() < 2.0);
        assert!((pq_eotf_nits(0.7518) - 1000.0).abs() < 15.0);
        assert_eq!(pq_eotf_nits(0.0), 0.0);
    }

    #[test]
    fn reference_white_maps_to_light_grey_and_black_stays_black() {
        // 203-nit PQ grey (code ≈ 0.58) should land in the upper mid-tones, not clip.
        let code = (0.58 * 65535.0) as u16;
        let img = to_sdr(&[code, code, code, 0, 0, 0], 2, 1, Transfer::Pq, None);
        let grey = img.get_pixel(0, 0).0[0];
        assert!((150..=235).contains(&grey), "{grey}");
        assert_eq!(img.get_pixel(1, 0).0, [0, 0, 0]);
    }
}
