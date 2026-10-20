//! Fixtures for tests and the `catalog-bench` binary (not used by the
//! server): turn the test corpus's ground truth into scanner input.
//!
//! - [`probe_from_ffprobe`]: an ffprobe `-of json` document → [`ProbeResult`]
//!   (the subset of fields the catalog uses; a stand-in until flux-probe).
//! - [`seed_probes`]: probes of the 7 seed files the 16.7k-file library was
//!   cloned from (`fixtures/seeds.ffprobe.json`, captured with ffprobe 8).
//! - [`ManifestEntry`] / [`guess_from_manifest`]: the library manifest's
//!   ground truth as a [`NameGuess`], i.e. a perfect name parser.

use std::collections::HashMap;
use std::path::Path;

use flux_core::{
    Codec, ColorInfo, Container, ExtraKind, GuessKind, HdrFormat, HdrInfo, NameGuess, ProbeResult,
    ProviderIds, Rational, Track, TrackKind, PROBE_VERSION,
};
use serde::Deserialize;
use serde_json::Value;

use crate::error::Result;
use crate::model::FileFacts;

const SEEDS_JSON: &str = include_str!("../fixtures/seeds.ffprobe.json");

fn codec_of(name: &str) -> Codec {
    match name {
        "h264" => Codec::H264,
        "hevc" => Codec::Hevc,
        "av1" => Codec::Av1,
        "vp9" => Codec::Vp9,
        "vp8" => Codec::Vp8,
        "mpeg2video" => Codec::Mpeg2Video,
        "mpeg4" => Codec::Mpeg4Part2,
        "vc1" => Codec::Vc1,
        "prores" => Codec::ProRes,
        "mjpeg" => Codec::Mjpeg,
        "aac" => Codec::Aac,
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
        "subrip" => Codec::Subrip,
        "ass" => Codec::Ass,
        "ssa" => Codec::Ssa,
        "webvtt" => Codec::WebVtt,
        "mov_text" => Codec::MovText,
        "hdmv_pgs_subtitle" => Codec::Pgs,
        "dvd_subtitle" => Codec::VobSub,
        "dvb_subtitle" => Codec::DvbSub,
        n if n.starts_with("pcm_") => Codec::Pcm,
        _ => Codec::Other,
    }
}

fn container_of(format_name: &str, file_name: &str) -> Container {
    let ext = file_name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    if format_name.contains("matroska") {
        if ext == "webm" {
            Container::WebM
        } else {
            Container::Matroska
        }
    } else if format_name.contains("mp4") {
        if ext == "mov" {
            Container::Mov
        } else {
            Container::Mp4
        }
    } else if format_name == "mpegts" {
        if ext == "m2ts" {
            Container::M2ts
        } else {
            Container::MpegTs
        }
    } else if format_name == "avi" {
        Container::Avi
    } else {
        Container::Unknown
    }
}

