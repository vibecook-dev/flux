//! HLS playlists (RFC 8216bis; Apple HLS authoring spec) and the URL layout
//! of one playback plan.
//!
//! Layout, relative to `/media/{plan_id}/`:
//!
//! | Path | Resource |
//! |---|---|
//! | `master.m3u8` | multivariant playlist |
//! | `v/index.m3u8`, `v/init.mp4`, `v/{n}.m4s` | video rendition |
//! | `a/{key}/index.m3u8`, `a/{key}/init.mp4`, `a/{key}/{n}.m4s` | audio rendition |
//! | `s/{track}.m3u8`, `s/{track}.vtt` | WebVTT subtitle rendition |
//!
//! An audio `key` is the source track index (`"1"`) for a copied track, or
//! track + recipe for a transcode (`"1-aac2"`, `"1-eac36"`), because the same
//! source track can be offered both ways.

use std::fmt::Write as _;

use flux_core::api::AudioRef;
use flux_core::{AudioOp, AudioPlan, Codec, DeliveryFormat, PlaybackPlan, SubtitleOp, TrackKind};

use crate::asset::Asset;
use crate::index::{bcp47, IndexTrack};

/// What an audio rendition delivers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioRecipe {
    Copy,
    Aac { channels: u8 },
    Eac3 { channels: u8 },
}

/// Identifies one audio rendition of a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AudioKey {
    /// Source track index.
    pub track: u32,
    pub recipe: AudioRecipe,
}

impl AudioKey {
    /// The key for a plan entry.
    pub fn from_plan(a: &AudioPlan) -> Self {
        let recipe = match &a.op {
            AudioOp::Copy => AudioRecipe::Copy,
            AudioOp::Transcode {
                codec: Codec::Eac3,
                channels,
                ..
            } => AudioRecipe::Eac3 { channels: *channels },
            AudioOp::Transcode { channels, .. } => AudioRecipe::Aac { channels: *channels },
        };
        AudioKey {
            track: a.track,
            recipe,
        }
    }

    /// URL path segment: `"1"`, `"1-aac2"`, `"1-eac36"`.
    pub fn path(&self) -> String {
        match self.recipe {
            AudioRecipe::Copy => self.track.to_string(),
            AudioRecipe::Aac { channels } => format!("{}-aac{channels}", self.track),
            AudioRecipe::Eac3 { channels } => format!("{}-eac3{channels}", self.track),
        }
    }

    /// Parses [`AudioKey::path`].
    pub fn parse(s: &str) -> Option<Self> {
        let (track, recipe) = match s.split_once('-') {
            None => (s, AudioRecipe::Copy),
            Some((t, r)) => {
                let recipe = match r.strip_prefix("aac") {
                    Some(c) => AudioRecipe::Aac {
                        channels: c.parse().ok()?,
                    },
                    None => AudioRecipe::Eac3 {
                        channels: r.strip_prefix("eac3")?.parse().ok()?,
                    },
                };
                (t, recipe)
            }
        };
        Some(AudioKey {
            track: track.parse().ok()?,
            recipe,
        })
    }
}

/// A rendition of a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Rendition {
    Video,
    Audio(AudioKey),
    /// Source subtitle track index.
    Subtitle(u32),
}

/// A resource under `/media/{plan_id}/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HlsResource {
    Master,
    Playlist(Rendition),
    Init(Rendition),
    Segment(Rendition, u32),
    /// The WebVTT file of a subtitle track.
    WebVtt(u32),
    /// `v/iframes.m3u8`: the I-frame (trick play) playlist of a copied video.
    IFramePlaylist,
    /// `v/kf/{n}.m4s`: keyframe n alone.
    IFrame(u32),
}

