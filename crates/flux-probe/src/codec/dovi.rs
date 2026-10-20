//! Dolby Vision decoder configuration record (`dvcC`/`dvvC`/`dvwC`, Matroska
//! `BlockAddIDExtraData`, MPEG-TS DOVI video stream descriptor 0xB0).
//! Dolby "Dolby Vision Streams Within the ISO Base Media File Format" §3.2.

use flux_core::DoviConfig;

use crate::bytes::Bits;

/// Parse the record body (starting at `dv_version_major`).
pub(crate) fn parse_config(b: &[u8]) -> Option<DoviConfig> {
    let mut r = Bits::new(b);
    r.skip(16)?; // dv_version_major, dv_version_minor
    let profile = r.u8(7)?;
    let level = r.u8(6)?;
    let rpu_present = r.flag()?;
    let el_present = r.flag()?;
    let bl_present = r.flag()?;
    let bl_signal_compat_id = r.u8(4)?;
    Some(DoviConfig {
        profile,
        level,
        rpu_present,
        el_present,
        bl_present,
        bl_signal_compat_id,
    })
}

/// The MPEG-TS DOVI descriptor has a 13-bit dependency PID (+3 reserved bits)
/// between `bl_present_flag` and the compatibility id when the BL is absent.
pub(crate) fn parse_ts_descriptor(b: &[u8]) -> Option<DoviConfig> {
    let mut r = Bits::new(b);
    r.skip(16)?;
    let profile = r.u8(7)?;
    let level = r.u8(6)?;
    let rpu_present = r.flag()?;
    let el_present = r.flag()?;
    let bl_present = r.flag()?;
    if !bl_present {
        r.skip(16)?;
    }
    let bl_signal_compat_id = r.u8(4)?;
    Some(DoviConfig {
        profile,
        level,
        rpu_present,
        el_present,
        bl_present,
        bl_signal_compat_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_8_1() {
        // version 1.0, profile 8, level 6, rpu+bl, compat 1
        let b = [1, 0, 0b0001_0000, 0b0011_0101, 0b0001_0000, 0, 0, 0];
        let c = parse_config(&b).unwrap();
        assert_eq!((c.profile, c.level), (8, 6));
        assert!(c.rpu_present && !c.el_present && c.bl_present);
        assert_eq!(c.bl_signal_compat_id, 1);
    }
}
