//! Playback facts derived from a probe at scan time (research/03 §11): the
//! asset's summary columns, its human label ("4K HEVC HDR10 · TrueHD 7.1")
//! and quality badges ("4K", "HDR10", "DV", "Atmos", "5.1").
//!
//! Computed once when a file is (re)probed and stored on the asset, so lists
//! and detail pages never parse probe JSON.

use flux_core::{Codec, Container, HdrFormat, NameGuess, ProbeResult, Track};

/// Denormalised asset columns.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AssetFacts {
    pub container: Option<Container>,
    pub duration_ms: Option<u64>,
    pub video_codec: Option<Codec>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub hdr: Option<HdrFormat>,
    /// "TrueHD 7.1, AAC 2.0"
    pub audio_summary: Option<String>,
    pub label: String,
    pub badges: Vec<String>,
}

/// Derive asset facts from a probe, falling back to the name guess
/// (`resolution`) when the probe is missing or failed.
pub fn asset_facts(probe: Option<&ProbeResult>, guess: Option<&NameGuess>) -> AssetFacts {
    let mut f = AssetFacts::default();
    let mut label: Vec<String> = Vec::new();
    let mut badges: Vec<String> = Vec::new();

    let video = probe.and_then(|p| p.main_video());
    if let Some(p) = probe {
        f.container = Some(p.container);
        f.duration_ms = p.duration_ms;
    }
    let res = match video {
        Some(v) => resolution_badge(v.width, v.height),
        None => guess
            .and_then(|g| g.resolution.as_deref())
            .and_then(resolution_from_guess),
    };
    if let Some(r) = res {
        label.push(r.to_string());
        badges.push(r.to_string());
    }
    if let Some(v) = video {
        f.video_codec = Some(v.codec);
        f.width = v.width;
        f.height = v.height;
        label.push(video_codec_name(v.codec, v.codec_tag.as_str()).to_string());
        f.hdr = hdr_of(v);
        if let Some(h) = f.hdr.filter(|h| *h != HdrFormat::Sdr) {
            label.push(hdr_name(h).to_string());
            badges.push(hdr_name(h).to_string());
        }
    }

    let audio: Vec<&Track> = probe.map(|p| p.audio().collect()).unwrap_or_default();
    if !audio.is_empty() {
        let summary: Vec<String> = audio.iter().map(|a| audio_name(a)).collect();
        f.audio_summary = Some(dedup_join(&summary));
        // Label and badges describe the best audio track (most channels;
        // the default track wins ties).
        if let Some(best) = audio
            .iter()
            .max_by_key(|a| (a.channels.unwrap_or(0), a.default, std::cmp::Reverse(a.index)))
        {
            label.push(format!("· {}", audio_name(best)));
            if audio.iter().any(|a| is_atmos(a)) {
                badges.push("Atmos".into());
            }
            match best.channels {
                Some(8) => badges.push("7.1".into()),
                Some(6) => badges.push("5.1".into()),
                _ => {}
            }
        }
    }
    f.label = label.join(" ");
    f.badges = badges;
    f
}

fn dedup_join(v: &[String]) -> String {
    let mut out: Vec<&str> = Vec::new();
    for s in v {
        if !out.contains(&s.as_str()) {
            out.push(s);
        }
    }
    out.join(", ")
}

fn resolution_badge(w: Option<u32>, h: Option<u32>) -> Option<&'static str> {
    let (w, h) = (w.unwrap_or(0), h.unwrap_or(0));
    if w == 0 && h == 0 {
        return None;
    }
    // Width-first so letterboxed scope encodes (3840×1600) still count as 4K.
    Some(if w >= 7000 || h >= 4000 {
        "8K"
    } else if w >= 3200 || h >= 1800 {
        "4K"
    } else if w >= 1800 || h >= 1000 {
        "1080p"
    } else if w >= 1200 || h >= 700 {
        "720p"
    } else {
        "SD"
    })
}

fn resolution_from_guess(r: &str) -> Option<&'static str> {
    match r.to_ascii_lowercase().as_str() {
        "4320p" | "8k" => Some("8K"),
        "2160p" | "4k" | "uhd" => Some("4K"),
        "1080p" | "1080i" => Some("1080p"),
        "720p" => Some("720p"),
        "576p" | "576i" | "480p" | "480i" | "360p" | "sd" => Some("SD"),
        _ => None,
    }
}