impl HlsResource {
    /// Parses a path relative to `/media/{plan_id}/`.
    pub fn parse(path: &str) -> Option<Self> {
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let file = |f: &str, r: Rendition| -> Option<HlsResource> {
            match f {
                "index.m3u8" => Some(HlsResource::Playlist(r)),
                "init.mp4" => Some(HlsResource::Init(r)),
                _ => f
                    .strip_suffix(".m4s")?
                    .parse()
                    .ok()
                    .map(|n| HlsResource::Segment(r, n)),
            }
        };
        match parts.as_slice() {
            ["master.m3u8"] => Some(HlsResource::Master),
            ["v", "iframes.m3u8"] => Some(HlsResource::IFramePlaylist),
            ["v", "kf", f] => f.strip_suffix(".m4s")?.parse().ok().map(HlsResource::IFrame),
            ["v", f] => file(f, Rendition::Video),
            ["a", key, f] => file(f, Rendition::Audio(AudioKey::parse(key)?)),
            ["s", f] => {
                if let Some(t) = f.strip_suffix(".m3u8") {
                    Some(HlsResource::Playlist(Rendition::Subtitle(t.parse().ok()?)))
                } else {
                    f.strip_suffix(".vtt")?.parse().ok().map(HlsResource::WebVtt)
                }
            }
            _ => None,
        }
    }

    /// The relative path (inverse of [`HlsResource::parse`]).
    pub fn path(&self) -> String {
        let dir = |r: &Rendition| match r {
            Rendition::Video => "v".to_string(),
            Rendition::Audio(k) => format!("a/{}", k.path()),
            Rendition::Subtitle(t) => format!("s/{t}"),
        };
        match self {
            HlsResource::Master => "master.m3u8".into(),
            HlsResource::Playlist(Rendition::Subtitle(t)) => format!("s/{t}.m3u8"),
            HlsResource::Playlist(r) => format!("{}/index.m3u8", dir(r)),
            HlsResource::Init(r) => format!("{}/init.mp4", dir(r)),
            HlsResource::Segment(r, n) => format!("{}/{n}.m4s", dir(r)),
            HlsResource::WebVtt(t) => format!("s/{t}.vtt"),
            HlsResource::IFramePlaylist => "v/iframes.m3u8".into(),
            HlsResource::IFrame(n) => format!("v/kf/{n}.m4s"),
        }
    }
}

/// Media playlist of a video or audio rendition: exact EXTINF from the plan.
pub fn media_playlist(asset: &Asset) -> String {
    let plan = &asset.segments;
    let mut s = String::with_capacity(64 + plan.len() * 24);
    s.push_str("#EXTM3U\n#EXT-X-VERSION:7\n");
    let _ = writeln!(s, "#EXT-X-TARGETDURATION:{}", plan.target_duration());
    s.push_str("#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-INDEPENDENT-SEGMENTS\n");
    s.push_str("#EXT-X-MAP:URI=\"init.mp4\"\n");
    for n in 0..plan.len() {
        let _ = writeln!(s, "#EXTINF:{:.6},\n{n}.m4s", plan.duration_secs(n));
    }
    s.push_str("#EXT-X-ENDLIST\n");
    s
}

/// Media playlist of a transcoded video rendition: its pieces (see
/// [`crate::vtranscode`]) instead of the remux segments.
pub fn pieces_playlist(pieces: &[crate::vtranscode::Piece], timescale: u32) -> String {
    let ts = timescale.max(1) as f64;
    let dur = |p: &crate::vtranscode::Piece| (p.end_ticks - p.start_ticks) as f64 / ts;
    let target = pieces
        .iter()
        .map(|p| dur(p).round() as u64)
        .max()
        .unwrap_or(1)
        .max(1);
    let mut s = String::with_capacity(64 + pieces.len() * 24);
    s.push_str("#EXTM3U\n#EXT-X-VERSION:7\n");
    let _ = writeln!(s, "#EXT-X-TARGETDURATION:{target}");
    s.push_str("#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-INDEPENDENT-SEGMENTS\n");
    s.push_str("#EXT-X-MAP:URI=\"init.mp4\"\n");
    for (n, p) in pieces.iter().enumerate() {
        let _ = writeln!(s, "#EXTINF:{:.6},\n{n}.m4s", dur(p));
    }
    s.push_str("#EXT-X-ENDLIST\n");
    s
}

