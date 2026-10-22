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
//!   consuming the first display set, which would never be drawn. A track
//!   of an MPEG-TS/M2TS (or of a file indexed by ffprobe) is seeked and
//!   shifted on ffmpeg's clock for it, which counts from the file's start
//!   time, and never seeked before the track's own start (`probe` asks
//!   ffprobe for both).
//! - **Canvas.** A PGS or VobSub stream is drawn for a picture size (its
//!   canvas) that need not be the video's: 1920×1080 subtitles over a
//!   cropped 1920×800 encode. The canvas is read from the stream
//!   (`probe`) and passed as `-canvas_size`; the subtitle picture is
//!   then scaled to the output (exactly like the video when it was drawn
//!   for the video's own size, else to fit inside, bottom-aligned).

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::Duration;

use flux_core::Codec;

use crate::asset::Asset;
use crate::index::Layout;

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
    /// For a track of a file whose timestamps ffmpeg counts from its start
    /// time (MPEG-TS, or a file indexed by ffprobe): ffmpeg's start time of
    /// the file, which `-ss` counts from, in seconds on its clock.
    pub file_start_s: Option<f64>,
    /// … and ffmpeg's start time of the track: its first packet when ffmpeg
    /// probed one, else the file's.
    pub stream_start_s: Option<f64>,
}

/// The ffmpeg input options and `-i` of the subtitle input of a run whose
/// first frame is at source time `first_s` (see the module docs).
/// `start_clock` is where the source timeline starts on ffmpeg's clock for
/// a video file whose timestamps ffmpeg counts from its start time
/// (`start_clock_origin_s`), `None` when they are the container's own.
pub(crate) fn input_args(
    b: &BurnIn,
    probe: &BurnProbe,
    first_s: f64,
    start_clock: Option<f64>,
) -> Vec<String> {
    let preroll = if b.embedded {
        EMBEDDED_PREROLL_S
    } else {
        FILE_PREROLL_S
    };
    let first_s = first_s.max(0.0);
    let seek = (first_s - preroll).max(0.0);
    let mut a: Vec<String> = Vec::new();
    match start_clock.filter(|_| b.embedded) {
        // A track of an MPEG-TS (or of a file ffmpeg indexed): `-ss` counts
        // from ffmpeg's start time of the file (`-seek_timestamp 1` there
        // loses a track that starts later than the file). Never before the
        // track's own start: ffmpeg would then line the track up from its
        // first packet instead of the seek point (measured with ffmpeg 8.1;
        // there is nothing before it to read anyway). `-itsoffset` minus
        // the seek point then takes source time `first_s` (at
        // `origin + first_s` on that clock) to 0.
        Some(origin) => {
            let file_start = probe.file_start_s.unwrap_or(origin);
            let at = (origin + seek)
                .max(file_start)
                .max(probe.stream_start_s.unwrap_or(file_start));
            a.extend([
                "-noaccurate_seek".into(),
                "-ss".into(),
                format!("{:.6}", at - file_start),
                "-itsoffset".into(),
                format!("{:.6}", at - origin - first_s),
            ]);
        }
        // Timestamps as they are: a `.sup` starts at its first subtitle,
        // which ffmpeg would otherwise subtract; Matroska and MP4 index
        // times are the container's.
        None => {
            // A subtitle file is seeked only past its first subtitle.
            let seek = match (b.embedded, probe.first_subtitle_s) {
                (true, _) => Some(seek),
                (false, Some(f)) if seek > f => Some(seek),
                _ => None,
            };
            a.extend(["-seek_timestamp".into(), "1".into()]);
            if let Some(seek) = seek {
                // No trim at the seek point: keep what is on screen already.
                a.extend(["-noaccurate_seek".into(), "-ss".into(), format!("{seek:.6}")]);
            }
            a.extend([
                "-itsoffset".into(),
                format!("{:.6}", seek.unwrap_or(0.0) - first_s),
            ]);
        }
    }
    if let Some((w, h)) = probe.canvas {
        a.extend(["-canvas_size".into(), format!("{w}x{h}")]);
    }
    a.extend(["-i".into(), b.path.display().to_string()]);
    a
}

/// MPEG-TS timestamps wrap at 2^33 (90 kHz ticks).
const TS_WRAP: i64 = 1 << 33;

