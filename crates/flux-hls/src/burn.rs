//! Picture subtitles drawn into a video transcode (burn-in): PGS and VobSub
//! (and DVB), from a subtitle file beside the video (`.sup`, `.idx`) or a
//! track of the video itself. Selecting one forces a video transcode
//! (`VideoOp::Transcode::burn_subtitle`).
//!
//! - **Input.** The subtitle is a second ffmpeg input: the subtitle file, or
//!   the video file opened again for its subtitle stream. ffmpeg turns it
//!   into pictures (`sub2video`) that are overlaid on the video.
//! - **Where.** The overlay comes after the scale / tone-map / rotate chain,
//!   at the output size: subtitles are SDR graphics (tone-mapping them as
//!   if they were PQ would dim them), and overlaying the smaller picture is
//!   cheaper. A run that burns in decodes and filters in software (the
//!   overlay needs frames in memory) and still encodes in hardware.
//! - **Timing.** A run's video input is seeked to its keyframe and trimmed
//!   to its first piece, so its output starts at 0 at the piece's first
//!   frame (source time `first`). The subtitle input is seeked a preroll
//!   earlier (`-ss first − preroll`, without the accurate-seek trim, which
//!   would drop a subtitle that is already on screen when the piece
//!   starts) and shifted by `-itsoffset` so that source time `first` is 0
//!   there too. Pieces and seeks then line up exactly like the video. A
//!   subtitle file is read from its start instead when that point is not
//!   after its first subtitle: ffmpeg's seek in a `.sup` then fails after
//!   consuming the first display set, which would never be drawn.
//! - **Canvas.** A PGS or VobSub stream is drawn for a picture size (its
//!   canvas) that need not be the video's: 1920×1080 subtitles over a
//!   cropped 1920×800 encode. The canvas is read from the stream
//!   (`probe`) and passed as `-canvas_size`; the subtitle picture is
//!   then scaled to the output (exactly like the video when it was drawn
//!   for the video's own size, else to fit inside, bottom-aligned).

use std::hash::{Hash, Hasher};
use std::path::PathBuf;

use flux_core::Codec;

use crate::asset::Asset;

/// Names a [`BurnIn`] inside a [`crate::vtranscode::VideoTarget`] (which
/// stays `Copy`): registered by `VideoTranscoder::burn_key`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BurnKey(pub(crate) u64);

/// A subtitle drawn into the picture.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BurnIn {
    /// Where ffmpeg reads it: a subtitle file, or the video file itself.
    pub path: PathBuf,
    /// The subtitle stream in `path`: 0 for a subtitle file, the track
    /// index for a track of the video.
    pub stream: u32,
    pub codec: Codec,
    /// `path` is the video (a track of it), not a subtitle file.
    pub embedded: bool,
    /// A subtitle file's size and modification time (0 for a track): a
    /// replaced file is another recipe, never served from the old one's
    /// cached pieces.
    pub size: u64,
    pub mtime_ns: i64,
}

impl BurnIn {
    /// Its key (a hash of everything above).
    pub fn key(&self) -> BurnKey {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut h);
        BurnKey(h.finish())
    }
}

/// Seconds of a subtitle file read before a run's first frame, so that a
/// subtitle shown since then is drawn too (one longer than this is cut).
/// Reading a subtitle file is cheap.
const FILE_PREROLL_S: f64 = 60.0;
/// The same for a track of the video: reading the video file again costs
/// that much video, so less.
const EMBEDDED_PREROLL_S: f64 = 10.0;

/// What a burn-in reads from the subtitle up front (`probe`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BurnProbe {
    /// The picture size the subtitle is drawn for, when known.
    pub canvas: Option<(u32, u32)>,
    /// When a subtitle file's first subtitle shows, in seconds.
    pub first_subtitle_s: Option<f64>,
}

