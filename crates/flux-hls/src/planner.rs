//! Explainable playback planner (research/03 §12.2; decision order in
//! `flux_core::plan`).
//!
//! 1. Direct file, if the container and every selected track are supported.
//! 2. HLS CMAF with video and audio copied (in-process, stateless remux).
//! 3. … plus bitstream fixes.
//! 4. … plus audio transcode (per segment).
//! 5. Video transcode (hardware first, keyframe-aligned segments).
//!
//! Subtitle choice is decided first: an image subtitle the client cannot
//! render forces a burn-in, which forces a video transcode.

use flux_core::{
    AudioCap, AudioOp, AudioPlan, ClientCaps, Codec, Container, DeliveryFormat, HdrFormat, PlaybackPlan,
    ProbeResult, Reason, SubtitleOp, SubtitlePlan, Track, VideoCap, VideoOp,
};

/// User/session preferences that shape a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanPrefs {
    /// Preferred audio track (source index); default track otherwise.
    pub audio_track: Option<u32>,
    /// Subtitle track to show (source index); `None` = off.
    pub subtitle_track: Option<u32>,
    /// Offer every audio track as an HLS rendition (switchable in the player).
    pub all_audio_tracks: bool,
    /// Offer every text subtitle track as a WebVTT rendition.
    pub all_text_subtitles: bool,
    /// Allow progressive direct play.
    pub allow_direct_play: bool,
    /// For multichannel sources a client can't decode, offer E-AC-3 5.1 when
    /// the client supports it (plus stereo AAC).
    pub prefer_eac3_surround: bool,
}

impl Default for PlanPrefs {
    fn default() -> Self {
        PlanPrefs {
            audio_track: None,
            subtitle_track: None,
            all_audio_tracks: true,
            all_text_subtitles: true,
            allow_direct_play: true,
            prefer_eac3_surround: true,
        }
    }
}

/// Audio codecs that can be packaged into fMP4 as-is.
///
/// Not MP3: `mp4a.40.34`/`mp4a.6B` in fMP4 is rejected by Chrome's and
/// Firefox's MediaSource (`isTypeSupported` false in both, measured by the
/// browser check), although both play MP3 files; MP3 becomes AAC.
fn fmp4_audio(c: Codec) -> bool {
    matches!(
        c,
        Codec::Aac | Codec::Ac3 | Codec::Eac3 | Codec::Opus | Codec::Flac
    )
}

/// Video codecs that can be packaged into fMP4 as-is.
fn fmp4_video(c: Codec) -> bool {
    matches!(c, Codec::H264 | Codec::Hevc | Codec::Av1)
}

fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// True when a decoder for profile `allowed` can decode a stream whose
/// profile is `profile`.
///
/// flux-probe (whose `ProbeResult` the catalog stores and `POST /play` plans
/// from) uses ffprobe's names, which spell out constraint-flag subsets that
/// this crate's own index and the presets don't. Without this, a Constrained
/// Baseline file planned from the catalog probe was video-transcoded on every
/// client (found by the workspace integration check). H.264 Annex A:
/// - Constrained Baseline (constraint_set1_flag) is the common subset of
///   Baseline and Main, and High decoders decode Main (A.2.1.1, A.2.4).
/// - The "… Intra" profiles obey every constraint of their base profile
///   (A.2.8–A.2.11), so "High 10" covers "High 10 Intra" (not the reverse).
/// - "High 4:4:4 Predictive" is ffprobe's name for profile_idc 244, which the
///   index calls "High 4:4:4".
fn profile_satisfies(allowed: &str, profile: &str) -> bool {
    let canon = |s: &str| {
        let n = norm(s);
        if n == "high444predictive" {
            "high444".to_string()
        } else {
            n
        }
    };
    let (a, p) = (canon(allowed), canon(profile));
    if a == p {
        return true;
    }
    if p == "constrainedbaseline" {
        return matches!(a.as_str(), "baseline" | "main" | "high");
    }
    p.strip_suffix("intra").is_some_and(|base| canon(base) == a)
}

/// Checks a video track against a client's video capabilities; returns the
/// reasons it cannot be played as-is (empty = playable).
pub fn video_problems(t: &Track, caps: &ClientCaps) -> Vec<Reason> {
    let Some(cap) = caps.video.iter().find(|c| c.codec == t.codec) else {
        return vec![Reason::VideoCodecUnsupported { codec: t.codec }];
    };
    let mut r = Vec::new();
    if let Some(p) = &t.profile {
        if !cap.profiles.is_empty() && !cap.profiles.iter().any(|x| profile_satisfies(x, p)) {
            r.push(Reason::VideoProfileUnsupported {
                codec: t.codec,
                profile: p.clone(),
            });
        }
    }
    if let (Some(level), Some(max)) = (t.level, cap.max_level) {
        if level > max {
            r.push(Reason::VideoLevelTooHigh {
                codec: t.codec,
                level,
                max,
            });
        }
    }
    if let (Some(bd), Some(max)) = (t.bit_depth, cap.max_bit_depth) {
        if bd > max {
            r.push(Reason::VideoBitDepthUnsupported { bit_depth: bd });
        }
    }
    if let (Some(w), Some(h)) = (t.width, t.height) {
        if cap.max_width.is_some_and(|mw| w > mw) || cap.max_height.is_some_and(|mh| h > mh) {
            r.push(Reason::VideoResolutionTooHigh { width: w, height: h });
        }
    }
    if let Some(hdr) = &t.hdr {
        match hdr.format {
            HdrFormat::Sdr => {}
            HdrFormat::DolbyVision => {
                let dv = hdr.dovi;
                let profile = dv.map(|d| d.profile).unwrap_or(0);
                if !cap.dovi_profiles.contains(&profile) {
                    // A cross-compatible base layer (HDR10 / SDR / HLG) still plays.
                    let base = match dv.map(|d| d.bl_signal_compat_id) {
                        Some(1) | Some(6) => Some(HdrFormat::Hdr10),
                        Some(2) => Some(HdrFormat::Sdr),
                        Some(4) => Some(HdrFormat::Hlg),
                        _ => None,
                    };
                    match base {
                        Some(HdrFormat::Sdr) => {}
                        Some(b) if cap.hdr.contains(&b) => {}
                        _ => r.push(Reason::DoviProfileUnsupported { profile }),
                    }
                }
            }
            HdrFormat::Hdr10Plus if cap.hdr.contains(&HdrFormat::Hdr10) => {}
            f => {
                if !cap.hdr.contains(&f) {
                    r.push(Reason::HdrUnsupported { format: f });
                }
            }
        }
    }
    r
}

