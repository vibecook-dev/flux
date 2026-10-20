//! `ffprobe` subprocess: fallback for containers flux-probe does not parse
//! (ASF/WMV, FLV, OGM, MPEG-PS/VOB, RealMedia…) and the oracle for
//! differential testing (`probe-diff`).

use std::path::Path;
use std::process::Command;

use flux_core::{
    Attachment, Chapter, Codec, ColorInfo, Container, ContentLightLevel, DoviConfig, HdrFormat, HdrInfo,
    MasteringDisplay, ProbeResult, Rational, Track, TrackKind,
};
use serde_json::Value;

use crate::codec::new_track;
use crate::error::ProbeError;
use crate::prober::empty_result;

/// ffprobe binary (override with `FLUX_FFPROBE`).
pub fn ffprobe_bin() -> String {
    std::env::var("FLUX_FFPROBE").unwrap_or_else(|_| "ffprobe".to_string())
}

/// The arguments flux uses for the oracle/fallback.
pub const DEFAULT_ARGS: &[&str] = &[
    "-v",
    "error",
    "-print_format",
    "json",
    "-show_format",
    "-show_streams",
    "-show_chapters",
];

/// Jellyfin 12's probe arguments (MediaBrowser.MediaEncoding.Probing), used
/// by `probe-bench` for a like-for-like cost comparison.
pub const JELLYFIN_ARGS: &[&str] = &[
    "-v",
    "error",
    "-analyzeduration",
    "200M",
    "-probesize",
    "1G",
    "-show_streams",
    "-show_format",
    "-show_chapters",
    "-print_format",
    "json",
];

/// Run ffprobe with `args` + `path` and parse its JSON output.
pub fn run_json(path: &Path, args: &[&str]) -> Result<Value, ProbeError> {
    let out = Command::new(ffprobe_bin())
        .args(args)
        .arg(path)
        .output()
        .map_err(|e| ProbeError::Ffprobe(format!("spawn: {e}")))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(ProbeError::Ffprobe(format!(
            "exit {}: {}",
            out.status,
            err.trim()
        )));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| ProbeError::Ffprobe(format!("bad JSON: {e}")))
}

/// Probe `path` with ffprobe and map the output to a [`ProbeResult`].
pub fn probe(path: &Path) -> Result<ProbeResult, ProbeError> {
    let v = run_json(path, DEFAULT_ARGS)?;
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let mut r = from_json(&v, ext);
    r.warnings.push("probed by ffprobe".into());
    Ok(r)
}

/// Fill HDR facts that ffprobe only reports per frame: runs
/// `-show_frames -read_intervals %+#1` on the first video stream and folds
/// the frame's colour and side data (mastering, CLL, HDR10+, DoVi RPU) into
/// `r`. Used by the oracle: e.g. for an HEVC MKV ffprobe 9 shows no
/// `color_transfer` at stream level even though the SPS VUI signals PQ.
pub fn add_first_frame_hdr(path: &Path, r: &mut ProbeResult) -> Result<(), ProbeError> {
    let v = run_json(
        path,
        &[
            "-v",
            "error",
            "-print_format",
            "json",
            "-select_streams",
            "v:0",
            "-show_frames",
            "-read_intervals",
            "%+#1",
        ],
    )?;
    let Some(frame) = v.get("frames").and_then(Value::as_array).and_then(|f| f.first()) else {
        return Ok(());
    };
    let Some(t) = r.tracks.iter_mut().find(|t| t.kind == TrackKind::Video) else {
        return Ok(());
    };
    let fc = color_of(frame);
    t.color = crate::hdr::merge_color(t.color, fc);
    let mut hdr = t.hdr.clone().unwrap_or(HdrInfo {
        format: HdrFormat::Sdr,
        mastering: None,
        cll: None,
        dovi: None,
    });
    let mut plus = false;
    let mut rpu = false;
    for sd in frame
        .get("side_data_list")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        side_data(sd, &mut hdr, &mut plus, &mut rpu);
    }
    hdr.format = format_of(t.color, hdr.dovi.is_some() || rpu, plus);
    if hdr.format == HdrFormat::Sdr {
        hdr.mastering = None;
        hdr.cll = None;
    }
    t.hdr = Some(hdr);
    Ok(())
}

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

fn num(v: &Value, k: &str) -> Option<f64> {
    match v.get(k)? {
        Value::Number(n) => n.as_f64(),
        Value::String(x) => x.parse().ok(),
        _ => None,
    }
}

/// `"34000/50000"` → 34000 (the numerator in the given denominator's units).
fn ratio_num(v: &Value, k: &str, unit_den: f64) -> Option<f64> {
    let x = s(v, k)?;
    let (n, d) = x.split_once('/')?;
    let (n, d): (f64, f64) = (n.parse().ok()?, d.parse().ok()?);
    (d != 0.0).then(|| n / d * unit_den)
}