fn hdr_of(v: &Track) -> Option<HdrFormat> {
    if let Some(h) = &v.hdr {
        if h.dovi.is_some() {
            return Some(HdrFormat::DolbyVision);
        }
        return Some(h.format);
    }
    // Container colour info without an HdrInfo: infer from the transfer
    // function (H.273: 16 = PQ, 18 = HLG).
    match v.color.and_then(|c| c.transfer) {
        Some(16) => Some(HdrFormat::Hdr10),
        Some(18) => Some(HdrFormat::Hlg),
        _ => None,
    }
}

/// Badge/label text for an HDR format.
pub fn hdr_name(h: HdrFormat) -> &'static str {
    match h {
        HdrFormat::Sdr => "SDR",
        HdrFormat::Hdr10 => "HDR10",
        HdrFormat::Hdr10Plus => "HDR10+",
        HdrFormat::Hlg => "HLG",
        HdrFormat::DolbyVision => "DV",
    }
}

fn video_codec_name(c: Codec, tag: &str) -> &str {
    match c {
        Codec::H264 => "H.264",
        Codec::Hevc => "HEVC",
        Codec::Av1 => "AV1",
        Codec::Vp9 => "VP9",
        Codec::Vp8 => "VP8",
        Codec::Mpeg2Video => "MPEG-2",
        Codec::Mpeg4Part2 => "MPEG-4",
        Codec::Vc1 => "VC-1",
        Codec::ProRes => "ProRes",
        Codec::Mjpeg => "MJPEG",
        Codec::Mpeg1Video
        | Codec::Wmv1
        | Codec::Wmv2
        | Codec::Msmpeg4
        | Codec::Vp6
        | Codec::Flv1
        | Codec::RealVideo => c.display_name(),
        _ => tag,
    }
}

fn audio_codec_name(t: &Track) -> String {
    let name = match t.codec {
        Codec::Aac => "AAC",
        Codec::Ac3 => "AC3",
        Codec::Eac3 => "E-AC3",
        Codec::TrueHd => "TrueHD",
        // "DTS-HD MA", "DTS:X" … live in the profile.
        Codec::Dts => {
            return t
                .profile
                .as_deref()
                .filter(|p| p.starts_with("DTS"))
                .unwrap_or("DTS")
                .to_string();
        }
        Codec::Flac => "FLAC",
        Codec::Opus => "Opus",
        Codec::Vorbis => "Vorbis",
        Codec::Mp3 => "MP3",
        Codec::Mp2 => "MP2",
        Codec::Pcm => "PCM",
        Codec::Alac => "ALAC",
        Codec::Wma | Codec::WmaPro | Codec::RealAudio => t.codec.display_name(),
        _ => return t.codec_tag.clone(),
    };
    name.to_string()
}

fn channel_layout(ch: u8) -> String {
    match ch {
        1 => "1.0".into(),
        2 => "2.0".into(),
        3 => "2.1".into(),
        6 => "5.1".into(),
        7 => "6.1".into(),
        8 => "7.1".into(),
        n => format!("{n}ch"),
    }
}

fn audio_name(t: &Track) -> String {
    let mut s = audio_codec_name(t);
    if is_atmos(t) {
        s.push_str(" Atmos");
    }
    if let Some(ch) = t.channels {
        s.push(' ');
        s.push_str(&channel_layout(ch));
    }
    s
}

/// Atmos rides on TrueHD or E-AC-3 (JOC); headers only reveal it through the
/// profile/title, so this is best effort.
fn is_atmos(t: &Track) -> bool {
    matches!(t.codec, Codec::TrueHd | Codec::Eac3)
        && [t.profile.as_deref(), t.title.as_deref()]
            .into_iter()
            .flatten()
            .any(|s| s.to_ascii_lowercase().contains("atmos"))
}