/// Why an audio track can't be played as-is (`None` = playable).
pub fn audio_problem(t: &Track, caps: &ClientCaps) -> Option<Reason> {
    let Some(cap) = caps.audio.iter().find(|c| c.codec == t.codec) else {
        return Some(Reason::AudioCodecUnsupported { codec: t.codec });
    };
    match t.channels {
        Some(ch) if ch > cap.max_channels => Some(Reason::AudioChannelsTooMany {
            channels: ch,
            max: cap.max_channels,
        }),
        _ => None,
    }
}

fn audio_cap(caps: &ClientCaps, c: Codec) -> Option<&AudioCap> {
    caps.audio.iter().find(|a| a.codec == c)
}

/// Builds the playback plan for `probe` on a client with `caps`.
pub fn plan(probe: &ProbeResult, caps: &ClientCaps, prefs: &PlanPrefs) -> PlaybackPlan {
    let mut reasons = Vec::new();
    let video = probe.main_video();
    let audio_tracks: Vec<&Track> = probe.audio().collect();
    let selected_audio: Option<&Track> = prefs
        .audio_track
        .and_then(|i| audio_tracks.iter().copied().find(|t| t.index == i))
        .or_else(|| audio_tracks.iter().copied().find(|t| t.default))
        .or_else(|| audio_tracks.first().copied());

    // ---- subtitles first: an image subtitle the client can't render forces burn-in.
    let mut subtitles = Vec::new();
    let mut burn_in = None;
    if let Some(st) = prefs
        .subtitle_track
        .and_then(|i| probe.subtitles().find(|t| t.index == i))
    {
        let op = if caps.subtitles.contains(&st.codec) && !st.codec.is_text_subtitle() {
            SubtitleOp::ClientRender
        } else if st.codec.is_text_subtitle() {
            if st.codec != Codec::Subrip && st.codec != Codec::WebVtt && caps.subtitles.contains(&st.codec) {
                SubtitleOp::ClientRender
            } else {
                SubtitleOp::WebVtt
            }
        } else {
            reasons.push(Reason::SubtitleNeedsBurnIn { codec: st.codec });
            burn_in = Some(st.index);
            SubtitleOp::BurnIn
        };
        subtitles.push(SubtitlePlan { track: st.index, op });
    }
    if prefs.all_text_subtitles {
        for st in probe.subtitles().filter(|t| t.codec.is_text_subtitle()) {
            if !subtitles.iter().any(|s| s.track == st.index) {
                subtitles.push(SubtitlePlan {
                    track: st.index,
                    op: SubtitleOp::WebVtt,
                });
            }
        }
    }

    let mut video_issues = video.map(|v| video_problems(v, caps)).unwrap_or_default();
    // H.264 only: the codecs browsers decode whose transcode deinterlaces
    // (MPEG-2 and VC-1 are transcoded for browsers anyway; an HEVC, AV1 or
    // VP9 transcode would come out as interlaced as it went in).
    let interlaced_h264 = video.is_some_and(|v| v.codec == Codec::H264 && v.interlaced == Some(true));
    if interlaced_h264 && quirks(&caps.client).no_deinterlacer {
        // Transcoded, so the transcoder's deinterlacer turns it progressive.
        video_issues.push(Reason::ClientQuirk {
            note: "this browser shows interlaced video combed, so it is deinterlaced".into(),
        });
    }
    let bitrate_issue = match (probe.bitrate, caps.max_bitrate) {
        (Some(b), Some(m)) if b > m => Some(Reason::BitrateTooHigh { bitrate: b, max: m }),
        _ => None,
    };

    // ---- 1. direct play
    let container_ok = caps.progressive_containers.contains(&probe.container);
    let selected_is_default = selected_audio
        .map(|a| Some(a.index) == audio_tracks.first().map(|t| t.index) || a.default)
        .unwrap_or(true);
    let direct_ok = prefs.allow_direct_play
        && caps.delivery.contains(&DeliveryFormat::ProgressiveFile)
        && container_ok
        && video_issues.is_empty()
        && selected_audio.is_none_or(|a| audio_problem(a, caps).is_none())
        && selected_is_default
        && burn_in.is_none()
        && bitrate_issue.is_none();
    if direct_ok {
        let audio = selected_audio
            .map(|a| AudioPlan {
                track: a.index,
                op: AudioOp::Copy,
            })
            .into_iter()
            .collect();
        let mbps = probe.bitrate.unwrap_or(0) as f64 / 1e6;
        return PlaybackPlan {
            delivery: DeliveryFormat::ProgressiveFile,
            video_track: video.map(|v| v.index),
            video: VideoOp::Copy { fixes: vec![] },
            audio,
            subtitles,
            demuxed_audio: false,
            reasons,
            // Byte serving only: ~0.1 ms of CPU per MB.
            est_cpu_ms_per_min: (mbps * 7.5 * 0.1) as u32,
        };
    }
    // "Repackages the streams (no quality loss)" is only true when the video
    // is copied; it is added below once that is known, at this position.
    let container_reason_at = (!container_ok).then_some(reasons.len());

    // ---- 2–5. HLS
    let remuxable_container = matches!(
        probe.container,
        Container::Matroska | Container::WebM | Container::Mp4 | Container::Mov
    );
    // Matroska/MP4 are always indexable by flux-hls, whatever the probe's
    // `has_index` says ("Cues/stss are cheap to load"): Cue-less Matroska is
    // indexed by a cluster scan, fragmented MP4 from its `moof` boxes.
    // MPEG-TS/M2TS with H.264/HEVC video is indexed by a packet scan.
    let can_index = remuxable_container || crate::ts::probe_remuxable(probe);
    if !can_index {
        reasons.push(Reason::NoIndex);
    }
    let mut cpu_ms_per_min = 0f64;
    let video_op = match video {
        Some(v)
            if can_index
                && video_issues.is_empty()
                && burn_in.is_none()
                && bitrate_issue.is_none()
                && fmp4_video(v.codec) =>
        {
            // Remux: measured ≈1 ms CPU per MB served (research/03 §4.6).
            let mbps = v.bitrate.or(probe.bitrate).unwrap_or(8_000_000) as f64 / 1e6;
            cpu_ms_per_min += mbps * 7.5;
            if let Some(dv) = v.hdr.as_ref().and_then(|h| h.dovi).filter(|d| d.profile == 7) {
                reasons.push(Reason::ClientQuirk {
                    note: format!(
                        "Dolby Vision profile {} plays as its HDR10 base layer",
                        dv.profile
                    ),
                });
            }
            VideoOp::Copy { fixes: vec![] }
        }
        Some(v) => {
            reasons.extend(video_issues.iter().cloned());
            if let Some(b) = bitrate_issue.clone() {
                reasons.push(b);
            }
            if !fmp4_video(v.codec) && video_issues.is_empty() {
                // The client decodes it; flux can't put it in CMAF segments.
                reasons.push(Reason::CodecNotPackageable { codec: v.codec });
            }
            cpu_ms_per_min += VIDEO_TRANSCODE_CPU_MS_PER_MIN;
            transcode_op(v, caps, burn_in)
        }
        None => VideoOp::Copy { fixes: vec![] },
    };

    if let (Some(at), VideoOp::Copy { .. }) = (container_reason_at, &video_op) {
        reasons.insert(
            at,
            Reason::ContainerUnsupported {
                container: probe.container,
            },
        );
    }

    // Audio renditions: selected track first.
    let mut order: Vec<&Track> = Vec::new();
    if let Some(a) = selected_audio {
        order.push(a);
    }
    if prefs.all_audio_tracks {
        for a in &audio_tracks {
            if !order.iter().any(|o| o.index == a.index) {
                order.push(a);
            }
        }
    }
    let aac_ok = audio_cap(caps, Codec::Aac).is_some();
    let eac3_ok = audio_cap(caps, Codec::Eac3).is_some_and(|c| c.max_channels >= 6);
    let mut audio = Vec::new();
    for (i, a) in order.iter().enumerate() {
        let problem = audio_problem(a, caps);
        let packable = fmp4_audio(a.codec);
        if problem.is_none() && packable {
            audio.push(AudioPlan {
                track: a.index,
                op: AudioOp::Copy,
            });
        } else {
            if i == 0 {
                match &problem {
                    Some(p) => reasons.push(p.clone()),
                    // The client decodes it (TrueHD/DTS pass-through, PCM…),
                    // but fMP4 HLS can't carry it.
                    None => reasons.push(Reason::CodecNotPackageable { codec: a.codec }),
                }
            }
            let ch = a.channels.unwrap_or(2);
            if ch > 2 && eac3_ok && prefs.prefer_eac3_surround {
                audio.push(AudioPlan {
                    track: a.index,
                    op: AudioOp::Transcode {
                        codec: Codec::Eac3,
                        channels: ch.min(6),
                        bitrate: 640_000,
                    },
                });
                cpu_ms_per_min += 400.0;
            }
            if aac_ok {
                audio.push(AudioPlan {
                    track: a.index,
                    op: AudioOp::Transcode {
                        codec: Codec::Aac,
                        channels: 2,
                        bitrate: 192_000,
                    },
                });
                cpu_ms_per_min += 300.0;
            }
        }
    }
    // Apple HLS rule 2.3: offer stereo AAC for the main track, so a route
    // that can't take the copy (AirPlay, Bluetooth, a TV without Dolby)
    // still has sound. A stereo or mono FLAC/Opus/MP3/ALAC copy needs no
    // fallback: every client that decodes it can also output it. Adding one
    // anyway costs a transcode and, in hls.js, puts the AAC copy in its own
    // cheaper codec tier, which hls.js starts on instead of the copy (it
    // starts on the lowest-BANDWIDTH tier), and its switch from a FLAC/Opus
    // SourceBuffer back to AAC fails (found by the browser check).
    let plain_stereo_copy = selected_audio.is_some_and(|a| {
        matches!(a.codec, Codec::Flac | Codec::Opus | Codec::Mp3 | Codec::Alac)
            && a.channels.unwrap_or(2) <= 2
            && audio.iter().any(|p| p.track == a.index && p.op == AudioOp::Copy)
    });
    if let (Some(a), true, false) = (selected_audio, aac_ok, plain_stereo_copy) {
        let has_aac = audio.iter().any(|p| {
            p.track == a.index
                && match &p.op {
                    AudioOp::Copy => a.codec == Codec::Aac,
                    AudioOp::Transcode { codec, .. } => *codec == Codec::Aac,
                }
        });
        if !has_aac {
            audio.push(AudioPlan {
                track: a.index,
                op: AudioOp::Transcode {
                    codec: Codec::Aac,
                    channels: 2,
                    bitrate: 192_000,
                },
            });
        }
    }

    PlaybackPlan {
        delivery: DeliveryFormat::HlsFmp4,
        video_track: video.map(|v| v.index),
        video: video_op,
        audio,
        subtitles,
        demuxed_audio: true,
        reasons,
        est_cpu_ms_per_min: cpu_ms_per_min.round() as u32,
    }
}