/// Map an ffprobe codec name.
pub fn codec_of(name: &str) -> Codec {
    match name {
        "h264" => Codec::H264,
        "hevc" => Codec::Hevc,
        "av1" => Codec::Av1,
        "vp9" => Codec::Vp9,
        "vp8" => Codec::Vp8,
        "mpeg2video" => Codec::Mpeg2Video,
        "mpeg4" => Codec::Mpeg4Part2,
        "vc1" | "wmv3" => Codec::Vc1,
        "prores" => Codec::ProRes,
        "mjpeg" => Codec::Mjpeg,
        "aac" | "aac_latm" => Codec::Aac,
        "ac3" => Codec::Ac3,
        "eac3" => Codec::Eac3,
        "truehd" => Codec::TrueHd,
        "dts" => Codec::Dts,
        "flac" => Codec::Flac,
        "opus" => Codec::Opus,
        "vorbis" => Codec::Vorbis,
        "mp3" => Codec::Mp3,
        "mp2" => Codec::Mp2,
        "alac" => Codec::Alac,
        n if n.starts_with("pcm_") => Codec::Pcm,
        "subrip" | "srt" => Codec::Subrip,
        "ass" => Codec::Ass,
        "ssa" => Codec::Ssa,
        "webvtt" => Codec::WebVtt,
        "mov_text" => Codec::MovText,
        "hdmv_pgs_subtitle" => Codec::Pgs,
        "dvd_subtitle" => Codec::VobSub,
        "dvb_subtitle" => Codec::DvbSub,
        _ => Codec::Other,
    }
}

fn primaries_code(n: &str) -> Option<u8> {
    Some(match n {
        "bt709" => 1,
        "bt470m" => 4,
        "bt470bg" => 5,
        "smpte170m" => 6,
        "smpte240m" => 7,
        "film" => 8,
        "bt2020" => 9,
        "smpte428" => 10,
        "smpte431" => 11,
        "smpte432" => 12,
        "ebu3213" | "jedec-p22" => 22,
        _ => return None,
    })
}

fn transfer_code(n: &str) -> Option<u8> {
    Some(match n {
        "bt709" => 1,
        "bt470m" | "gamma22" => 4,
        "bt470bg" | "gamma28" => 5,
        "smpte170m" => 6,
        "smpte240m" => 7,
        "linear" => 8,
        "log100" | "log" => 9,
        "log316" | "log_sqrt" => 10,
        "iec61966-2-4" => 11,
        "bt1361e" | "bt1361" => 12,
        "iec61966-2-1" => 13,
        "bt2020-10" | "bt2020_10bit" => 14,
        "bt2020-12" | "bt2020_12bit" => 15,
        "smpte2084" => 16,
        "smpte428" => 17,
        "arib-std-b67" => 18,
        _ => return None,
    })
}

fn matrix_code(n: &str) -> Option<u8> {
    Some(match n {
        "gbr" => 0,
        "bt709" => 1,
        "fcc" => 4,
        "bt470bg" => 5,
        "smpte170m" => 6,
        "smpte240m" => 7,
        "ycgco" => 8,
        "bt2020nc" => 9,
        "bt2020c" => 10,
        "smpte2085" => 11,
        "chroma-derived-nc" => 12,
        "chroma-derived-c" => 13,
        "ictcp" => 14,
        _ => return None,
    })
}

fn color_of(v: &Value) -> Option<ColorInfo> {
    let c = ColorInfo {
        primaries: s(v, "color_primaries").and_then(primaries_code),
        transfer: s(v, "color_transfer").and_then(transfer_code),
        matrix: s(v, "color_space").and_then(matrix_code),
        full_range: s(v, "color_range").and_then(|r| match r {
            "pc" | "jpeg" => Some(true),
            "tv" | "mpeg" => Some(false),
            _ => None,
        }),
    };
    (c != ColorInfo::default()).then_some(c)
}

fn bit_depth_of(st: &Value) -> Option<u8> {
    if let Some(b) = num(st, "bits_per_raw_sample").filter(|b| *b > 0.0) {
        return Some(b as u8);
    }
    let pf = s(st, "pix_fmt")?;
    for (pat, d) in [
        ("16", 16u8),
        ("14", 14),
        ("12", 12),
        ("10", 10),
        ("p010", 10),
        ("p016", 16),
    ] {
        if pf.contains(pat) {
            return Some(d);
        }
    }
    Some(8)
}