/// For a video file whose timestamps ffmpeg counts from its start time, where
/// the source timeline (the playback index's, the pieces') starts on
/// ffmpeg's clock, in seconds: the ffprobe-scan index counts from the start
/// time, and the MPEG-TS index from its origin (ffmpeg's start time,
/// unwrapped: ffmpeg instead takes the wrap off a file that starts less
/// than 60 s before it, whose timestamps then start below 0).
/// Discontinuities, which the TS index closes up and ffmpeg doesn't, still
/// move what comes after them. `None` for Matroska and MP4, whose index
/// times are the container's own.
pub(crate) fn start_clock_origin_s(layout: &Layout) -> Option<f64> {
    match layout {
        Layout::Matroska(_) | Layout::Mp4(_) => None,
        Layout::External(l) => Some(l.start_time_ns as f64 / 1e9),
        Layout::Ts(l) => {
            let o = if l.origin >= TS_WRAP - 60 * 90_000 {
                l.origin - TS_WRAP
            } else {
                l.origin
            };
            Some(o as f64 / 90_000.0)
        }
    }
}

/// ffmpeg's start times of the file at `path` and of its stream `spec`, in
/// seconds on ffmpeg's clock ([`BurnProbe::file_start_s`]), as ffprobe
/// (the same probe as ffmpeg's) reads them. Blocking, bounded.
fn ffmpeg_start_times(ffprobe: &Path, path: &Path, spec: &str) -> Option<(f64, Option<f64>)> {
    let limits = crate::index::external::ScanLimits {
        stall: Duration::from_secs(20),
        base: Duration::from_secs(30),
        per_gb: Duration::ZERO,
    };
    let args = [
        "-v",
        "error",
        "-select_streams",
        spec,
        "-show_entries",
        "format=start_time:stream=start_time",
        "-of",
        "json",
    ];
    let out = crate::index::external::run(ffprobe, &args, path, &limits, 0).ok()?;
    let v: serde_json::Value = serde_json::from_str(&out).ok()?;
    let secs = |v: &serde_json::Value| v.as_str().and_then(|s| s.parse::<f64>().ok());
    let file = secs(&v["format"]["start_time"])?;
    let stream = v["streams"].get(0).and_then(|s| secs(&s["start_time"]));
    Some((file, stream))
}