/// Plans from a probe alone (no playback index), for a dry run such as
/// `POST /api/v1/plan` ("expected on this device"): [`plan`] plus the
/// engine's own limits as a pure function of container and codec, so it
/// equals `HlsEngine::plan` whenever the index agrees with the probe:
/// - an audio copy the engine can't package (AVI, MPEG-TS/M2TS without
///   H.264/HEVC video, or TS audio other than AAC/AC-3/E-AC-3) becomes
///   stereo AAC;
/// - a transcoded rendition of a codec the transcoder can't read is dropped;
/// - duplicate renditions collapse.
///
/// What only the index knows is not modelled: a missing codec configuration
/// (e.g. AC-3 with no sync frame), open-GOP video on clients that seek
/// only to IDR pictures ([`Quirks`]), and MPEG-TS files the in-process
/// remux rejects after reading them (field-coded H.264, parameter sets that
/// change mid-stream, scrambled video).
pub fn plan_from_probe(probe: &ProbeResult, caps: &ClientCaps, prefs: &PlanPrefs) -> PlaybackPlan {
    let remuxable = matches!(
        probe.container,
        Container::Matroska | Container::WebM | Container::Mp4 | Container::Mov
    );
    let codec_of = |track: u32| probe.tracks.iter().find(|t| t.index == track).map(|t| t.codec);
    // MPEG-TS remux copies AAC (ADTS), AC-3 and E-AC-3.
    let ts_remux = crate::ts::probe_remuxable(probe);
    let ts_copy = |track: u32| {
        ts_remux
            && probe
                .tracks
                .iter()
                .find(|t| t.index == track)
                .is_some_and(crate::ts::copyable)
    };
    finalize_with(
        plan(probe, caps, prefs),
        |track| (remuxable || ts_copy(track)) && codec_of(track).is_some_and(fmp4_audio),
        |track| !remuxable || codec_of(track).is_some_and(transcoder_reads),
    )
}

/// Audio codecs the transcoder can read from an indexed (Matroska/MP4)
/// source: elementary streams, PCM, or Opus/Vorbis wrapped in Matroska.
fn transcoder_reads(c: Codec) -> bool {
    matches!(
        c,
        Codec::TrueHd
            | Codec::Dts
            | Codec::Ac3
            | Codec::Eac3
            | Codec::Flac
            | Codec::Mp3
            | Codec::Mp2
            | Codec::Aac
            | Codec::Pcm
            | Codec::Opus
            | Codec::Vorbis
    )
}