fn side_data(sd: &Value, hdr: &mut HdrInfo, plus: &mut bool, rpu: &mut bool) {
    match s(sd, "side_data_type").unwrap_or("") {
        "DOVI configuration record" => {
            let g = |k| num(sd, k).unwrap_or(0.0) as u8;
            hdr.dovi = Some(DoviConfig {
                profile: g("dv_profile"),
                level: g("dv_level"),
                rpu_present: g("rpu_present_flag") != 0,
                el_present: g("el_present_flag") != 0,
                bl_present: g("bl_present_flag") != 0,
                bl_signal_compat_id: g("dv_bl_signal_compatibility_id"),
            });
        }
        "Mastering display metadata" => {
            let c = |k| ratio_num(sd, k, 50_000.0).map(|x| x.round() as u16);
            let l = |k| ratio_num(sd, k, 10_000.0).map(|x| x.round() as u32);
            if let (
                Some(gx),
                Some(gy),
                Some(bx),
                Some(by),
                Some(rx),
                Some(ry),
                Some(wx),
                Some(wy),
                Some(max),
                Some(min),
            ) = (
                c("green_x"),
                c("green_y"),
                c("blue_x"),
                c("blue_y"),
                c("red_x"),
                c("red_y"),
                c("white_point_x"),
                c("white_point_y"),
                l("max_luminance"),
                l("min_luminance"),
            ) {
                hdr.mastering = Some(MasteringDisplay {
                    primaries: [[gx, gy], [bx, by], [rx, ry]],
                    white_point: [wx, wy],
                    max_luminance: max,
                    min_luminance: min,
                });
            }
        }
        "Content light level metadata" => {
            if let (Some(a), Some(b)) = (num(sd, "max_content"), num(sd, "max_average")) {
                hdr.cll = Some(ContentLightLevel {
                    max_cll: a as u16,
                    max_fall: b as u16,
                });
            }
        }
        t if t.contains("SMPTE2094-40") || t.contains("HDR10+") => *plus = true,
        t if t.contains("Dolby Vision RPU") || t.contains("Dolby Vision Metadata") => *rpu = true,
        _ => {}
    }
}

fn format_of(color: Option<ColorInfo>, dovi: bool, plus: bool) -> HdrFormat {
    let tr = color.and_then(|c| c.transfer);
    if dovi {
        HdrFormat::DolbyVision
    } else if tr == Some(crate::hdr::TRANSFER_PQ) {
        if plus {
            HdrFormat::Hdr10Plus
        } else {
            HdrFormat::Hdr10
        }
    } else if tr == Some(crate::hdr::TRANSFER_HLG) {
        HdrFormat::Hlg
    } else {
        HdrFormat::Sdr
    }
}

fn rational(x: &str) -> Option<Rational> {
    let (n, d) = x.split_once('/')?;
    let (num, den): (u32, u32) = (n.parse().ok()?, d.parse().ok()?);
    (num > 0 && den > 0).then_some(Rational { num, den })
}

/// ffprobe's `"32:27"` (absent, `"N/A"` or `"0:1"` when unknown).
fn aspect(x: &str) -> Option<Rational> {
    let (n, d) = x.split_once(':')?;
    let (num, den): (u32, u32) = (n.parse().ok()?, d.parse().ok()?);
    (num > 0 && den > 0).then_some(Rational { num, den })
}

