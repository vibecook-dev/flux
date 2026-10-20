//! AVI (RIFF `AVI `): `hdrl` → `avih`, per-stream `strl` → `strh`/`strf`/`strn`,
//! OpenDML `odml/dmlh`. One read for typical files (the `hdrl` list is a few KiB).

use flux_core::{Codec, Container, TrackKind};

use crate::bytes::{fourcc_str, string, Rd};
use crate::codec::{self, apply_audio, mpegvideo, new_track, riff};
use crate::prober::{empty_result, Parsed};
use crate::store::{malformed, Ctx, PResult};

const MAX_HDRL: u64 = 1 << 20;
const AVIF_HASINDEX: u32 = 0x10;

/// RIFF sub-chunks of a body: (fourcc, data). LIST chunks are returned with
/// their list type as the first 4 bytes of data.
fn chunks(b: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut out = Vec::new();
    let mut r = Rd::new(b);
    while r.remaining() >= 8 {
        let (Some(id), Some(size)) = (r.fourcc(), r.le32()) else {
            break;
        };
        let size = size as usize;
        let Some(data) = r.take(size.min(r.remaining())) else {
            break;
        };
        out.push((id, data));
        if size % 2 == 1 {
            r.skip(1); // word alignment padding
        }
        if data.len() < size {
            break;
        }
    }
    out
}

pub(crate) fn parse(ctx: &mut Ctx<'_>) -> PResult<Parsed> {
    ctx.container = Container::Avi;
    let size = ctx.file_size();
    // RIFF header (12) then the first chunk should be LIST hdrl.
    let h = ctx.bytes(0, 24.min(size))?;
    if h.len() < 24 || &h[12..16] != b"LIST" || &h[20..24] != b"hdrl" {
        return malformed(Container::Avi, "first chunk is not LIST hdrl");
    }
    let hdrl_size = u64::from(u32::from_le_bytes([h[16], h[17], h[18], h[19]]));
    if hdrl_size > MAX_HDRL {
        return malformed(Container::Avi, format!("hdrl is {hdrl_size} bytes"));
    }
    let hdrl = ctx.bytes(24, hdrl_size.saturating_sub(4))?;

    let mut r = empty_result(Container::Avi);
    let mut usec_per_frame = 0u64;
    let mut total_frames = 0u64;
    let mut odml_frames = None;
    let mut flags = 0u32;
    let mut video_duration = None;
    for (id, data) in chunks(hdrl) {
        match (&id, data.get(..4)) {
            (b"avih", _) => {
                let mut a = Rd::new(data);
                usec_per_frame = u64::from(a.le32().unwrap_or(0));
                a.skip(8);
                flags = a.le32().unwrap_or(0);
                total_frames = u64::from(a.le32().unwrap_or(0));
            }
            (b"LIST", Some(b"strl")) => {
                let index = r.tracks.len() as u32;
                let (track, dur) = strl(ctx, data.get(4..).unwrap_or(&[]), index);
                if let Some(t) = track {
                    if t.kind == TrackKind::Video && video_duration.is_none() {
                        video_duration = dur;
                    }
                    r.tracks.push(t);
                }
            }
            (b"LIST", Some(b"odml")) => {
                for (oid, od) in chunks(data.get(4..).unwrap_or(&[])) {
                    if &oid == b"dmlh" {
                        odml_frames = Rd::new(od).le32().map(u64::from).filter(|f| *f > 0);
                    }
                }
            }
            _ => {}
        }
    }
    // OpenDML files (> 1 GiB) only count the first RIFF in avih; dmlh has the total.
    let frames = odml_frames.unwrap_or(total_frames);
    r.duration_ms = match (odml_frames, video_duration) {
        (None, Some(d)) => Some(d),
        _ if frames > 0 && usec_per_frame > 0 => Some(frames * usec_per_frame / 1000),
        (_, d) => d,
    };
    r.has_index = flags & AVIF_HASINDEX != 0;
    Ok(Parsed {
        result: r,
        locator: None,
    })
}