/// I-frame playlist (`EXT-X-I-FRAMES-ONLY`, RFC 8216 §4.3.3.6) of a copied
/// video: one segment per source keyframe, shown until the next one. Lets
/// players scrub with real frames (AVPlayer trick play, hls.js
/// `createIFramePlayer`) at no transcode cost: the index already lists every
/// keyframe.
pub fn iframe_playlist(asset: &Asset) -> String {
    let kfs = &asset.index.keyframes;
    let secs = |i: usize| -> f64 {
        let a = kfs[i].pts_ns;
        // The last keyframe is shown until the video ends (its last
        // segment's end), not the container's duration.
        let b = kfs.get(i + 1).map(|k| k.pts_ns).unwrap_or_else(|| {
            asset
                .segments
                .segments
                .last()
                .map_or(asset.index.duration_ns, |s| s.end_ns)
                .max(a + 1)
        });
        (b - a) as f64 / 1e9
    };
    let target = (0..kfs.len())
        .map(|i| secs(i).ceil() as u64)
        .max()
        .unwrap_or(1)
        .max(1);
    let mut s = String::with_capacity(96 + kfs.len() * 28);
    s.push_str("#EXTM3U\n#EXT-X-VERSION:7\n");
    let _ = writeln!(s, "#EXT-X-TARGETDURATION:{target}");
    s.push_str("#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-I-FRAMES-ONLY\n#EXT-X-INDEPENDENT-SEGMENTS\n");
    s.push_str("#EXT-X-MAP:URI=\"init.mp4\"\n");
    for i in 0..kfs.len() {
        let _ = writeln!(s, "#EXTINF:{:.6},\nkf/{i}.m4s", secs(i));
    }
    s.push_str("#EXT-X-ENDLIST\n");
    s
}

/// Single-segment WebVTT playlist for a subtitle track (VOD; Apple rule 5.7
/// allows the long target duration).
pub fn subtitle_playlist(asset: &Asset, track: u32) -> String {
    let total: f64 = (0..asset.segments.len())
        .map(|n| asset.segments.duration_secs(n))
        .sum();
    format!(
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{}\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXTINF:{total:.6},\n{track}.vtt\n#EXT-X-ENDLIST\n",
        total.ceil().max(1.0) as u64
    )
}

/// HLS `CODECS` entry of the video rendition.
pub fn video_codec_string(asset: &Asset) -> Option<String> {
    let v = asset.index.video_track()?;
    let entry = asset.video_entry().ok()?;
    let crate::fmp4::EntrySpec::Video { fourcc, .. } = entry else {
        return None;
    };
    let fourcc = String::from_utf8_lossy(&fourcc).to_string();
    let base = v.codec_string.clone()?;
    Some(match fourcc.as_str() {
        // Same profile/level part, named after the entry written (the
        // source's may differ: a `hev1` file remuxes to `hvc1`).
        "hvc1" | "hev1" | "avc1" | "avc3" => format!("{fourcc}{}", base.get(4..).unwrap_or_default()),
        "dvh1" | "dvhe" => {
            let d = v.hdr.as_ref().and_then(|h| h.dovi)?;
            format!("{fourcc}.{:02}.{:02}", d.profile, d.level)
        }
        _ => base,
    })
}

/// `CODECS` entry and channel count of an audio rendition.
pub fn audio_codec_string(t: &IndexTrack, recipe: AudioRecipe) -> (String, u8) {
    match recipe {
        AudioRecipe::Copy => (
            t.codec_string.clone().unwrap_or_else(|| "mp4a.40.2".into()),
            t.channels.unwrap_or(2),
        ),
        AudioRecipe::Aac { channels } => ("mp4a.40.2".into(), channels),
        AudioRecipe::Eac3 { channels } => ("ec-3".into(), channels),
    }
}

fn quote(s: &str) -> String {
    s.replace('"', "'").replace(['\n', '\r'], " ")
}