/// Adapts a plan to what an engine can produce: audio copies it can't
/// package (`copy_ok(track)` false) become stereo AAC, duplicate renditions
/// collapse, and renditions it can't produce are dropped. Shared by
/// [`plan_from_probe`] and `HlsEngine::finalize_plan`.
pub(crate) fn finalize_with(
    mut plan: PlaybackPlan,
    copy_ok: impl Fn(u32) -> bool,
    transcode_ok: impl Fn(u32) -> bool,
) -> PlaybackPlan {
    let hls = plan.delivery != DeliveryFormat::ProgressiveFile;
    for a in plan.audio.iter_mut() {
        if hls && a.op == AudioOp::Copy && !copy_ok(a.track) {
            a.op = AudioOp::Transcode {
                codec: Codec::Aac,
                channels: 2,
                bitrate: 192_000,
            };
        }
    }
    let mut seen = Vec::new();
    plan.audio.retain(|a| {
        let k = crate::playlist::AudioKey::from_plan(a);
        let fresh = !seen.contains(&k);
        seen.push(k);
        fresh
    });
    plan.audio.retain(|a| match a.op {
        AudioOp::Copy => !hls || copy_ok(a.track),
        AudioOp::Transcode { .. } => transcode_ok(a.track),
    });
    plan
}

/// Rough CPU cost of a hardware video transcode (ms of CPU per minute).
const VIDEO_TRANSCODE_CPU_MS_PER_MIN: f64 = 12_000.0;

/// The video transcode for `v` on a client (H.264, hardware first).
fn transcode_op(v: &Track, caps: &ClientCaps, burn_in: Option<u32>) -> VideoOp {
    let target = transcode_target(v, caps);
    let hdr_out = caps.video.iter().find(|c| c.codec == target.0);
    let tonemap = v
        .hdr
        .as_ref()
        .is_some_and(|h| h.format != HdrFormat::Sdr && hdr_out.is_none_or(|c| !c.hdr.contains(&h.format)));
    VideoOp::Transcode {
        codec: target.0,
        width: target.1,
        height: target.2,
        bitrate: target.3,
        tonemap,
        burn_subtitle: burn_in,
        hardware: true,
    }
}

/// Behaviour of a client family that its capability lists can't express,
/// looked up by [`ClientCaps::client`] (the quirks table the field exists for).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Quirks {
    /// HEVC seeks only land on IDR pictures: Firefox on macOS treats a CRA
    /// as a non-keyframe (`MP4Demuxer`: "VideoToolbox can return a bad data
    /// error if a CRA frame is the first sample after a seek"), so an
    /// open-GOP x265 stream, whose only IDR is its first frame, can only be
    /// seeked by decoding from the start. Found by the browser check: seeks
    /// outside the buffer never complete, seeks inside take time
    /// proportional to the position. Open-GOP HEVC is transcoded for these
    /// clients. (Open-GOP H.264, non-IDR I pictures, seeks fine there.)
    pub idr_random_access_only: bool,
    /// Interlaced video plays combed: browsers show the two fields of a
    /// frame woven together (no deinterlacer for MSE or progressive video;
    /// a 576i DVD clip looked so in Chrome). Interlaced H.264 is
    /// transcoded, and the transcode deinterlaces it.
    pub no_deinterlacer: bool,
}

/// The quirks of a client id such as `"web/firefox-157/macos"`.
pub fn quirks(client: &str) -> Quirks {
    let c = client.to_ascii_lowercase();
    Quirks {
        idr_random_access_only: c.starts_with("web/firefox") && c.ends_with("/macos"),
        no_deinterlacer: c.starts_with("web/"),
    }
}

/// Plans `probe` for a client whose quirks need facts only the playback
/// index has: with `open_gop` video on a client that seeks only to IDR
/// pictures, the video is transcoded (direct play is not offered either:
/// the browser's file demuxer has the same keyframe rule).
pub fn plan_with_index_facts(
    probe: &ProbeResult,
    caps: &ClientCaps,
    prefs: &PlanPrefs,
    open_gop: bool,
) -> PlaybackPlan {
    let affected = open_gop
        && quirks(&caps.client).idr_random_access_only
        && probe.main_video().is_some_and(|v| v.codec == Codec::Hevc);
    if !affected {
        return plan(probe, caps, prefs);
    }
    let mut p = plan(
        probe,
        caps,
        &PlanPrefs {
            allow_direct_play: false,
            ..prefs.clone()
        },
    );
    if let (VideoOp::Copy { .. }, Some(v)) = (&p.video, probe.main_video()) {
        apply_open_gop_quirk(&mut p, v, caps);
    }
    p
}

fn apply_open_gop_quirk(plan: &mut PlaybackPlan, v: &Track, caps: &ClientCaps) {
    // The video is transcoded after all: "repackages the streams (no
    // quality loss)" no longer holds (it read next to "transcodes it").
    plan.reasons
        .retain(|r| !matches!(r, Reason::ContainerUnsupported { .. }));
    plan.reasons.push(Reason::ClientQuirk {
        note: "this browser can only seek this video's open GOPs by decoding from the start".into(),
    });
    plan.video = transcode_op(v, caps, None);
    plan.est_cpu_ms_per_min = plan
        .est_cpu_ms_per_min
        .saturating_add(VIDEO_TRANSCODE_CPU_MS_PER_MIN as u32);
}

/// (codec, width, height, bitrate) for a video transcode.
fn transcode_target(v: &Track, caps: &ClientCaps) -> (Codec, u32, u32, u64) {
    let h264 = caps.video.iter().find(|c| c.codec == Codec::H264);
    let (mw, mh) = h264
        .map(|c| (c.max_width.unwrap_or(1920), c.max_height.unwrap_or(1080)))
        .unwrap_or((1920, 1080));
    // H.264 is the universal target; cap at 1080p (hardware H.264 at 4K is
    // heavy and rarely needed for a fallback rung).
    let (w, h) = (v.width.unwrap_or(1920), v.height.unwrap_or(1080));
    let scale = (mw.min(1920) as f64 / w as f64)
        .min(mh.min(1080) as f64 / h as f64)
        .min(1.0);
    let w2 = ((w as f64 * scale) as u32) & !1;
    let h2 = ((h as f64 * scale) as u32) & !1;
    let bitrate = if h2 > 720 {
        8_000_000
    } else if h2 > 480 {
        4_000_000
    } else {
        2_000_000
    };
    let bitrate = caps
        .max_bitrate
        .map(|m| bitrate.min(m * 8 / 10))
        .unwrap_or(bitrate);
    (Codec::H264, w2, h2, bitrate)
}

/// Researched client capability presets (research/03 §2, as of 2026-10).
/// Real clients should send runtime-probed caps; these are for tests, dev
/// tools and as server-side defaults per client family.
///
/// `VideoCap::hdr` lists what each family presents correctly (macOS browsers
/// tone-map HDR to SDR displays themselves); `display_hdr` is the typical
/// display: SDR for desktop browsers, HDR for the 4K TV boxes.
pub mod presets {
    use super::*;