fn num<T: std::str::FromStr>(v: &Value) -> Option<T> {
    match v {
        Value::Number(n) => n.to_string().parse().ok(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Convert an ffprobe JSON document (`-show_format -show_streams`) to a
/// [`ProbeResult`]. `file_name` disambiguates WebM/MOV/M2TS.
pub fn probe_from_ffprobe(doc: &Value, file_name: &str, size: u64) -> ProbeResult {
    let format = &doc["format"];
    let duration_ms = num::<f64>(&format["duration"]).map(|d| (d * 1000.0).round() as u64);
    let mut tracks = Vec::new();
    for (i, s) in doc["streams"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .enumerate()
    {
        let codec_name = s["codec_name"].as_str().unwrap_or("");
        let kind = match s["codec_type"].as_str().unwrap_or("") {
            "video" => TrackKind::Video,
            "audio" => TrackKind::Audio,
            "subtitle" => TrackKind::Subtitle,
            "attachment" => TrackKind::Attachment,
            _ => TrackKind::Data,
        };
        let transfer = match s["color_transfer"].as_str() {
            Some("smpte2084") => Some(16),
            Some("arib-std-b67") => Some(18),
            Some("bt709") => Some(1),
            _ => None,
        };
        let hdr = match (kind, transfer) {
            (TrackKind::Video, Some(16)) => Some(HdrInfo {
                format: HdrFormat::Hdr10,
                mastering: None,
                cll: None,
                dovi: None,
            }),
            (TrackKind::Video, Some(18)) => Some(HdrInfo {
                format: HdrFormat::Hlg,
                mastering: None,
                cll: None,
                dovi: None,
            }),
            _ => None,
        };
        let frame_rate = s["r_frame_rate"].as_str().and_then(|r| {
            let (n, d) = r.split_once('/')?;
            let (num, den) = (n.parse().ok()?, d.parse().ok()?);
            (den != 0 && num != 0).then_some(Rational { num, den })
        });
        let pix = s["pix_fmt"].as_str().unwrap_or("");
        tracks.push(Track {
            index: i as u32,
            id: num(&s["index"]).unwrap_or(i as u64),
            kind,
            codec: codec_of(codec_name),
            codec_tag: codec_name.to_string(),
            profile: s["profile"].as_str().map(str::to_string),
            level: num(&s["level"]),
            bit_depth: (kind == TrackKind::Video).then(|| if pix.contains("10") { 10 } else { 8 }),
            width: num(&s["width"]),
            height: num(&s["height"]),
            frame_rate,
            interlaced: None,
            color: transfer.map(|t| ColorInfo {
                primaries: None,
                transfer: Some(t),
                matrix: None,
                full_range: None,
            }),
            hdr,
            channels: num(&s["channels"]),
            sample_rate: num(&s["sample_rate"]),
            language: s["tags"]["language"]
                .as_str()
                .filter(|l| *l != "und")
                .map(str::to_string),
            title: s["tags"]["title"].as_str().map(str::to_string),
            default: i < 2,
            forced: false,
            hearing_impaired: false,
            bitrate: num(&s["bit_rate"]),
            // ffprobe's "32:27"; unknown is absent or "0:1".
            sample_aspect_ratio: s["sample_aspect_ratio"].as_str().and_then(|r| {
                let (n, d) = r.split_once(':')?;
                let (num, den) = (n.parse().ok()?, d.parse().ok()?);
                (den != 0 && num != 0).then_some(Rational { num, den })
            }),
        });
    }
    let container = container_of(format["format_name"].as_str().unwrap_or(""), file_name);
    ProbeResult {
        container,
        duration_ms,
        bitrate: num(&format["bit_rate"]).or_else(|| {
            duration_ms
                .filter(|d| *d > 0)
                .map(|d| size.saturating_mul(8000) / d)
        }),
        title: None,
        tracks,
        chapters: Vec::new(),
        attachments: Vec::new(),
        has_index: !matches!(container, Container::MpegTs | Container::M2ts),
        faststart: None,
        needs_frame_sei: false,
        warnings: Vec::new(),
        probe_version: PROBE_VERSION,
        bytes_read: 0,
        reads: 0,
    }
}

/// Probes of the corpus seed files, keyed by seed name (`h264_mkv`, …).
pub fn seed_probes() -> HashMap<String, ProbeResult> {
    let doc: Value = match serde_json::from_str(SEEDS_JSON) {
        Ok(v) => v,
        Err(_) => return HashMap::new(),
    };
    let mut out = HashMap::new();
    if let Some(map) = doc.as_object() {
        for (name, d) in map {
            let ext = match name.rsplit('_').next() {
                Some("tail") => "mp4",
                Some(e) => e,
                None => "",
            };
            let size = num(&d["bytes"]).unwrap_or(0);
            out.insert(name.clone(), probe_from_ffprobe(d, &format!("seed.{ext}"), size));
        }
    }
    out
}

/// One line of `library.manifest.jsonl` (the corpus ground truth).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ManifestEntry {
    pub path: String,
    pub kind: String,
    pub title: Option<String>,
    pub show: Option<String>,
    pub parent_title: Option<String>,
    pub year: Option<u16>,
    pub season: Option<u16>,
    #[serde(default)]
    pub episodes: Vec<u16>,
    pub absolute: Option<u32>,
    pub edition: Option<String>,
    pub part: Option<u8>,
    pub tmdb_id: Option<u64>,
    pub resolution: Option<String>,
    pub seed: Option<String>,
    pub group: Option<String>,
}

impl ManifestEntry {
    /// Movie, episode, extra or sample (the files a scanner hands the catalog).
    pub fn is_media(&self) -> bool {
        matches!(self.kind.as_str(), "movie" | "episode" | "extra" | "sample")
    }
}

/// Read a `.manifest.jsonl` file.
pub fn read_manifest(path: &Path) -> Result<Vec<ManifestEntry>> {
    let text = std::fs::read_to_string(path)?;
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| Ok(serde_json::from_str(l)?))
        .collect()
}

/// The ground truth as a name guess (a perfect parser). `None` for non-media.
pub fn guess_from_manifest(e: &ManifestEntry) -> Option<NameGuess> {
    let mut g = NameGuess::unknown();
    g.confidence = 1.0;
    g.evidence = vec!["manifest ground truth".into()];
    g.year = e.year;
    g.resolution = e.resolution.clone();
    g.release_group = e.group.clone();
    match e.kind.as_str() {
        "movie" => {
            g.kind = GuessKind::Movie;
            g.title = e.title.clone();
            g.edition = e.edition.clone();
            g.part = e.part;
            g.ids = ProviderIds {
                tmdb: e.tmdb_id,
                imdb: None,
                tvdb: None,
            };
        }
        "episode" => {
            g.kind = GuessKind::Episode;
            g.title = e.show.clone();
            g.season = e.season;
            g.episodes = e.episodes.clone();
            g.absolute_episode = e.absolute;
        }
        "extra" => {
            g.kind = GuessKind::Extra;
            g.title = e.parent_title.clone();
            let folder = e.path.rsplit('/').nth(1).unwrap_or("").to_ascii_lowercase();
            g.extra = Some(match folder.as_str() {
                "trailers" => ExtraKind::Trailer,
                "featurettes" => ExtraKind::Featurette,
                "behind the scenes" => ExtraKind::BehindTheScenes,
                "deleted scenes" => ExtraKind::DeletedScene,
                "interviews" => ExtraKind::Interview,
                "shorts" => ExtraKind::Short,
                "scenes" => ExtraKind::Scene,
                _ => ExtraKind::Other,
            });
        }
        "sample" => g.kind = GuessKind::Sample,
        _ => return None,
    }
    Some(g)
}

/// A probe for a manifest entry: its seed's probe with the picture size the
/// release name claims (the corpus files are clones of 720p seeds).
pub fn probe_for_entry(e: &ManifestEntry, seeds: &HashMap<String, ProbeResult>) -> Option<ProbeResult> {
    let mut p = seeds.get(e.seed.as_deref()?)?.clone();
    let (w, h) = match e.resolution.as_deref() {
        Some("2160p") => (3840, 2160),
        Some("1080p") => (1920, 1080),
        Some("720p") => (1280, 720),
        Some("576p") => (720, 576),
        Some("480p") => (720, 480),
        _ => return Some(p),
    };
    for t in p.tracks.iter_mut().filter(|t| t.kind == TrackKind::Video) {
        t.width = Some(w);
        t.height = Some(h);
    }
    Some(p)
}

/// Stat facts from real file metadata (`dev`, `ino`, `btime`, ns mtime).
pub fn facts_from_metadata(rel_path: &str, md: &std::fs::Metadata) -> FileFacts {
    let btime_ns = md
        .created()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64);
    #[cfg(unix)]
    let (dev, ino, mtime_ns) = {
        use std::os::unix::fs::MetadataExt;
        let mtime_ns = md
            .mtime()
            .saturating_mul(1_000_000_000)
            .saturating_add(md.mtime_nsec());
        (Some(md.dev()), Some(md.ino()), mtime_ns)
    };
    // Not Unix: no device/inode numbers in std; the mtime from SystemTime.
    #[cfg(not(unix))]
    let (dev, ino, mtime_ns) = {
        let mtime_ns = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos() as i64);
        (None, None, mtime_ns)
    };
    FileFacts {
        rel_path: rel_path.to_string(),
        dev,
        ino,
        btime_ns,
        size: md.len(),
        mtime_ns,
        fp128: None,
        oshash: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_probes_parse() {
        let seeds = seed_probes();
        assert_eq!(seeds.len(), 7);
        let hevc = &seeds["hevc_mkv"];
        assert_eq!(hevc.container, Container::Matroska);
        assert_eq!(hevc.main_video().map(|v| v.codec), Some(Codec::Hevc));
        assert_eq!(hevc.audio().next().map(|a| a.codec), Some(Codec::Eac3));
        assert_eq!(seeds["h264_ts"].container, Container::MpegTs);
        assert_eq!(
            seeds["mpeg4_avi"].main_video().map(|v| v.codec),
            Some(Codec::Mpeg4Part2)
        );
        assert!(seeds["h264_mp4_tail"].duration_ms.is_some());
    }
}