fn audio_name(t: &IndexTrack, recipe: AudioRecipe) -> String {
    let lang = t.language.as_deref().map(language_name).unwrap_or("Unknown");
    let base = t
        .name
        .clone()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| lang.to_string());
    let layout = |ch: u8| match ch {
        1 => "Mono".to_string(),
        2 => "Stereo".to_string(),
        6 => "5.1".to_string(),
        8 => "7.1".to_string(),
        n => format!("{n} ch"),
    };
    match recipe {
        AudioRecipe::Copy => format!(
            "{base} ({} {})",
            codec_label(t.codec),
            layout(t.channels.unwrap_or(2))
        ),
        AudioRecipe::Aac { channels } => format!("{base} (AAC {})", layout(channels)),
        AudioRecipe::Eac3 { channels } => format!("{base} (E-AC-3 {})", layout(channels)),
    }
}

/// Short human codec name for rendition labels.
pub fn codec_label(c: Codec) -> &'static str {
    match c {
        Codec::Aac => "AAC",
        Codec::Ac3 => "AC-3",
        Codec::Eac3 => "E-AC-3",
        Codec::TrueHd => "TrueHD",
        Codec::Dts => "DTS",
        Codec::Flac => "FLAC",
        Codec::Opus => "Opus",
        Codec::Mp3 => "MP3",
        Codec::Vorbis => "Vorbis",
        Codec::Pcm => "PCM",
        _ => "audio",
    }
}

fn language_name(code: &str) -> &str {
    match code {
        "eng" | "en" => "English",
        "jpn" | "ja" => "Japanese",
        "fre" | "fra" | "fr" => "French",
        "ger" | "deu" | "de" => "German",
        "spa" | "es" => "Spanish",
        "ita" | "it" => "Italian",
        other => other,
    }
}

/// The audio renditions of `plan`, in plan order, for
/// `PlayResponse::audio` (a player's own audio menu). `rendition` is the key
/// in `a/{rendition}/…` URLs and `label` the playlist's `NAME` (deduplicated
/// within each codec group exactly as [`master_playlist`] does). The first
/// entry is the planner's choice (`default`). Empty for progressive play.
pub fn audio_refs(asset: &Asset, plan: &PlaybackPlan) -> Vec<AudioRef> {
    if plan.delivery == DeliveryFormat::ProgressiveFile {
        return Vec::new();
    }
    // Per codec group (the master playlist's GROUP-ID), the names used so far.
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    let mut out = Vec::new();
    for (i, a) in plan.audio.iter().enumerate() {
        let Some(t) = asset.index.track(a.track) else {
            continue;
        };
        let key = AudioKey::from_plan(a);
        let (codec_string, channels) = audio_codec_string(t, key.recipe);
        let gi = match groups.iter().position(|g| g.0 == codec_string) {
            Some(gi) => gi,
            None => {
                groups.push((codec_string, Vec::new()));
                groups.len() - 1
            }
        };
        let names = &mut groups[gi].1;
        let mut label = audio_name(t, key.recipe);
        while names.contains(&label) {
            label.push('\'');
        }
        names.push(label.clone());
        out.push(AudioRef {
            rendition: key.path(),
            track: a.track,
            label,
            language: t.language.clone(),
            codec: match key.recipe {
                AudioRecipe::Copy => t.codec,
                AudioRecipe::Aac { .. } => Codec::Aac,
                AudioRecipe::Eac3 { .. } => Codec::Eac3,
            },
            channels,
            default: i == 0,
        });
    }
    out
}