    fn v(
        codec: Codec,
        profiles: &[&str],
        max_bit_depth: u8,
        max_w: u32,
        max_h: u32,
        hdr: &[HdrFormat],
        dovi: &[u8],
    ) -> VideoCap {
        VideoCap {
            codec,
            profiles: profiles.iter().map(|s| s.to_string()).collect(),
            max_level: None,
            max_bit_depth: Some(max_bit_depth),
            max_width: Some(max_w),
            max_height: Some(max_h),
            hdr: hdr.to_vec(),
            dovi_profiles: dovi.to_vec(),
        }
    }

    fn a(codec: Codec, max_channels: u8) -> AudioCap {
        AudioCap { codec, max_channels }
    }

    use HdrFormat::*;

    /// Chrome/Edge desktop via hls.js (MSE). HEVC needs a hardware decoder
    /// (default on macOS/Windows since Chrome 107); no AC-3/E-AC-3/DTS/TrueHD;
    /// MKV progressive not assumed (Jellyfin docs say Chrome 145+, unverified).
    pub fn chrome_desktop() -> ClientCaps {
        ClientCaps {
            client: "web/chrome/desktop".into(),
            delivery: vec![DeliveryFormat::ProgressiveFile, DeliveryFormat::HlsFmp4],
            progressive_containers: vec![Container::Mp4, Container::WebM],
            video: vec![
                v(
                    Codec::H264,
                    &["Baseline", "Main", "High"],
                    8,
                    4096,
                    2304,
                    &[],
                    &[],
                ),
                v(
                    Codec::Hevc,
                    &["Main", "Main 10"],
                    10,
                    7680,
                    4320,
                    &[Hdr10, Hlg],
                    &[],
                ),
                v(Codec::Av1, &[], 10, 7680, 4320, &[Hdr10, Hlg], &[]),
                v(Codec::Vp9, &[], 10, 7680, 4320, &[Hdr10, Hlg], &[]),
            ],
            audio: vec![
                a(Codec::Aac, 8),
                a(Codec::Opus, 8),
                a(Codec::Flac, 8),
                a(Codec::Mp3, 2),
                a(Codec::Vorbis, 8),
            ],
            subtitles: vec![Codec::WebVtt],
            display_hdr: false,
            needs_demuxed_audio: false,
            max_bitrate: None,
        }
    }

    /// Safari on macOS (native HLS). AV1 only on M3+/A17 Pro+, so not assumed;
    /// Opus in MP4 stereo only (macOS 14+).
    pub fn safari_macos() -> ClientCaps {
        ClientCaps {
            client: "web/safari/macos".into(),
            delivery: vec![
                DeliveryFormat::ProgressiveFile,
                DeliveryFormat::HlsFmp4,
                DeliveryFormat::HlsTs,
            ],
            progressive_containers: vec![Container::Mp4, Container::Mov],
            video: vec![
                v(
                    Codec::H264,
                    &["Baseline", "Main", "High"],
                    8,
                    4096,
                    2304,
                    &[],
                    &[],
                ),
                v(
                    Codec::Hevc,
                    &["Main", "Main 10"],
                    10,
                    7680,
                    4320,
                    &[Hdr10, Hlg, DolbyVision],
                    &[5, 8],
                ),
            ],
            audio: vec![
                a(Codec::Aac, 8),
                a(Codec::Ac3, 6),
                a(Codec::Eac3, 8),
                a(Codec::Flac, 8),
                a(Codec::Alac, 8),
                a(Codec::Opus, 2),
                a(Codec::Mp3, 2),
            ],
            subtitles: vec![Codec::WebVtt],
            display_hdr: false,
            needs_demuxed_audio: false,
            max_bitrate: None,
        }
    }

    /// Firefox (hls.js/MSE). HEVC via hardware on macOS 136+/Windows 134+;
    /// Matroska progressive since Firefox 145; no AC-3/E-AC-3.
    pub fn firefox() -> ClientCaps {
        ClientCaps {
            client: "web/firefox/desktop".into(),
            delivery: vec![DeliveryFormat::ProgressiveFile, DeliveryFormat::HlsFmp4],
            progressive_containers: vec![Container::Mp4, Container::WebM, Container::Matroska],
            video: vec![
                v(
                    Codec::H264,
                    &["Baseline", "Main", "High"],
                    8,
                    4096,
                    2304,
                    &[],
                    &[],
                ),
                v(Codec::Hevc, &["Main", "Main 10"], 10, 7680, 4320, &[Hdr10], &[]),
                v(Codec::Av1, &[], 10, 7680, 4320, &[Hdr10], &[]),
                v(Codec::Vp9, &[], 10, 7680, 4320, &[], &[]),
            ],
            audio: vec![
                a(Codec::Aac, 8),
                a(Codec::Opus, 8),
                a(Codec::Flac, 8),
                a(Codec::Mp3, 2),
                a(Codec::Vorbis, 8),
            ],
            subtitles: vec![Codec::WebVtt],
            display_hdr: false,
            needs_demuxed_audio: false,
            max_bitrate: None,
        }
    }

    /// Apple TV 4K (4th gen, A19, 2026): AV1, DV 8.1 and 5, HDR10/HDR10+/HLG;
    /// AC-3/E-AC-3 (Atmos via E-AC-3 JOC); no TrueHD/DTS; no MKV (AVPlayer).
    pub fn apple_tv_4k() -> ClientCaps {
        ClientCaps {
            client: "tvos/appletv4k-gen4".into(),
            delivery: vec![
                DeliveryFormat::ProgressiveFile,
                DeliveryFormat::HlsFmp4,
                DeliveryFormat::HlsTs,
            ],
            progressive_containers: vec![Container::Mp4, Container::Mov],
            video: vec![
                v(
                    Codec::H264,
                    &["Baseline", "Main", "High"],
                    8,
                    3840,
                    2160,
                    &[],
                    &[],
                ),
                v(
                    Codec::Hevc,
                    &["Main", "Main 10"],
                    10,
                    3840,
                    2160,
                    &[Hdr10, Hdr10Plus, Hlg, DolbyVision],
                    &[5, 8],
                ),
                v(
                    Codec::Av1,
                    &[],
                    10,
                    3840,
                    2160,
                    &[Hdr10, Hdr10Plus, Hlg, DolbyVision],
                    &[10],
                ),
            ],
            audio: vec![
                a(Codec::Aac, 8),
                a(Codec::Ac3, 6),
                a(Codec::Eac3, 8),
                a(Codec::Flac, 8),
                a(Codec::Alac, 8),
                a(Codec::Opus, 2),
                a(Codec::Mp3, 2),
            ],
            subtitles: vec![Codec::WebVtt],
            display_hdr: true,
            needs_demuxed_audio: false,
            max_bitrate: None,
        }
    }