/// Merge the badges of an item's assets into one canonical list: the best
/// resolution, every HDR format (DV first), Atmos, the best channel layout.
pub fn merge_badges<'a>(lists: impl IntoIterator<Item = &'a [String]>) -> Vec<String> {
    const RES: [&str; 5] = ["8K", "4K", "1080p", "720p", "SD"];
    const HDR: [&str; 4] = ["DV", "HDR10+", "HDR10", "HLG"];
    const CH: [&str; 2] = ["7.1", "5.1"];
    let all: Vec<&str> = lists
        .into_iter()
        .flat_map(|l| l.iter().map(String::as_str))
        .collect();
    let mut out = Vec::new();
    if let Some(r) = RES.iter().find(|r| all.contains(r)) {
        out.push(r.to_string());
    }
    out.extend(HDR.iter().filter(|h| all.contains(h)).map(|h| h.to_string()));
    if all.contains(&"Atmos") {
        out.push("Atmos".into());
    }
    if let Some(c) = CH.iter().find(|c| all.contains(c)) {
        out.push(c.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use flux_core::{ColorInfo, DoviConfig, HdrInfo, TrackKind};

    pub(crate) fn track(kind: TrackKind, codec: Codec) -> Track {
        Track {
            index: 0,
            id: 1,
            kind,
            codec,
            codec_tag: format!("{codec:?}"),
            profile: None,
            level: None,
            bit_depth: None,
            width: None,
            height: None,
            frame_rate: None,
            interlaced: None,
            color: None,
            hdr: None,
            channels: None,
            sample_rate: None,
            language: None,
            title: None,
            default: false,
            forced: false,
            hearing_impaired: false,
            bitrate: None,
            sample_aspect_ratio: None,
        }
    }

    fn probe(tracks: Vec<Track>) -> ProbeResult {
        ProbeResult {
            container: Container::Matroska,
            duration_ms: Some(1000),
            bitrate: None,
            title: None,
            tracks,
            chapters: vec![],
            attachments: vec![],
            has_index: true,
            faststart: None,
            needs_frame_sei: false,
            warnings: vec![],
            probe_version: 1,
            bytes_read: 0,
            reads: 0,
        }
    }

    #[test]
    fn uhd_hdr10_truehd_label_and_badges() {
        let mut v = track(TrackKind::Video, Codec::Hevc);
        v.width = Some(3840);
        v.height = Some(2160);
        v.hdr = Some(HdrInfo {
            format: HdrFormat::Hdr10,
            mastering: None,
            cll: None,
            dovi: None,
        });
        let mut a = track(TrackKind::Audio, Codec::TrueHd);
        a.channels = Some(8);
        a.index = 1;
        a.title = Some("TrueHD Atmos 7.1".into());
        let mut b = track(TrackKind::Audio, Codec::Aac);
        b.channels = Some(2);
        b.index = 2;
        let f = asset_facts(Some(&probe(vec![v, a, b])), None);
        assert_eq!(f.label, "4K HEVC HDR10 · TrueHD Atmos 7.1");
        assert_eq!(f.badges, vec!["4K", "HDR10", "Atmos", "7.1"]);
        assert_eq!(f.audio_summary.as_deref(), Some("TrueHD Atmos 7.1, AAC 2.0"));
        assert_eq!(f.hdr, Some(HdrFormat::Hdr10));
    }

    #[test]
    fn dovi_config_wins_and_pq_transfer_implies_hdr10() {
        let mut v = track(TrackKind::Video, Codec::Hevc);
        v.width = Some(3840);
        v.height = Some(1600);
        v.hdr = Some(HdrInfo {
            format: HdrFormat::Hdr10,
            mastering: None,
            cll: None,
            dovi: Some(DoviConfig {
                profile: 8,
                level: 6,
                rpu_present: true,
                el_present: false,
                bl_present: true,
                bl_signal_compat_id: 1,
            }),
        });
        let f = asset_facts(Some(&probe(vec![v])), None);
        assert_eq!(f.badges, vec!["4K", "DV"]);

        let mut v = track(TrackKind::Video, Codec::Hevc);
        v.width = Some(1920);
        v.height = Some(1080);
        v.color = Some(ColorInfo {
            primaries: Some(9),
            transfer: Some(16),
            matrix: Some(9),
            full_range: None,
        });
        assert_eq!(
            asset_facts(Some(&probe(vec![v])), None).hdr,
            Some(HdrFormat::Hdr10)
        );
    }

    #[test]
    fn falls_back_to_guess_resolution_without_probe() {
        let mut g = NameGuess::unknown();
        g.resolution = Some("2160p".into());
        let f = asset_facts(None, Some(&g));
        assert_eq!(f.label, "4K");
        assert_eq!(f.badges, vec!["4K"]);
    }

    #[test]
    fn merge_keeps_best_resolution_and_union_of_hdr() {
        let a = vec!["1080p".to_string(), "5.1".to_string()];
        let b = vec!["4K".to_string(), "DV".to_string(), "HDR10".to_string()];
        let c = vec!["720p".to_string(), "Atmos".to_string(), "7.1".to_string()];
        assert_eq!(
            merge_badges([a.as_slice(), b.as_slice(), c.as_slice()]),
            vec!["4K", "DV", "HDR10", "Atmos", "7.1"]
        );
    }
}