/// The ffmpeg input options and `-i` of the subtitle input of a run whose
/// first frame is at source time `first_s` (see the module docs).
pub(crate) fn input_args(b: &BurnIn, probe: &BurnProbe, first_s: f64) -> Vec<String> {
    let preroll = if b.embedded {
        EMBEDDED_PREROLL_S
    } else {
        FILE_PREROLL_S
    };
    let first_s = first_s.max(0.0);
    let seek = (first_s - preroll).max(0.0);
    // A subtitle file is seeked only past its first subtitle.
    let seek = match (b.embedded, probe.first_subtitle_s) {
        (true, _) => Some(seek),
        (false, Some(f)) if seek > f => Some(seek),
        _ => None,
    };
    // Timestamps as they are: a `.sup` starts at its first subtitle, which
    // ffmpeg would otherwise subtract.
    let mut a: Vec<String> = vec!["-seek_timestamp".into(), "1".into()];
    if let Some(seek) = seek {
        // No trim at the seek point: keep what is on screen already.
        a.extend(["-noaccurate_seek".into(), "-ss".into(), format!("{seek:.6}")]);
    }
    a.extend([
        "-itsoffset".into(),
        format!("{:.6}", seek.unwrap_or(0.0) - first_s),
    ]);
    if let Some((w, h)) = probe.canvas {
        a.extend(["-canvas_size".into(), format!("{w}x{h}")]);
    }
    a.extend(["-i".into(), b.path.display().to_string()]);
    a
}

/// Size and place of the subtitle picture on an `out` picture: `(w, h, x,
/// y)`. A canvas the size of the source video `src` is scaled with it
/// (anamorphic DVDs included); another is scaled to fit inside, keeping its
/// shape, centred and at the bottom.
pub(crate) fn place(canvas: (u32, u32), src: (u32, u32), out: (u32, u32)) -> (u32, u32, u32, u32) {
    let (cw, ch) = canvas;
    let (ow, oh) = out;
    if cw == 0 || ch == 0 || canvas == src {
        return (ow, oh, 0, 0);
    }
    let s = (ow as f64 / cw as f64).min(oh as f64 / ch as f64);
    let w = ((cw as f64 * s).round() as u32).clamp(1, ow);
    let h = ((ch as f64 * s).round() as u32).clamp(1, oh);
    (w, h, (ow - w) / 2, oh - h)
}

/// The `-filter_complex` of a run that burns `b` in: the video chain
/// (`pre`: trim and setpts; `chain`: the software scale / tone-map / rotate
/// ending in `pix`), the subtitle picture scaled and overlaid, then
/// `upload` (`hwupload` for VAAPI) when the encoder needs it. The output
/// pad is `[out]`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn filter_graph(
    b: &BurnIn,
    pre: &str,
    chain: &str,
    pix: &str,
    upload: Option<&str>,
    canvas: (u32, u32),
    src: (u32, u32),
    out: (u32, u32),
) -> String {
    let stream = if b.embedded { b.stream } else { 0 };
    let (w, h, x, y) = place(canvas, src, out);
    let scale = if (w, h) == canvas {
        "null".to_string()
    } else {
        format!("scale={w}:{h}")
    };
    let upload = upload.map(|u| format!(",{u}")).unwrap_or_default();
    format!(
        "[0:v:0]{pre},{chain}[v];[1:{stream}]{scale}[s];\
         [v][s]overlay=x={x}:y={y}:eof_action=pass:repeatlast=0,format={pix}{upload}[out]"
    )
}

/// When the first segment of a `.sup` file shows (its 90 kHz PTS), in
/// seconds.
pub(crate) fn sup_first_s(data: &[u8]) -> Option<f64> {
    if data.get(..2)? != b"PG" {
        return None;
    }
    let pts = u32::from_be_bytes(data.get(2..6)?.try_into().ok()?);
    Some(pts as f64 / 90_000.0)
}

/// The first `timestamp: hh:mm:ss:mmm` of a VobSub index, in seconds.
pub(crate) fn idx_first_s(idx: &[u8]) -> Option<f64> {
    let text = String::from_utf8_lossy(idx);
    let ts = text.lines().find_map(|l| l.trim().strip_prefix("timestamp:"))?;
    let ts = ts.split(',').next()?.trim();
    let p: Vec<f64> = ts
        .split(':')
        .map(|x| x.trim().parse().ok())
        .collect::<Option<_>>()?;
    match p.as_slice() {
        [h, m, s, ms] => Some(h * 3600.0 + m * 60.0 + s + ms / 1000.0),
        _ => None,
    }
}