/// One stream list → (track, duration in ms).
fn strl(ctx: &mut Ctx<'_>, b: &[u8], index: u32) -> (Option<flux_core::Track>, Option<u64>) {
    let kids = chunks(b);
    let Some(strh) = kids.iter().find(|(i, _)| i == b"strh").map(|(_, d)| *d) else {
        ctx.warn(format!("stream {index}: no strh"));
        return (None, None);
    };
    let mut h = Rd::new(strh);
    let fcc_type = h.fourcc().unwrap_or([0; 4]);
    let handler = h.fourcc().unwrap_or([0; 4]);
    h.skip(4 + 2 + 2 + 4);
    let scale = u64::from(h.le32().unwrap_or(0));
    let rate = u64::from(h.le32().unwrap_or(0));
    h.skip(4);
    let length = u64::from(h.le32().unwrap_or(0));
    let duration = (scale > 0 && rate > 0 && length > 0)
        .then(|| u128::from(length) * u128::from(scale) * 1000 / u128::from(rate))
        .and_then(|d| u64::try_from(d).ok());
    let strf = kids
        .iter()
        .find(|(i, _)| i == b"strf")
        .map(|(_, d)| *d)
        .unwrap_or(&[]);
    let name = kids
        .iter()
        .find(|(i, _)| i == b"strn")
        .map(|(_, d)| string(d))
        .filter(|s| !s.is_empty());
    let kind = match &fcc_type {
        b"vids" => TrackKind::Video,
        b"auds" => TrackKind::Audio,
        b"txts" => TrackKind::Subtitle,
        _ => TrackKind::Data,
    };
    let mut t = new_track(index, u64::from(index), kind, Codec::Other, fourcc_str(handler));
    t.title = name;
    match kind {
        TrackKind::Video => {
            // OpenDML video properties: the stream's aspect ratio (beats the codec's).
            t.sample_aspect_ratio = kids
                .iter()
                .find(|(i, _)| i == b"vprp")
                .and_then(|(_, d)| vprp_sar(d));
            if let Some((fourcc, w, hgt, _)) = riff::bitmap_info(strf) {
                t.codec = riff::video_fourcc(fourcc);
                if t.codec == Codec::Other {
                    t.codec = riff::video_fourcc(handler);
                }
                t.codec_tag = fourcc_str(fourcc);
                t.width = Some(w).filter(|w| *w > 0);
                t.height = Some(hgt).filter(|h| *h > 0);
                // Some muxers put an MPEG-2 sequence header after the BITMAPINFOHEADER.
                if t.codec == Codec::Mpeg2Video {
                    if let Some(c) = strf.get(40..).and_then(mpegvideo::parse_mpeg2_es) {
                        codec::apply_video(&mut t, &c);
                    }
                }
            }
            if scale > 0 && rate > 0 {
                t.frame_rate = codec::rate_from_ticks(rate, scale);
            }
            t.bit_depth = Some(8);
            crate::hdr::classify(&mut t, &codec::VideoCfg::default(), true);
        }
        TrackKind::Audio => {
            if let Some((tag, a)) = riff::wave_format_ex(strf) {
                t.codec_tag = format!("{tag:#06x}");
                apply_audio(&mut t, &a);
                t.codec = a.codec.unwrap_or(Codec::Other);
            }
        }
        TrackKind::Subtitle => t.codec = Codec::Other,
        _ => {}
    }
    (Some(t), duration)
}

/// `vprp` (OpenDML `VideoPropHeader`): frame aspect ratio (X:Y in the high
/// and low words) over the active frame size gives the SAR, as FFmpeg's
/// demuxer computes it.
fn vprp_sar(b: &[u8]) -> Option<flux_core::Rational> {
    // FFmpeg reads it only when the chunk is larger than the 9-word header.
    if b.len() <= 9 * 4 {
        return None;
    }
    let mut r = Rd::new(b);
    r.skip(5 * 4); // format token, standard, refresh rate, h total, v total
    let aspect_y = u64::from(r.le16()?);
    let aspect_x = u64::from(r.le16()?);
    let width = u64::from(r.le32()?);
    let height = u64::from(r.le32()?);
    if aspect_x == 0 || aspect_y == 0 || width == 0 || height == 0 {
        return None;
    }
    // (X / Y) / (width / height)
    codec::sar(aspect_x * height, aspect_y * width)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vprp_aspect() {
        let mut b = vec![0u8; 5 * 4];
        b.extend_from_slice(&9u16.to_le_bytes()); // Y
        b.extend_from_slice(&16u16.to_le_bytes()); // X
        b.extend_from_slice(&720u32.to_le_bytes());
        b.extend_from_slice(&480u32.to_le_bytes());
        b.extend_from_slice(&1u32.to_le_bytes()); // fields per frame
        assert_eq!(vprp_sar(&b), None, "header only: FFmpeg ignores it");
        b.extend_from_slice(&[0; 32]); // one VIDEO_FIELD_DESC
        assert_eq!(vprp_sar(&b), Some(flux_core::Rational { num: 32, den: 27 }));
    }

    #[test]
    fn chunk_iter() {
        let mut b = Vec::new();
        b.extend_from_slice(b"strh");
        b.extend_from_slice(&3u32.to_le_bytes());
        b.extend_from_slice(&[1, 2, 3, 0]); // odd size + pad
        b.extend_from_slice(b"strf");
        b.extend_from_slice(&100u32.to_le_bytes()); // overruns: truncated
        b.extend_from_slice(&[9; 4]);
        let c = chunks(&b);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].1, &[1, 2, 3]);
        assert_eq!(c[1].1.len(), 4);
    }
}