/// Map ffprobe's `-show_format -show_streams -show_chapters` JSON. `ext` (the
/// file extension) disambiguates what ffprobe lumps together (MKV/WebM,
/// TS/M2TS).
pub fn from_json(v: &Value, ext: &str) -> ProbeResult {
    let fmt = v.get("format").cloned().unwrap_or(Value::Null);
    let fname = s(&fmt, "format_name").unwrap_or("");
    let ext = ext.to_ascii_lowercase();
    let container = if fname.contains("matroska") {
        if ext == "webm" {
            Container::WebM
        } else {
            Container::Matroska
        }
    } else if fname.starts_with("mov") {
        let brand = fmt.get("tags").and_then(|t| s(t, "major_brand")).unwrap_or("");
        if brand.trim() == "qt" {
            Container::Mov
        } else {
            Container::Mp4
        }
    } else if fname == "mpegts" {
        if matches!(ext.as_str(), "m2ts" | "mts" | "m2t") {
            Container::M2ts
        } else {
            Container::MpegTs
        }
    } else if fname == "avi" {
        Container::Avi
    } else {
        Container::Unknown
    };
    let mut r = empty_result(container);
    r.duration_ms = num(&fmt, "duration").map(|d| (d * 1000.0).round() as u64);
    r.bitrate = num(&fmt, "bit_rate").map(|b| b as u64);
    r.title = fmt.get("tags").and_then(|t| s(t, "title")).map(str::to_string);

    for st in v.get("streams").and_then(Value::as_array).into_iter().flatten() {
        let ctype = s(st, "codec_type").unwrap_or("");
        let disp = st.get("disposition").cloned().unwrap_or(Value::Null);
        let flag = |k: &str| num(&disp, k).is_some_and(|x| x != 0.0);
        let tags = st.get("tags").cloned().unwrap_or(Value::Null);
        if ctype == "attachment" || flag("attached_pic") {
            r.attachments.push(Attachment {
                name: s(&tags, "filename").unwrap_or("").to_string(),
                mime: s(&tags, "mimetype").unwrap_or("").to_string(),
                size: num(st, "extradata_size").unwrap_or(0.0) as u64,
            });
            continue;
        }
        let kind = match ctype {
            "video" => TrackKind::Video,
            "audio" => TrackKind::Audio,
            "subtitle" => TrackKind::Subtitle,
            _ => TrackKind::Data,
        };
        let name = s(st, "codec_name").unwrap_or("");
        let id = s(st, "id")
            .and_then(|x| u64::from_str_radix(x.trim_start_matches("0x"), 16).ok())
            .or_else(|| num(st, "index").map(|i| i as u64))
            .unwrap_or(0);
        let mut t: Track = new_track(r.tracks.len() as u32, id, kind, codec_of(name), name);
        t.profile = s(st, "profile").map(str::to_string);
        t.level = num(st, "level").filter(|l| *l >= 0.0).map(|l| l as u16);
        t.language = s(&tags, "language").map(str::to_string);
        t.title = s(&tags, "title").map(str::to_string);
        t.default = flag("default");
        t.forced = flag("forced");
        t.hearing_impaired = flag("hearing_impaired");
        t.bitrate = num(st, "bit_rate").map(|b| b as u64);
        match kind {
            TrackKind::Video => {
                t.width = num(st, "width").map(|w| w as u32);
                t.height = num(st, "height").map(|h| h as u32);
                t.sample_aspect_ratio = s(st, "sample_aspect_ratio").and_then(aspect);
                t.bit_depth = bit_depth_of(st);
                t.frame_rate = s(st, "r_frame_rate").and_then(rational);
                t.interlaced = s(st, "field_order").map(|f| f != "progressive" && f != "unknown");
                t.color = color_of(st);
                let mut hdr = HdrInfo {
                    format: HdrFormat::Sdr,
                    mastering: None,
                    cll: None,
                    dovi: None,
                };
                let (mut plus, mut rpu) = (false, false);
                for sd in st
                    .get("side_data_list")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    side_data(sd, &mut hdr, &mut plus, &mut rpu);
                }
                hdr.format = format_of(t.color, hdr.dovi.is_some() || rpu, plus);
                t.hdr = Some(hdr);
            }
            TrackKind::Audio => {
                t.channels = num(st, "channels").map(|c| c as u8);
                t.sample_rate = num(st, "sample_rate").map(|x| x as u32);
                t.bit_depth = num(st, "bits_per_raw_sample")
                    .filter(|b| *b > 0.0)
                    .map(|b| b as u8);
            }
            _ => {}
        }
        r.tracks.push(t);
    }
    for ch in v.get("chapters").and_then(Value::as_array).into_iter().flatten() {
        r.chapters.push(Chapter {
            start_ms: num(ch, "start_time").map_or(0, |x| (x * 1000.0).round() as u64),
            end_ms: num(ch, "end_time").map(|x| (x * 1000.0).round() as u64),
            title: ch.get("tags").and_then(|t| s(t, "title")).map(str::to_string),
        });
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_minimal_json() {
        let v: Value = serde_json::from_str(
            r#"{"streams":[{"index":0,"codec_name":"hevc","codec_type":"video","width":3840,"height":2160,
                "pix_fmt":"yuv420p10le","color_transfer":"smpte2084","color_primaries":"bt2020",
                "sample_aspect_ratio":"32:27",
                "disposition":{"default":1,"forced":0}},
               {"index":1,"codec_name":"eac3","codec_type":"audio","channels":6,"sample_rate":"48000",
                "sample_aspect_ratio":"1:1",
                "tags":{"language":"eng"}}],
               "format":{"format_name":"matroska,webm","duration":"30.005000","bit_rate":"100"}}"#,
        )
        .unwrap();
        let r = from_json(&v, "mkv");
        assert_eq!(r.container, Container::Matroska);
        assert_eq!(r.duration_ms, Some(30005));
        assert_eq!(r.tracks[0].bit_depth, Some(10));
        assert_eq!(r.tracks[0].hdr.as_ref().map(|h| h.format), Some(HdrFormat::Hdr10));
        assert_eq!(
            r.tracks[0].sample_aspect_ratio,
            Some(Rational { num: 32, den: 27 })
        );
        assert_eq!(r.tracks[1].sample_aspect_ratio, None, "audio has no SAR");
        assert_eq!(aspect("0:1"), None);
        assert_eq!(aspect("N/A"), None);
        assert_eq!(r.tracks[1].channels, Some(6));
        assert_eq!(r.tracks[1].language.as_deref(), Some("eng"));
    }
}