/// The width and height of the first presentation composition segment in
/// PGS data: `sup` = a `.sup` file (each segment after a 13-byte `PG`
/// header), else a Matroska block (3-byte segment headers).
pub(crate) fn pgs_canvas(data: &[u8], sup: bool) -> Option<(u32, u32)> {
    let header = if sup { 13 } else { 3 };
    let mut i = 0;
    while i + header <= data.len() {
        if sup && &data[i..i + 2] != b"PG" {
            return None;
        }
        let ty = data[i + header - 3];
        let len = u16::from_be_bytes([data[i + header - 2], data[i + header - 1]]) as usize;
        let body = data.get(i + header..i + header + len)?;
        if ty == 0x16 && body.len() >= 4 {
            let w = u16::from_be_bytes([body[0], body[1]]) as u32;
            let h = u16::from_be_bytes([body[2], body[3]]) as u32;
            return (w > 0 && h > 0).then_some((w, h));
        }
        i += header + len;
    }
    None
}

/// The `size: 720x480` line of a VobSub index (an `.idx` file, or a
/// Matroska VobSub track's CodecPrivate).
pub(crate) fn vobsub_canvas(idx: &[u8]) -> Option<(u32, u32)> {
    let text = String::from_utf8_lossy(idx);
    let line = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("size:").map(str::trim))?;
    let (w, h) = line.split_once('x')?;
    let (w, h) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    (w > 0 && h > 0).then_some((w, h))
}