/// Multivariant playlist for a plan: one video rendition, demuxed audio
/// renditions grouped by codec (one `EXT-X-STREAM-INF` per audio group, as
/// Apple's examples do, so a player that cannot decode one group's codec can
/// pick another), and WebVTT subtitle renditions.
pub fn master_playlist(asset: &Asset, plan: &PlaybackPlan) -> String {
    let mut s = String::from("#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-INDEPENDENT-SEGMENTS\n");
    let v = asset.index.video_track();

    // Audio groups, keyed by codec string: (codec, [(rendition, channels, bitrate)]).
    type Group = (String, Vec<(AudioKey, u8, u64)>);
    let mut groups: Vec<Group> = Vec::new();
    let default_track = plan.audio.first().map(|a| a.track);
    for a in &plan.audio {
        let Some(t) = asset.index.track(a.track) else {
            continue;
        };
        let key = AudioKey::from_plan(a);
        let (codec, ch) = audio_codec_string(t, key.recipe);
        let bitrate = match &a.op {
            AudioOp::Copy => t.bitrate.unwrap_or(256_000),
            AudioOp::Transcode { bitrate, .. } => *bitrate,
        };
        match groups.iter_mut().find(|g| g.0 == codec) {
            Some(g) => g.1.push((key, ch, bitrate)),
            None => groups.push((codec, vec![(key, ch, bitrate)])),
        }
    }
    for (gi, (_, members)) in groups.iter().enumerate() {
        let mut names: Vec<String> = Vec::new();
        let default_in_group = members
            .iter()
            .position(|m| Some(m.0.track) == default_track)
            .unwrap_or(0);
        for (mi, (key, ch, _)) in members.iter().enumerate() {
            let Some(t) = asset.index.track(key.track) else {
                continue;
            };
            let mut name = audio_name(t, key.recipe);
            while names.contains(&name) {
                name.push('\'');
            }
            names.push(name.clone());
            let _ = write!(
                s,
                "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a{gi}\",NAME=\"{}\"",
                quote(&name)
            );
            if let Some(l) = &t.language {
                let _ = write!(s, ",LANGUAGE=\"{}\"", bcp47(l));
            }
            let default = if mi == default_in_group { "YES" } else { "NO" };
            let _ = writeln!(
                s,
                ",DEFAULT={default},AUTOSELECT=YES,CHANNELS=\"{ch}\",URI=\"{}\"",
                HlsResource::Playlist(Rendition::Audio(*key)).path()
            );
        }
    }

    // Subtitles
    let subs: Vec<u32> = plan
        .subtitles
        .iter()
        .filter(|p| p.op == SubtitleOp::WebVtt)
        .map(|p| p.track)
        .collect();
    let mut sub_names: Vec<String> = Vec::new();
    for &track in &subs {
        let Some(t) = asset.index.track(track).filter(|t| t.kind == TrackKind::Subtitle) else {
            continue;
        };
        let lang = t.language.as_deref().map(language_name).unwrap_or("Unknown");
        let mut name = t
            .name
            .clone()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| lang.to_string());
        while sub_names.contains(&name) {
            name.push('\'');
        }
        sub_names.push(name.clone());
        let _ = write!(
            s,
            "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"{}\"",
            quote(&name)
        );
        if let Some(l) = &t.language {
            let _ = write!(s, ",LANGUAGE=\"{}\"", bcp47(l));
        }
        let _ = writeln!(
            s,
            ",DEFAULT=NO,AUTOSELECT={},FORCED={},URI=\"{}\"",
            if t.forced { "YES" } else { "NO" },
            if t.forced { "YES" } else { "NO" },
            HlsResource::Playlist(Rendition::Subtitle(track)).path()
        );
    }

    let vt = crate::vtranscode::VideoTarget::from_op(&plan.video);
    let (peak, avg) = match &vt {
        // Transcoded video: the encoder's rate (VBV max 1.5×).
        Some(t) => (t.bitrate * 3 / 2, t.bitrate),
        None => asset.segments.bandwidth(),
    };
    let vcodec = match &vt {
        Some(t) => Some(t.codec_string(crate::vtranscode::source_fps(asset))),
        None => video_codec_string(asset),
    };
    // A copied Dolby Vision stream's range is its base layer's (profile 5,
    // with no compatible base layer, is PQ): its colour fields may be unset.
    let dovi_range = v
        .filter(|_| vt.is_none())
        .and_then(|t| t.hdr.as_ref()?.dovi)
        .and_then(|d| match d.bl_signal_compat_id {
            0 | 1 | 6 => Some("PQ"),
            4 => Some("HLG"),
            _ => None,
        });
    let video_range = dovi_range.or_else(|| {
        v.and_then(|t| t.color).and_then(|c| match c.transfer {
            _ if vt.is_some_and(|t| t.tonemap) => None,
            Some(16) => Some("PQ"),
            Some(18) => Some("HLG"),
            _ => None,
        })
    });
    let resolution = match &vt {
        // The encoded size: upright for a rotated source (PB-9).
        Some(t) => Some(crate::vtranscode::output_size(asset, t)),
        None => v.and_then(|t| Some((t.width?, t.height?))),
    };
    let stream_inf = |s: &mut String, audio: Option<(usize, &str, u64)>| {
        let extra = audio.map(|a| a.2).unwrap_or(0);
        let _ = write!(
            s,
            "#EXT-X-STREAM-INF:BANDWIDTH={},AVERAGE-BANDWIDTH={}",
            peak + extra,
            avg + extra
        );
        let codecs: Vec<&str> = vcodec
            .iter()
            .map(|c| c.as_str())
            .chain(audio.map(|a| a.1))
            .collect();
        if !codecs.is_empty() {
            let _ = write!(s, ",CODECS=\"{}\"", codecs.join(","));
        }
        if let Some((w, h)) = resolution {
            let _ = write!(s, ",RESOLUTION={w}x{h}");
        }
        if let Some(t) = v {
            if let Some(r) = t.frame_rate {
                let _ = write!(s, ",FRAME-RATE={:.3}", r.as_f64());
            }
        }
        let _ = write!(s, ",VIDEO-RANGE={}", video_range.unwrap_or("SDR"));
        if let Some((gi, ..)) = audio {
            let _ = write!(s, ",AUDIO=\"a{gi}\"");
        }
        if !sub_names.is_empty() {
            s.push_str(",SUBTITLES=\"subs\"");
        }
        s.push_str("\nv/index.m3u8\n");
    };
    if groups.is_empty() {
        stream_inf(&mut s, None);
    }
    for (gi, (codec, members)) in groups.iter().enumerate() {
        let max_rate = members.iter().map(|m| m.2).max().unwrap_or(0);
        stream_inf(&mut s, Some((gi, codec.as_str(), max_rate)));
    }
    // Trick play from the source keyframes (copied video only).
    if let (None, Some(codec), true) = (&vt, &vcodec, asset.index.keyframes.len() > 1) {
        // A keyframe is ~10 average frames; one per keyframe interval.
        let kfs = asset.index.keyframes.len() as f64;
        let interval = (asset.index.duration_ns as f64 / 1e9 / kfs).max(0.5);
        let fps = v.and_then(|t| t.frame_rate).map(|r| r.as_f64()).unwrap_or(24.0);
        let bw = (10.0 * avg as f64 / fps / interval).max(1.0) as u64;
        let _ = write!(s, "#EXT-X-I-FRAME-STREAM-INF:BANDWIDTH={bw},CODECS=\"{codec}\"");
        if let Some((w, h)) = resolution {
            let _ = write!(s, ",RESOLUTION={w}x{h}");
        }
        let _ = writeln!(
            s,
            ",VIDEO-RANGE={},URI=\"{}\"",
            video_range.unwrap_or("SDR"),
            HlsResource::IFramePlaylist.path()
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_roundtrip() {
        for p in [
            "master.m3u8",
            "v/index.m3u8",
            "v/init.mp4",
            "v/12.m4s",
            "a/1/init.mp4",
            "a/1-aac2/3.m4s",
            "a/2-eac36/index.m3u8",
            "s/3.m3u8",
            "s/3.vtt",
            "v/iframes.m3u8",
            "v/kf/7.m4s",
        ] {
            let r = HlsResource::parse(p).unwrap_or_else(|| panic!("{p}"));
            assert_eq!(r.path(), p);
        }
        assert_eq!(HlsResource::parse("a/x/0.m4s"), None);
        assert_eq!(HlsResource::parse("v/-1.m4s"), None);
    }
}
