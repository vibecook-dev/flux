//! Colour merging and HDR classification.

use flux_core::{Codec, ColorInfo, HdrFormat, HdrInfo, Track, TrackKind};

use crate::codec::VideoCfg;

/// H.273 "unspecified".
const UNSPECIFIED: u8 = 2;
/// H.273 transfer characteristics.
pub(crate) const TRANSFER_PQ: u8 = 16;
pub(crate) const TRANSFER_HLG: u8 = 18;

/// Merge two colour descriptions field by field: `primary` wins unless a
/// field is absent or "unspecified" (2), then `fallback` fills it.
pub(crate) fn merge_color(primary: Option<ColorInfo>, fallback: Option<ColorInfo>) -> Option<ColorInfo> {
    let spec = |v: Option<u8>| v.filter(|x| *x != UNSPECIFIED);
    let (p, f) = match (primary, fallback) {
        (None, None) => return None,
        (Some(p), None) => (p, ColorInfo::default()),
        (None, Some(f)) => (ColorInfo::default(), f),
        (Some(p), Some(f)) => (p, f),
    };
    let c = ColorInfo {
        primaries: spec(p.primaries)
            .or(spec(f.primaries))
            .or(p.primaries)
            .or(f.primaries),
        transfer: spec(p.transfer)
            .or(spec(f.transfer))
            .or(p.transfer)
            .or(f.transfer),
        matrix: spec(p.matrix).or(spec(f.matrix)).or(p.matrix).or(f.matrix),
        full_range: p.full_range.or(f.full_range),
    };
    (c != ColorInfo::default()).then_some(c)
}

/// Set `track.hdr` from the merged colour and the HDR facts in `cfg`.
///
/// `known` says whether we actually saw the codec's configuration (or the
/// codec cannot carry HDR); without it a video track stays `hdr: None`.
pub(crate) fn classify(track: &mut Track, cfg: &VideoCfg, known: bool) {
    if track.kind != TrackKind::Video {
        return;
    }
    let transfer = track.color.and_then(|c| c.transfer);
    let format = if cfg.dovi.is_some() || cfg.dovi_rpu {
        HdrFormat::DolbyVision
    } else if transfer == Some(TRANSFER_PQ) {
        if cfg.hdr10plus {
            HdrFormat::Hdr10Plus
        } else {
            HdrFormat::Hdr10
        }
    } else if transfer == Some(TRANSFER_HLG) {
        HdrFormat::Hlg
    } else if known || track.color.is_some() || !can_carry_hdr(track.codec) {
        HdrFormat::Sdr
    } else {
        track.hdr = None;
        return;
    };
    let hdr = format != HdrFormat::Sdr;
    track.hdr = Some(HdrInfo {
        format,
        mastering: if hdr { cfg.mastering } else { None },
        cll: if hdr { cfg.cll } else { None },
        dovi: cfg.dovi,
    });
}

fn can_carry_hdr(c: Codec) -> bool {
    matches!(
        c,
        Codec::Hevc | Codec::Av1 | Codec::Vp9 | Codec::H264 | Codec::ProRes
    )
}

/// Whether static HDR metadata still has to come from the first keyframe.
pub(crate) fn needs_frame_sei(track: &Track) -> bool {
    match &track.hdr {
        Some(h) => match h.format {
            HdrFormat::Sdr => false,
            HdrFormat::Hlg => h.mastering.is_none() && h.dovi.is_none(),
            _ => h.mastering.is_none() || h.cll.is_none(),
        },
        None => false,
    }
}

/// Re-derive the facts in `cfg` from an already classified track (so a
/// second pass can add to them and re-classify).
pub(crate) fn facts_of(track: &Track) -> VideoCfg {
    let mut cfg = VideoCfg::default();
    if let Some(h) = &track.hdr {
        cfg.mastering = h.mastering;
        cfg.cll = h.cll;
        cfg.dovi = h.dovi;
        cfg.hdr10plus = h.format == HdrFormat::Hdr10Plus;
        cfg.dovi_rpu = h.format == HdrFormat::DolbyVision && h.dovi.is_none();
    }
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_wins_unless_unspecified() {
        let container = ColorInfo {
            primaries: Some(9),
            transfer: Some(2),
            matrix: None,
            full_range: None,
        };
        let vui = ColorInfo {
            primaries: Some(1),
            transfer: Some(16),
            matrix: Some(9),
            full_range: Some(false),
        };
        let m = merge_color(Some(container), Some(vui)).unwrap();
        assert_eq!(m.primaries, Some(9));
        assert_eq!(m.transfer, Some(16));
        assert_eq!(m.matrix, Some(9));
        assert_eq!(m.full_range, Some(false));
        assert_eq!(merge_color(None, None), None);
    }
}