/// What `b` is drawn for and when it starts: the canvas from its first
/// presentation segment (PGS) or index (VobSub), and a file's first
/// subtitle time. Unknown parts stay `None` (for DVB, ffmpeg then uses the
/// video's size). Blocking: reads the head of a subtitle file, or a
/// track's first block.
pub(crate) fn probe(asset: &Asset, b: &BurnIn) -> BurnProbe {
    use std::io::Read;
    let head = |limit: u64| -> Option<Vec<u8>> {
        let mut v = Vec::new();
        std::fs::File::open(&b.path)
            .ok()?
            .take(limit)
            .read_to_end(&mut v)
            .ok()?;
        Some(v)
    };
    match (b.codec, b.embedded) {
        (Codec::Pgs, false) => {
            let h = head(1 << 20).unwrap_or_default();
            BurnProbe {
                canvas: pgs_canvas(&h, true),
                first_subtitle_s: sup_first_s(&h),
            }
        }
        (Codec::VobSub, false) => {
            let h = head(256 << 10).unwrap_or_default();
            BurnProbe {
                canvas: vobsub_canvas(&h),
                first_subtitle_s: idx_first_s(&h),
            }
        }
        (Codec::Pgs, true) => BurnProbe {
            canvas: asset
                .first_subtitle_block(b.stream)
                .ok()
                .flatten()
                .and_then(|block| pgs_canvas(&block, false)),
            first_subtitle_s: None,
        },
        (Codec::VobSub, true) => BurnProbe {
            canvas: asset
                .index
                .track(b.stream)
                .and_then(|t| vobsub_canvas(&t.codec_private)),
            first_subtitle_s: None,
        },
        _ => BurnProbe::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, codec: Codec) -> BurnIn {
        BurnIn {
            path: PathBuf::from(path),
            stream: 0,
            codec,
            embedded: false,
            size: 10,
            mtime_ns: 1,
        }
    }

    #[test]
    fn subtitle_input_is_seeked_a_preroll_early_and_shifted_to_the_first_frame() {
        let b = file("/m/Show/Ep.sup", Codec::Pgs);
        let probe = BurnProbe {
            canvas: Some((1920, 1080)),
            first_subtitle_s: Some(9.1),
        };
        let a = input_args(&b, &probe, 600.5);
        let s = a.join(" ");
        assert!(
            s.contains("-seek_timestamp 1 -noaccurate_seek -ss 540.500000 -itsoffset -60.000000 -canvas_size 1920x1080 -i /m/Show/Ep.sup"),
            "{s}"
        );
        // Not past the file's first subtitle (near the start, or a late
        // first subtitle): read from the start, shifted the whole way.
        for first in [11.0, 65.0] {
            let s = input_args(&b, &probe, first).join(" ");
            assert!(
                s.contains("-seek_timestamp 1 -itsoffset") && !s.contains("-ss"),
                "{s}"
            );
        }
        let late = BurnProbe {
            first_subtitle_s: Some(600.0),
            ..probe
        };
        let s = input_args(&b, &late, 610.0).join(" ");
        assert!(
            s.contains("-itsoffset -610.000000 -canvas_size") && !s.contains("-ss"),
            "{s}"
        );
        // A track of the video reads less ahead (it reads video too).
        let e = BurnIn {
            embedded: true,
            stream: 3,
            ..file("/m/Film.mkv", Codec::Pgs)
        };
        let s = input_args(&e, &BurnProbe::default(), 100.0).join(" ");
        assert!(
            s.contains("-ss 90.000000 -itsoffset -10.000000 -i /m/Film.mkv"),
            "{s}"
        );
        // Different files, different recipes.
        assert_ne!(
            b.key(),
            BurnIn {
                size: 11,
                ..b.clone()
            }
            .key()
        );
    }

    #[test]
    fn subtitle_pictures_are_placed_on_the_output() {
        // Drawn for the video's size: scaled with it.
        assert_eq!(place((1920, 1080), (1920, 1080), (1280, 720)), (1280, 720, 0, 0));
        // An anamorphic DVD: still with the video.
        assert_eq!(place((720, 480), (720, 480), (854, 480)), (854, 480, 0, 0));
        // 1080p subtitles over a cropped 1920×800 encode: fit, at the bottom.
        assert_eq!(place((1920, 1080), (1920, 800), (1920, 800)), (1422, 800, 249, 0));
        // Same shape, other size (UHD video with 1080p PGS).
        assert_eq!(
            place((1920, 1080), (3840, 2160), (1920, 1080)),
            (1920, 1080, 0, 0)
        );
    }

    #[test]
    fn overlay_comes_after_the_chain() {
        let b = file("/m/Ep.sup", Codec::Pgs);
        let g = filter_graph(
            &b,
            "trim=start=1.5,setpts=PTS-STARTPTS",
            "scale=1280:720,format=nv12",
            "nv12",
            None,
            (1920, 1080),
            (1920, 1080),
            (1280, 720),
        );
        assert_eq!(
            g,
            "[0:v:0]trim=start=1.5,setpts=PTS-STARTPTS,scale=1280:720,format=nv12[v];\
             [1:0]scale=1280:720[s];\
             [v][s]overlay=x=0:y=0:eof_action=pass:repeatlast=0,format=nv12[out]"
        );
        let e = BurnIn {
            embedded: true,
            stream: 4,
            ..b
        };
        let g = filter_graph(
            &e,
            "p",
            "c",
            "nv12",
            Some("hwupload"),
            (1280, 720),
            (1280, 720),
            (1280, 720),
        );
        assert!(
            g.contains("[1:4]null[s]") && g.ends_with("format=nv12,hwupload[out]"),
            "{g}"
        );
    }

    #[test]
    fn pgs_canvas_from_the_first_composition() {
        // A `.sup`: PG, pts, dts, then (type, size, body); PDS first, then PCS.
        let seg = |ty: u8, body: &[u8]| {
            let mut v = b"PG".to_vec();
            v.extend_from_slice(&[0; 8]);
            v.push(ty);
            v.extend_from_slice(&(body.len() as u16).to_be_bytes());
            v.extend_from_slice(body);
            v
        };
        let mut sup = seg(0x14, &[0, 0]);
        sup.extend(seg(0x16, &[0x07, 0x80, 0x04, 0x38, 0x10]));
        assert_eq!(pgs_canvas(&sup, true), Some((1920, 1080)));
        assert_eq!(pgs_canvas(b"not pgs at all", true), None);
        // A Matroska block: the same segments without the 10-byte prefix.
        let block = [0x16, 0, 4, 0x05, 0x00, 0x02, 0xD0];
        assert_eq!(pgs_canvas(&block, false), Some((1280, 720)));
        // Truncated: nothing.
        assert_eq!(pgs_canvas(&block[..5], false), None);
    }

    #[test]
    fn vobsub_canvas_from_the_index() {
        let idx = b"# VobSub index file, v7\nsize: 720x576\norg: 0, 0\npalette: 000000\n\
                    id: en, index: 0\ntimestamp: 00:01:02:500, filepos: 000000000\n";
        assert_eq!(vobsub_canvas(idx), Some((720, 576)));
        assert_eq!(vobsub_canvas(b"size: x"), None);
        assert_eq!(idx_first_s(idx), Some(62.5));
        assert_eq!(idx_first_s(b"no timestamps"), None);
    }

    #[test]
    fn sup_first_subtitle_time() {
        let mut sup = b"PG".to_vec();
        sup.extend_from_slice(&(90_000u32 * 9).to_be_bytes());
        sup.extend_from_slice(&[0, 0, 0, 0, 0x16, 0, 0]);
        assert_eq!(sup_first_s(&sup), Some(9.0));
        assert_eq!(sup_first_s(b"XX"), None);
    }
}