/// How a run's filter graph names the subtitle stream of its second input
/// (`[1:…]`): 0 in a subtitle file; a track of the video by its index, or
/// in MPEG-TS by its PID, since ffmpeg's stream numbers there count streams
/// the index leaves out and the AC-3 core it adds on a Blu-ray TrueHD PID.
pub(crate) fn stream_spec(asset: &Asset, b: &BurnIn) -> String {
    if !b.embedded {
        return "0".into();
    }
    match (&asset.index.layout, asset.index.track(b.stream)) {
        (Layout::Ts(_), Some(t)) => format!("i:{}", t.id),
        _ => b.stream.to_string(),
    }
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

/// The `-filter_complex` of a run that burns subtitle stream `stream`
/// (`stream_spec`) of its second input in: the video chain (`pre`: trim
/// and setpts; `chain`: the software scale / tone-map / rotate ending in
/// `pix`), the subtitle picture scaled and overlaid, then `upload`
/// (`hwupload` for VAAPI) when the encoder needs it. The output pad is
/// `[out]`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn filter_graph(
    stream: &str,
    pre: &str,
    chain: &str,
    pix: &str,
    upload: Option<&str>,
    canvas: (u32, u32),
    src: (u32, u32),
    out: (u32, u32),
) -> String {
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
/// video's size); for a track of an MPEG-TS (or a file indexed by
/// ffprobe), ffmpeg's start times of the file and the track. Blocking:
/// reads the head of a subtitle file, or a track's first block, or runs
/// ffprobe once.
pub(crate) fn probe(asset: &Asset, b: &BurnIn, ffprobe: &Path) -> BurnProbe {
    let mut p = probe_stream(asset, b);
    if b.embedded && start_clock_origin_s(&asset.index.layout).is_some() {
        if let Some((file, stream)) = ffmpeg_start_times(ffprobe, &b.path, &stream_spec(asset, b)) {
            p.file_start_s = Some(file);
            p.stream_start_s = stream;
        }
    }
    p
}

/// [`probe`] without the start times.
fn probe_stream(asset: &Asset, b: &BurnIn) -> BurnProbe {
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
                ..Default::default()
            }
        }
        (Codec::VobSub, false) => {
            let h = head(256 << 10).unwrap_or_default();
            BurnProbe {
                canvas: vobsub_canvas(&h),
                first_subtitle_s: idx_first_s(&h),
                ..Default::default()
            }
        }
        (Codec::Pgs, true) => BurnProbe {
            canvas: asset
                .first_subtitle_block(b.stream)
                .ok()
                .flatten()
                .and_then(|block| pgs_canvas(&block, false)),
            ..Default::default()
        },
        (Codec::VobSub, true) => BurnProbe {
            canvas: asset
                .index
                .track(b.stream)
                .and_then(|t| vobsub_canvas(&t.codec_private)),
            ..Default::default()
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
            ..Default::default()
        };
        // A subtitle file keeps its own times, whatever the video's clock.
        let a = input_args(&b, &probe, 600.5, Some(21.4));
        let s = a.join(" ");
        assert!(
            s.contains("-seek_timestamp 1 -noaccurate_seek -ss 540.500000 -itsoffset -60.000000 -canvas_size 1920x1080 -i /m/Show/Ep.sup"),
            "{s}"
        );
        // Not past the file's first subtitle (near the start, or a late
        // first subtitle): read from the start, shifted the whole way.
        for first in [11.0, 65.0] {
            let s = input_args(&b, &probe, first, None).join(" ");
            assert!(
                s.contains("-seek_timestamp 1 -itsoffset") && !s.contains("-ss"),
                "{s}"
            );
        }
        let late = BurnProbe {
            first_subtitle_s: Some(600.0),
            ..probe
        };
        let s = input_args(&b, &late, 610.0, None).join(" ");
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
        let s = input_args(&e, &BurnProbe::default(), 100.0, None).join(" ");
        assert!(
            s.contains(
                "-seek_timestamp 1 -noaccurate_seek -ss 90.000000 -itsoffset -10.000000 -i /m/Film.mkv"
            ),
            "{s}"
        );
        // A track of an M2TS: ffmpeg's clock starts at the file's start time
        // (21.4 s), the source timeline at 21.4 s on it, and the track's
        // first packet is at 22.4 s.
        let ts = BurnIn {
            path: PathBuf::from("/m/Film.m2ts"),
            ..e
        };
        let tp = BurnProbe {
            file_start_s: Some(21.4),
            stream_start_s: Some(22.4),
            ..Default::default()
        };
        // `-ss` counts from the file's start time (no `-seek_timestamp`).
        let s = input_args(&ts, &tp, 100.0, Some(21.4)).join(" ");
        assert!(
            s.contains("-noaccurate_seek -ss 90.000000 -itsoffset -10.000000 -i /m/Film.m2ts")
                && !s.contains("-seek_timestamp"),
            "{s}"
        );
        // Near the start: seeked to the track's first packet, not before it,
        // and shifted from there.
        let s = input_args(&ts, &tp, 0.0, Some(21.4)).join(" ");
        assert!(s.contains("-ss 1.000000 -itsoffset 1.000000 -i"), "{s}");
        let s = input_args(&ts, &tp, 4.0, Some(21.4)).join(" ");
        assert!(s.contains("-ss 1.000000 -itsoffset -3.000000 -i"), "{s}");
        // ffmpeg's start time earlier than the source timeline's (a subtitle
        // track starting just before the audio and video): `-ss` from it.
        let early = BurnProbe {
            file_start_s: Some(21.0),
            stream_start_s: Some(21.0),
            ..Default::default()
        };
        let s = input_args(&ts, &early, 100.0, Some(21.4)).join(" ");
        assert!(s.contains("-ss 90.400000 -itsoffset -10.000000 -i"), "{s}");
        // ffprobe couldn't tell: the source timeline's origin stands in.
        let s = input_args(&ts, &BurnProbe::default(), 2.0, Some(21.4)).join(" ");
        assert!(s.contains("-ss 0.000000 -itsoffset -2.000000 -i"), "{s}");
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
        // A subtitle file is stream 0 of its input.
        let g = filter_graph(
            "0",
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
        let g = filter_graph(
            "4",
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
        // A TS track, by PID.
        let g = filter_graph("i:4608", "p", "c", "nv12", None, (1, 1), (1, 1), (1, 1));
        assert!(g.contains("[1:i:4608]null[s]"), "{g}");
    }

    #[test]
    fn source_timelines_start_where_ffmpeg_reads_them() {
        let ts = |origin: i64| {
            Layout::Ts(crate::ts::TsLayout {
                packet_size: 192,
                first_packet: 0,
                data_end: 0,
                origin,
                discontinuities: Vec::new(),
                video_share_ppm: 0,
                byte_rate: 0,
            })
        };
        assert_eq!(start_clock_origin_s(&ts(1_926_000)), Some(21.4));
        // Starting 19.3 s before the 33-bit wrap: ffmpeg's timestamps start
        // at −19.3 s there (it takes the wrap off), not at 95 424.4 s.
        let near = TS_WRAP - 1_737_000;
        assert_eq!(start_clock_origin_s(&ts(near)), Some(-19.3));
        // More than 60 s before it: as they are.
        let far = TS_WRAP - 61 * 90_000;
        assert_eq!(start_clock_origin_s(&ts(far)), Some(far as f64 / 90_000.0));
        let ext = Layout::External(crate::index::external::ExternalLayout {
            start_time_ns: 1_400_000_000,
        });
        assert_eq!(start_clock_origin_s(&ext), Some(1.4));
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