    /// Android / Google TV (Media3), e.g. Google TV Streamer: MKV progressive,
    /// HEVC Main10, AV1 (hardware on current devices), DV/HDR10/HDR10+/HLG;
    /// Dolby audio (usually pass-through), no DTS/TrueHD assumed; renders
    /// SubRip/ASS (basic)/PGS itself.
    pub fn android_tv() -> ClientCaps {
        ClientCaps {
            client: "android-tv/media3".into(),
            delivery: vec![
                DeliveryFormat::ProgressiveFile,
                DeliveryFormat::HlsFmp4,
                DeliveryFormat::HlsTs,
            ],
            progressive_containers: vec![Container::Mp4, Container::Matroska, Container::WebM],
            video: vec![
                v(
                    Codec::H264,
                    &["Baseline", "Main", "High"],
                    8,
                    3840,
                    2160,
                    &[],
                    &[],
                ),
                v(
                    Codec::Hevc,
                    &["Main", "Main 10"],
                    10,
                    3840,
                    2160,
                    &[Hdr10, Hdr10Plus, Hlg, DolbyVision],
                    &[5, 8],
                ),
                v(Codec::Av1, &[], 10, 3840, 2160, &[Hdr10, Hlg], &[]),
                v(Codec::Vp9, &[], 10, 3840, 2160, &[Hdr10], &[]),
            ],
            audio: vec![
                a(Codec::Aac, 8),
                a(Codec::Ac3, 6),
                a(Codec::Eac3, 8),
                a(Codec::Opus, 8),
                a(Codec::Flac, 8),
                a(Codec::Mp3, 2),
                a(Codec::Vorbis, 8),
            ],
            subtitles: vec![Codec::WebVtt, Codec::Subrip, Codec::Ass, Codec::Pgs],
            display_hdr: true,
            needs_demuxed_audio: false,
            max_bitrate: None,
        }
    }

    /// An older H.264-only 1080p SDR device (2015-era smart TV / streaming
    /// stick): the class of client that needs a video transcode.
    pub fn legacy_h264() -> ClientCaps {
        ClientCaps {
            client: "legacy/h264-1080p".into(),
            delivery: vec![
                DeliveryFormat::ProgressiveFile,
                DeliveryFormat::HlsFmp4,
                DeliveryFormat::HlsTs,
            ],
            progressive_containers: vec![Container::Mp4],
            video: vec![v(
                Codec::H264,
                &["Baseline", "Main", "High"],
                8,
                1920,
                1080,
                &[],
                &[],
            )],
            audio: vec![a(Codec::Aac, 2), a(Codec::Ac3, 6), a(Codec::Mp3, 2)],
            subtitles: vec![Codec::WebVtt],
            display_hdr: false,
            needs_demuxed_audio: false,
            max_bitrate: None,
        }
    }

    /// Firefox on macOS: [`firefox`] with the macOS client id, so the
    /// planner applies its quirks (IDR-only random access, see
    /// [`super::Quirks`]).
    pub fn firefox_macos() -> ClientCaps {
        ClientCaps {
            client: "web/firefox/macos".into(),
            ..firefox()
        }
    }

