//! SEI messages shared by H.264 (§7.3.2.3) and HEVC (§7.3.5): the static HDR
//! metadata payloads and HDR10+ detection. Parse, don't decode.

use flux_core::{ContentLightLevel, MasteringDisplay};

use super::VideoCfg;
use crate::bytes::Rd;

/// `mastering_display_colour_volume` (H.264 D.1.29 / HEVC D.2.28).
const PAYLOAD_MASTERING: u32 = 137;
/// `content_light_level_info` (H.264 D.1.31 / HEVC D.2.35).
const PAYLOAD_CLL: u32 = 144;
/// `user_data_registered_itu_t_t35`.
const PAYLOAD_T35: u32 = 4;

/// Walk the `sei_message()`s of an SEI RBSP (NAL header already removed,
/// emulation prevention already stripped) and fold what we recognise into `cfg`.
pub(crate) fn parse_sei_rbsp(rbsp: &[u8], cfg: &mut VideoCfg) {
    let mut r = Rd::new(rbsp);
    // A message needs at least type + size bytes; 0x80 alone is rbsp_trailing_bits.
    while r.remaining() >= 2 {
        let Some(ptype) = read_ff_coded(&mut r) else {
            return;
        };
        let Some(psize) = read_ff_coded(&mut r) else {
            return;
        };
        let Some(payload) = r.take(psize as usize) else {
            return;
        };
        match ptype {
            PAYLOAD_MASTERING => {
                if let Some(m) = mastering(payload) {
                    cfg.mastering = Some(m);
                }
            }
            PAYLOAD_CLL => {
                let mut p = Rd::new(payload);
                if let (Some(max_cll), Some(max_fall)) = (p.be16(), p.be16()) {
                    cfg.cll = Some(ContentLightLevel { max_cll, max_fall });
                }
            }
            PAYLOAD_T35 if is_hdr10plus_t35(payload) => cfg.hdr10plus = true,
            _ => {}
        }
    }
}

/// `0xFF`-run coded integer used for SEI payload type and size.
fn read_ff_coded(r: &mut Rd<'_>) -> Option<u32> {
    let mut v = 0u32;
    loop {
        let b = r.u8()?;
        v = v.checked_add(u32::from(b))?;
        if b != 0xff {
            return Some(v);
        }
    }
}

/// Mastering display payload: 3 × (x, y) in G, B, R order, white point,
/// max/min luminance. Units match `flux_core::MasteringDisplay` exactly.
pub(crate) fn mastering(p: &[u8]) -> Option<MasteringDisplay> {
    let mut r = Rd::new(p);
    let mut primaries = [[0u16; 2]; 3];
    for prim in &mut primaries {
        *prim = [r.be16()?, r.be16()?];
    }
    let white_point = [r.be16()?, r.be16()?];
    let max_luminance = r.be32()?;
    let min_luminance = r.be32()?;
    Some(MasteringDisplay {
        primaries,
        white_point,
        max_luminance,
        min_luminance,
    })
}

/// ITU-T T.35 payload carrying SMPTE ST 2094-40 (HDR10+): country 0xB5 (USA),
/// provider 0x003C (Samsung), provider-oriented code 0x0001, application id 4.
pub(crate) fn is_hdr10plus_t35(p: &[u8]) -> bool {
    let mut r = Rd::new(p);
    let (Some(country), Some(provider), Some(oriented), Some(app)) = (r.u8(), r.be16(), r.be16(), r.u8())
    else {
        return false;
    };
    country == 0xB5 && provider == 0x003C && oriented == 0x0001 && app == 4
}

/// ITU-T T.35 payload carrying a Dolby Vision RPU (AV1 metadata OBU):
/// country 0xB5, provider 0x003B (Dolby), provider-oriented code 0x00000800.
pub(crate) fn is_dovi_t35(p: &[u8]) -> bool {
    let mut r = Rd::new(p);
    let (Some(country), Some(provider), Some(oriented)) = (r.u8(), r.be16(), r.be32()) else {
        return false;
    };
    country == 0xB5 && provider == 0x003B && oriented == 0x0000_0800
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x265_hdr10_seis() {
        // payload 137 (24 bytes) + payload 144 (4 bytes), as written by x265 with
        // master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1)
        // and max-cll=1000,400.
        let mut sei = vec![137, 24];
        for v in [13250u16, 34500, 7500, 3000, 34000, 16000, 15635, 16450] {
            sei.extend_from_slice(&v.to_be_bytes());
        }
        sei.extend_from_slice(&10_000_000u32.to_be_bytes());
        sei.extend_from_slice(&1u32.to_be_bytes());
        sei.extend_from_slice(&[144, 4, 0x03, 0xe8, 0x01, 0x90, 0x80]);
        let mut cfg = VideoCfg::default();
        parse_sei_rbsp(&sei, &mut cfg);
        let m = cfg.mastering.expect("mastering");
        assert_eq!(m.primaries, [[13250, 34500], [7500, 3000], [34000, 16000]]);
        assert_eq!(m.white_point, [15635, 16450]);
        assert_eq!((m.max_luminance, m.min_luminance), (10_000_000, 1));
        assert_eq!(
            cfg.cll,
            Some(ContentLightLevel {
                max_cll: 1000,
                max_fall: 400
            })
        );
    }

    #[test]
    fn hdr10plus_marker() {
        assert!(is_hdr10plus_t35(&[0xB5, 0x00, 0x3C, 0x00, 0x01, 0x04, 0x01]));
        assert!(!is_hdr10plus_t35(&[0xB5, 0x00, 0x3B, 0x00, 0x01, 0x04]));
    }
}