    /// Looks up a preset by name (`chrome`, `safari`, `firefox`,
    /// `firefox_macos`, `appletv`, `androidtv`, `legacy`).
    pub fn by_name(name: &str) -> Option<ClientCaps> {
        Some(match name {
            "chrome" | "chrome_desktop" => chrome_desktop(),
            "safari" | "safari_macos" => safari_macos(),
            "firefox" => firefox(),
            "firefox_macos" | "firefoxmac" => firefox_macos(),
            "appletv" | "apple_tv" | "apple_tv_4k" => apple_tv_4k(),
            "androidtv" | "android_tv" => android_tv(),
            "legacy" | "legacy_h264" => legacy_h264(),
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flux_core::{HdrInfo, TrackKind, PROBE_VERSION};

    fn track(index: u32, kind: TrackKind, codec: Codec) -> Track {
        Track {
            index,
            id: index as u64 + 1,
            kind,
            codec,
            codec_tag: String::new(),
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
            default: index <= 1,
            forced: false,
            hearing_impaired: false,
            bitrate: None,
            sample_aspect_ratio: None,
        }
    }

    fn probe(container: Container, tracks: Vec<Track>) -> ProbeResult {
        ProbeResult {
            container,
            duration_ms: Some(60_000),
            bitrate: Some(8_000_000),
            title: None,
            tracks,
            chapters: vec![],
            attachments: vec![],
            has_index: true,
            faststart: Some(true),
            needs_frame_sei: false,
            warnings: vec![],
            probe_version: PROBE_VERSION,
            bytes_read: 0,
            reads: 0,
        }
    }

    fn h264() -> Track {
        Track {
            profile: Some("High".into()),
            bit_depth: Some(8),
            width: Some(1920),
            height: Some(1080),
            ..track(0, TrackKind::Video, Codec::H264)
        }
    }

    #[test]
    fn ffprobe_profile_names_satisfy_base_profiles() {
        // flux-probe's (ffprobe's) names vs the base names presets list.
        assert!(profile_satisfies("Baseline", "Constrained Baseline"));
        assert!(profile_satisfies("Main", "Constrained Baseline"));
        assert!(profile_satisfies("High", "Constrained Baseline"));
        assert!(profile_satisfies("High 10", "High 10 Intra"));
        assert!(profile_satisfies("High 4:2:2", "High 4:2:2 Intra"));
        assert!(profile_satisfies("High 4:4:4", "High 4:4:4 Predictive"));
        assert!(profile_satisfies("High 4:4:4 Predictive", "High 4:4:4 Intra"));
        assert!(profile_satisfies("main10", "Main 10"));
        // Supersets are not subsets.
        assert!(!profile_satisfies("Baseline", "Main"));
        assert!(!profile_satisfies("Constrained Baseline", "Baseline"));
        assert!(!profile_satisfies("High 10 Intra", "High 10"));
        assert!(!profile_satisfies("Main", "Main 10"));

        // End to end: a Constrained Baseline MP4 direct-plays on Chrome.
        let p = probe(
            Container::Mp4,
            vec![
                Track {
                    profile: Some("Constrained Baseline".into()),
                    ..h264()
                },
                Track {
                    channels: Some(2),
                    ..track(1, TrackKind::Audio, Codec::Aac)
                },
            ],
        );
        let plan = plan(&p, &presets::chrome_desktop(), &PlanPrefs::default());
        assert!(plan.is_direct(), "{:?}", plan.reasons);
    }

    /// A 576i H.264 clip (SAR 16:15) is transcoded for browsers, which
    /// would show it combed, at its own size (the init's `pasp` keeps 4:3);
    /// a TV box that deinterlaces gets it as it is.
    #[test]
    fn interlaced_video_is_transcoded_for_browsers() {
        let sd = Track {
            width: Some(720),
            height: Some(576),
            interlaced: Some(true),
            sample_aspect_ratio: Some(flux_core::Rational { num: 16, den: 15 }),
            ..h264()
        };
        let p = probe(
            Container::Mp4,
            vec![
                sd.clone(),
                Track {
                    channels: Some(2),
                    ..track(1, TrackKind::Audio, Codec::Aac)
                },
            ],
        );
        for caps in [
            presets::chrome_desktop(),
            presets::safari_macos(),
            presets::firefox(),
        ] {
            let plan = plan(&p, &caps, &PlanPrefs::default());
            let VideoOp::Transcode { width, height, .. } = plan.video else {
                panic!("{}: {:?}", caps.client, plan.video);
            };
            assert_eq!((width, height), (720, 576), "{}", caps.client);
            assert!(
                plan.reasons
                    .iter()
                    .any(|r| matches!(r, Reason::ClientQuirk { note } if note.contains("deinterlaced"))),
                "{}: {:?}",
                caps.client,
                plan.reasons
            );
        }
        let tv = presets::apple_tv_4k();
        assert!(matches!(
            plan(&p, &tv, &PlanPrefs::default()).video,
            VideoOp::Copy { .. }
        ));
        // Progressive (or unknown) video still direct-plays.
        for interlaced in [Some(false), None] {
            let q = probe(
                Container::Mp4,
                vec![
                    Track {
                        interlaced,
                        ..sd.clone()
                    },
                    Track {
                        channels: Some(2),
                        ..track(1, TrackKind::Audio, Codec::Aac)
                    },
                ],
            );
            assert!(plan(&q, &presets::chrome_desktop(), &PlanPrefs::default()).is_direct());
        }
        // Only H.264: an HEVC transcode wouldn't deinterlace (HEVC codes
        // fields as separate pictures), so interlaced HEVC stays a copy.
        let hevc = probe(
            Container::Matroska,
            vec![
                Track {
                    codec: Codec::Hevc,
                    profile: Some("Main".into()),
                    ..sd.clone()
                },
                Track {
                    channels: Some(2),
                    ..track(1, TrackKind::Audio, Codec::Aac)
                },
            ],
        );
        let p = plan(&hevc, &presets::safari_macos(), &PlanPrefs::default());
        assert!(matches!(p.video, VideoOp::Copy { .. }), "{:?}", p.reasons);
    }

    #[test]
    fn mp4_aac_direct_plays_on_chrome() {
        let p = probe(
            Container::Mp4,
            vec![
                h264(),
                Track {
                    channels: Some(2),
                    ..track(1, TrackKind::Audio, Codec::Aac)
                },
            ],
        );
        let plan = plan(&p, &presets::chrome_desktop(), &PlanPrefs::default());
        assert!(plan.is_direct());
        assert_eq!(plan.label(), "Direct play");
    }

    #[test]
    fn mkv_truehd_on_chrome_remuxes_with_aac() {
        let p = probe(
            Container::Matroska,
            vec![
                h264(),
                Track {
                    channels: Some(8),
                    ..track(1, TrackKind::Audio, Codec::TrueHd)
                },
            ],
        );
        let plan = plan(&p, &presets::chrome_desktop(), &PlanPrefs::default());
        assert_eq!(plan.delivery, DeliveryFormat::HlsFmp4);
        assert!(matches!(plan.video, VideoOp::Copy { .. }));
        assert_eq!(plan.audio.len(), 1);
        assert!(matches!(
            plan.audio[0].op,
            AudioOp::Transcode {
                codec: Codec::Aac,
                channels: 2,
                ..
            }
        ));
        assert!(plan.reasons.contains(&Reason::ContainerUnsupported {
            container: Container::Matroska
        }));
        assert!(plan
            .reasons
            .contains(&Reason::AudioCodecUnsupported { codec: Codec::TrueHd }));
        assert_eq!(plan.label(), "Audio conversion");
    }

    #[test]
    fn mkv_truehd_on_apple_tv_gets_eac3_and_aac() {
        let p = probe(
            Container::Matroska,
            vec![
                h264(),
                Track {
                    channels: Some(8),
                    ..track(1, TrackKind::Audio, Codec::TrueHd)
                },
            ],
        );
        let plan = plan(&p, &presets::apple_tv_4k(), &PlanPrefs::default());
        let ops: Vec<_> = plan.audio.iter().map(|a| a.op.clone()).collect();
        assert_eq!(ops.len(), 2);
        assert!(matches!(
            ops[0],
            AudioOp::Transcode {
                codec: Codec::Eac3,
                channels: 6,
                ..
            }
        ));
        assert!(matches!(
            ops[1],
            AudioOp::Transcode {
                codec: Codec::Aac,
                channels: 2,
                ..
            }
        ));
    }

    #[test]
    fn eac3_copies_on_apple_tv_plus_stereo_aac() {
        let p = probe(
            Container::Matroska,
            vec![
                h264(),
                Track {
                    channels: Some(6),
                    ..track(1, TrackKind::Audio, Codec::Eac3)
                },
            ],
        );
        let plan = plan(&p, &presets::apple_tv_4k(), &PlanPrefs::default());
        assert_eq!(plan.audio[0].op, AudioOp::Copy);
        assert!(matches!(
            plan.audio[1].op,
            AudioOp::Transcode {
                codec: Codec::Aac,
                channels: 2,
                ..
            }
        ));
        assert_eq!(plan.label(), "Audio conversion");
    }

    #[test]
    fn hdr10_on_sdr_client_tonemaps() {
        let mut v = Track {
            profile: Some("Main 10".into()),
            bit_depth: Some(10),
            width: Some(3840),
            height: Some(2160),
            ..track(0, TrackKind::Video, Codec::Hevc)
        };
        v.hdr = Some(HdrInfo {
            format: HdrFormat::Hdr10,
            mastering: None,
            cll: None,
            dovi: None,
        });
        let p = probe(
            Container::Matroska,
            vec![
                v,
                Track {
                    channels: Some(2),
                    ..track(1, TrackKind::Audio, Codec::Aac)
                },
            ],
        );
        let mut caps = presets::chrome_desktop();
        caps.video.retain(|c| c.codec == Codec::H264);
        let plan = plan(&p, &caps, &PlanPrefs::default());
        match plan.video {
            VideoOp::Transcode {
                codec,
                tonemap,
                height,
                ..
            } => {
                assert_eq!(codec, Codec::H264);
                assert!(tonemap);
                assert_eq!(height, 1080);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(plan.label(), "Transcode");
    }

    #[test]
    fn pgs_on_apple_tv_burns_in() {
        let p = probe(
            Container::Matroska,
            vec![
                h264(),
                Track {
                    channels: Some(2),
                    ..track(1, TrackKind::Audio, Codec::Aac)
                },
                track(2, TrackKind::Subtitle, Codec::Pgs),
            ],
        );
        let prefs = PlanPrefs {
            subtitle_track: Some(2),
            ..Default::default()
        };
        let plan = plan(&p, &presets::apple_tv_4k(), &prefs);
        assert!(matches!(
            plan.video,
            VideoOp::Transcode {
                burn_subtitle: Some(2),
                ..
            }
        ));
        assert!(plan
            .reasons
            .contains(&Reason::SubtitleNeedsBurnIn { codec: Codec::Pgs }));
    }

    #[test]
    fn decodable_but_unpackageable_codecs_say_so() {
        // A TrueHD-capable client (AV receiver pass-through) still gets AAC
        // over HLS: fMP4 can't carry TrueHD.
        let mut caps = presets::android_tv();
        caps.audio.push(AudioCap {
            codec: Codec::TrueHd,
            max_channels: 8,
        });
        let p = probe(
            Container::Matroska,
            vec![
                h264(),
                Track {
                    channels: Some(8),
                    ..track(1, TrackKind::Audio, Codec::TrueHd)
                },
            ],
        );
        caps.progressive_containers.clear();
        let thd = plan(&p, &caps, &PlanPrefs::default());
        assert!(thd
            .reasons
            .contains(&Reason::CodecNotPackageable { codec: Codec::TrueHd }));
        assert!(thd.audio.iter().all(|a| a.op != AudioOp::Copy));

        // VP9 in MKV on Chrome: Chrome decodes VP9, but flux's CMAF writer
        // doesn't package it, so this is not "can't decode".
        let p = probe(
            Container::Matroska,
            vec![
                Track {
                    width: Some(1920),
                    height: Some(1080),
                    ..track(0, TrackKind::Video, Codec::Vp9)
                },
                track(1, TrackKind::Audio, Codec::Opus),
            ],
        );
        let vp9 = plan(&p, &presets::chrome_desktop(), &PlanPrefs::default());
        assert!(matches!(vp9.video, VideoOp::Transcode { .. }));
        assert!(vp9
            .reasons
            .contains(&Reason::CodecNotPackageable { codec: Codec::Vp9 }));
        assert!(!vp9
            .reasons
            .iter()
            .any(|r| matches!(r, Reason::VideoCodecUnsupported { .. })));
    }

    #[test]
    fn stereo_aac_fallback_only_for_dolby_or_multichannel_copies() {
        let with_audio = |codec, channels| {
            probe(
                Container::Matroska,
                vec![
                    h264(),
                    Track {
                        channels: Some(channels),
                        ..track(1, TrackKind::Audio, codec)
                    },
                ],
            )
        };
        let chrome = presets::chrome_desktop();
        // Mono/stereo FLAC and Opus copies stand alone (one codec tier in hls.js).
        for codec in [Codec::Flac, Codec::Opus] {
            let p = plan(&with_audio(codec, 2), &chrome, &PlanPrefs::default());
            assert_eq!(p.audio.len(), 1, "{codec:?}: {:?}", p.audio);
            assert_eq!(p.audio[0].op, AudioOp::Copy);
            assert_eq!(p.label(), "Direct stream");
        }
        // 5.1 FLAC keeps a stereo AAC fallback.
        let p = plan(&with_audio(Codec::Flac, 6), &chrome, &PlanPrefs::default());
        assert_eq!(p.audio.len(), 2);
        // Dolby on Apple TV keeps it too (rule 2.3).
        let p = plan(
            &with_audio(Codec::Eac3, 2),
            &presets::apple_tv_4k(),
            &PlanPrefs::default(),
        );
        assert_eq!(p.audio.len(), 2);
    }

    #[test]
    fn ts_without_index_transcodes() {
        let mut p = probe(
            Container::MpegTs,
            vec![
                Track {
                    profile: Some("Main".into()),
                    ..track(0, TrackKind::Video, Codec::Mpeg2Video)
                },
                Track {
                    channels: Some(6),
                    ..track(1, TrackKind::Audio, Codec::Ac3)
                },
            ],
        );
        p.has_index = false;
        let plan = plan(&p, &presets::chrome_desktop(), &PlanPrefs::default());
        assert!(matches!(plan.video, VideoOp::Transcode { .. }));
        assert!(plan.reasons.contains(&Reason::NoIndex));
        // A transcode is not "repackaged with no quality loss" (the stats
        // overlay showed both sentences for every TS and AVI file).
        assert!(
            !plan
                .reasons
                .iter()
                .any(|r| matches!(r, Reason::ContainerUnsupported { .. })),
            "{:?}",
            plan.reasons
        );
    }

    /// The AVI case from the merge report, and every other container ×
    /// preset × open-GOP combination: a plan that transcodes the video never
    /// also says the streams are repackaged with no quality loss.
    #[test]
    fn transcodes_never_claim_a_lossless_repackage() {
        let names = [
            "chrome",
            "safari",
            "firefox",
            "firefox_macos",
            "appletv",
            "androidtv",
            "legacy",
        ];
        let containers = [
            Container::Avi,
            Container::MpegTs,
            Container::M2ts,
            Container::Matroska,
            Container::WebM,
            Container::Mp4,
            Container::Mov,
        ];
        let videos = [
            h264(),
            Track {
                profile: Some("Main 10".into()),
                bit_depth: Some(10),
                ..track(0, TrackKind::Video, Codec::Hevc)
            },
            track(0, TrackKind::Video, Codec::Mpeg4Part2),
            track(0, TrackKind::Video, Codec::Vp9),
        ];
        for name in names {
            let caps = presets::by_name(name).unwrap();
            for c in containers {
                for v in &videos {
                    let p = probe(
                        c,
                        vec![
                            v.clone(),
                            Track {
                                channels: Some(2),
                                ..track(1, TrackKind::Audio, Codec::Aac)
                            },
                        ],
                    );
                    for open_gop in [false, true] {
                        let plan = plan_with_index_facts(&p, &caps, &PlanPrefs::default(), open_gop);
                        let claims = plan
                            .reasons
                            .iter()
                            .any(|r| matches!(r, Reason::ContainerUnsupported { .. }));
                        assert!(
                            !(claims && matches!(plan.video, VideoOp::Transcode { .. })),
                            "{name} {c:?} {:?} open_gop={open_gop}: {:?}",
                            v.codec,
                            plan.explain()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn unsupported_container_with_copied_video_says_repackaged_first() {
        let p = probe(
            Container::Matroska,
            vec![
                h264(),
                Track {
                    channels: Some(6),
                    ..track(1, TrackKind::Audio, Codec::Eac3)
                },
            ],
        );
        let mut caps = presets::chrome_desktop();
        caps.progressive_containers.retain(|c| *c != Container::Matroska);
        let plan = plan(&p, &caps, &PlanPrefs::default());
        assert!(matches!(plan.video, VideoOp::Copy { .. }));
        assert_eq!(
            plan.reasons.first(),
            Some(&Reason::ContainerUnsupported {
                container: Container::Matroska
            })
        );
    }
}
