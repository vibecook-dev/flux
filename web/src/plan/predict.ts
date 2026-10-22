/**
 * A client-side sketch of flux-hls's planner, following the decision order in
 * plan.rs / research/03 §12.2: direct file → HLS copy → copy + bitstream fixes
 * → copy + audio transcode → video transcode. Every step away from "direct"
 * records a `Reason`.
 *
 * Two users: the mock backend (standing in for the server), and the item page,
 * which shows the *expected* outcome before you press Play. The server's plan
 * is authoritative; the player's stats overlay shows that one.
 */
import { explainReason } from '../api/explain';
import type {
  AudioPlan,
  ClientCaps,
  Codec,
  DeliveryFormat,
  PlaybackPlan,
  ProbeResult,
  Reason,
  SubtitleFile,
  SubtitlePlan,
  Track,
  VideoOp,
} from '../api/types';

export interface PlanInput {
  probe: ProbeResult;
  caps: ClientCaps;
  /** Source track index of the chosen audio; `null` = default track. */
  audioTrack: number | null;
  /** Source track index (or a subtitle file's track) of the chosen subtitle; `null` = off. */
  subtitleTrack: number | null;
  /** Subtitle files beside the video (`AssetInfo.subtitle_files`). */
  subtitleFiles?: SubtitleFile[];
}

export interface PredictedPlan {
  plan: PlaybackPlan;
  explain: string[];
}

/**
 * What flux-hls packages into fMP4 HLS as-is (`fmp4_video` / `fmp4_audio`).
 * Not MP3: Chrome's and Firefox's MSE reject MP3 in fMP4, so it becomes AAC.
 */
const FMP4_VIDEO: Codec[] = ['h264', 'hevc', 'av1'];
const FMP4_AUDIO: Codec[] = ['aac', 'ac3', 'eac3', 'opus', 'flac'];

/** Containers flux-hls always indexes (Cue-less Matroska by a cluster scan, fragmented MP4 from `moof`). */
const INDEXABLE: ProbeResult['container'][] = ['matroska', 'web_m', 'mp4', 'mov'];

/** `Codec::is_text_subtitle()`: convertible to WebVTT without OCR. */
const TEXT_SUBS: Codec[] = ['subrip', 'ass', 'ssa', 'web_vtt', 'mov_text'];

/** "Main 10" → "main10", "High" → "high": the profile names ClientCaps uses. */
export function normalizeProfile(p: string | null): string | null {
  return p ? p.toLowerCase().replace(/\s+/g, '') : null;
}

/** The audio track playback starts with: the requested one, else the default, else the first. */
export function pickAudio(probe: ProbeResult, wanted: number | null): Track | undefined {
  const audio = probe.tracks.filter((t) => t.kind === 'audio');
  return audio.find((t) => t.index === wanted) ?? audio.find((t) => t.default) ?? audio[0];
}

/**
 * flux-hls offers every audio track of the file as an HLS rendition, the
 * chosen one first (`PlanPrefs::all_audio_tracks`), so the player can switch
 * audio inside the session (player/audio.ts). Direct play carries only the
 * file. The extra tracks get the same treatment as the chosen one: copied
 * when this device decodes them and fMP4 can carry them, else AAC.
 */
export function addOtherAudioTracks(plan: PlaybackPlan, probe: ProbeResult, caps: ClientCaps): void {
  if (plan.delivery === 'progressive_file') return;
  for (const t of probe.tracks) {
    if (t.kind !== 'audio' || plan.audio.some((a) => a.track === t.index)) continue;
    const cap = caps.audio.find((c) => c.codec === t.codec);
    const ch = t.channels ?? 2;
    const copy = cap !== undefined && ch <= cap.max_channels && FMP4_AUDIO.includes(t.codec);
    const outCh = Math.min(ch, 6);
    plan.audio.push({ track: t.index, op: copy ? { op: 'copy' } : { op: 'transcode', codec: 'aac', channels: outCh, bitrate: outCh > 2 ? 640_000 : 192_000 } });
  }
}

export function predictPlan({ probe, caps, audioTrack, subtitleTrack, subtitleFiles }: PlanInput): PredictedPlan {
  const reasons: Reason[] = [];
  const video = probe.tracks.find((t) => t.kind === 'video');
  const audio = pickAudio(probe, audioTrack);
  const sub = subtitleTrack === null ? undefined : probe.tracks.find((t) => t.index === subtitleTrack && t.kind === 'subtitle');

  let transcodeVideo = false;
  let tonemap = false;
  const fixes: Extract<VideoOp, { op: 'copy' }>['fixes'] = [];

  // Video.
  if (video) {
    const cap = caps.video.find((c) => c.codec === video.codec);
    const profile = normalizeProfile(video.profile);
    if (!cap) {
      reasons.push({ reason: 'video_codec_unsupported', codec: video.codec });
      transcodeVideo = true;
    } else {
      if (profile && cap.profiles.length > 0 && !cap.profiles.includes(profile) && video.codec !== 'h264') {
        reasons.push({ reason: 'video_profile_unsupported', codec: video.codec, profile: video.profile ?? profile });
        transcodeVideo = true;
      }
      if (video.bit_depth && cap.max_bit_depth && video.bit_depth > cap.max_bit_depth) {
        reasons.push({ reason: 'video_bit_depth_unsupported', bit_depth: video.bit_depth });
        transcodeVideo = true;
      }
      if (video.width && video.height && cap.max_width && cap.max_height && (video.width > cap.max_width || video.height > cap.max_height)) {
        reasons.push({ reason: 'video_resolution_too_high', width: video.width, height: video.height });
        transcodeVideo = true;
      }
      const hdr = video.hdr?.format;
      if (hdr && hdr !== 'sdr') {
        const dovi = video.hdr?.dovi;
        if (hdr === 'dolby_vision' && dovi && !cap.dovi_profiles.includes(dovi.profile)) {
          if (dovi.profile === 7 && cap.dovi_profiles.includes(8)) {
            fixes.push('dovi_p7_to81');
          } else if (dovi.bl_signal_compat_id === 1 || dovi.bl_signal_compat_id === 6) {
            // HDR10-compatible base layer: plays as HDR10 where DV isn't supported.
            if (!cap.hdr.includes('hdr10')) {
              reasons.push({ reason: 'hdr_unsupported', format: 'hdr10' });
              transcodeVideo = tonemap = true;
            }
          } else {
            reasons.push({ reason: 'dovi_profile_unsupported', profile: dovi.profile });
            transcodeVideo = tonemap = true;
          }
        } else if (hdr !== 'dolby_vision' && !cap.hdr.includes(hdr)) {
          reasons.push({ reason: 'hdr_unsupported', format: hdr });
          transcodeVideo = tonemap = true;
        }
      }
    }
  }

  // Subtitles: text → WebVTT sidecar; image → client render if possible, else burn in.
  const subtitles: SubtitlePlan[] = [];
  const subFile = sub ? undefined : subtitleFiles?.find((f) => f.track === subtitleTrack);
  if (sub) {
    // ASS/SSA lose styling as WebVTT; client rendering (JASSUB) isn't bundled yet.
    if (TEXT_SUBS.includes(sub.codec)) {
      subtitles.push({ track: sub.index, op: { op: 'web_vtt' }, file: null });
    } else if (caps.subtitles.includes(sub.codec)) {
      subtitles.push({ track: sub.index, op: { op: 'client_render' }, file: null });
    } else {
      reasons.push({ reason: 'subtitle_needs_burn_in', codec: sub.codec });
      subtitles.push({ track: sub.index, op: { op: 'burn_in' }, file: null });
      transcodeVideo = true;
    }
  } else if (subFile) {
    // A file is never sent as it is: text as WebVTT, a picture format burned in.
    if (TEXT_SUBS.includes(subFile.codec)) {
      subtitles.push({ track: subFile.track, op: { op: 'web_vtt' }, file: subFile });
    } else {
      reasons.push({ reason: 'subtitle_needs_burn_in', codec: subFile.codec });
      subtitles.push({ track: subFile.track, op: { op: 'burn_in' }, file: subFile });
      transcodeVideo = true;
    }
  }

  // Audio.
  const audioPlans: AudioPlan[] = [];
  if (audio) {
    const cap = caps.audio.find((c) => c.codec === audio.codec);
    const ch = audio.channels ?? 2;
    if (!cap) {
      reasons.push({ reason: 'audio_codec_unsupported', codec: audio.codec });
    } else if (ch > cap.max_channels) {
      reasons.push({ reason: 'audio_channels_too_many', channels: ch, max: cap.max_channels });
    }
    const convert = !cap || ch > cap.max_channels;
    const outCh = Math.min(ch, 6);
    audioPlans.push({
      track: audio.index,
      op: convert ? { op: 'transcode', codec: 'aac', channels: outCh, bitrate: outCh > 2 ? 640_000 : 192_000 } : { op: 'copy' },
    });
  }
  let audioConverts = audioPlans.some((a) => a.op.op !== 'copy');

  // Bitrate vs path capacity.
  if (caps.max_bitrate && probe.bitrate && probe.bitrate > caps.max_bitrate) {
    reasons.push({ reason: 'bitrate_too_high', bitrate: probe.bitrate, max: caps.max_bitrate });
    transcodeVideo = true;
  }

  // Container: direct play only when nothing else needs changing.
  const containerOk = caps.progressive_containers.includes(probe.container);
  const direct = containerOk && !transcodeVideo && !audioConverts && fixes.length === 0 && caps.delivery.includes('progressive_file');
  // Over HLS, a codec the client decodes may still not fit in fMP4 (TrueHD, DTS, PCM, Vorbis; VP9 video).
  if (!direct) {
    if (video && !transcodeVideo && !FMP4_VIDEO.includes(video.codec)) {
      reasons.push({ reason: 'codec_not_packageable', codec: video.codec });
      transcodeVideo = true;
    }
    for (const a of audioPlans) {
      if (a.op.op === 'copy' && audio && !FMP4_AUDIO.includes(audio.codec)) {
        reasons.push({ reason: 'codec_not_packageable', codec: audio.codec });
        const outCh = Math.min(audio.channels ?? 2, 6);
        a.op = { op: 'transcode', codec: 'aac', channels: outCh, bitrate: outCh > 2 ? 640_000 : 192_000 };
        audioConverts = true;
      }
    }
  }
  // When the video is re-encoded anyway, the container is moot: don't also claim a lossless repackage.
  if (!direct && !INDEXABLE.includes(probe.container) && !transcodeVideo) {
    reasons.push({ reason: 'no_index', container: probe.container });
    transcodeVideo = true;
  }
  // "Repackages the streams (no quality loss)" only when the video is copied (as plan.rs).
  if (!direct && !containerOk && !transcodeVideo) reasons.unshift({ reason: 'container_unsupported', container: probe.container });

  const delivery: DeliveryFormat = direct ? 'progressive_file' : caps.delivery.includes('hls_fmp4') ? 'hls_fmp4' : 'hls_ts';
  const srcW = video?.width ?? 1920;
  const srcH = video?.height ?? 1080;
  const scale = Math.min(1, 1920 / srcW, 1080 / srcH);
  const videoOp: VideoOp = transcodeVideo
    ? {
        op: 'transcode',
        codec: 'h264',
        width: Math.round((srcW * scale) / 2) * 2,
        height: Math.round((srcH * scale) / 2) * 2,
        bitrate: 8_000_000,
        tonemap,
        burn_subtitle: subtitles.find((s) => s.op.op === 'burn_in')?.track ?? null,
        hardware: true,
      }
    : { op: 'copy', fixes };

  const plan: PlaybackPlan = {
    delivery,
    video_track: video?.index ?? null,
    video: videoOp,
    audio: audioPlans,
    subtitles,
    demuxed_audio: delivery !== 'progressive_file' && caps.needs_demuxed_audio,
    reasons,
    // Rough server cost (research/03 §4.6, §6.2): remux ≈ free, audio ≈ 1.5% core, video ≈ 10%.
    est_cpu_ms_per_min: direct ? 0 : transcodeVideo ? 6000 : audioConverts ? 900 : 60,
  };
  return { plan, explain: reasons.map(explainReason) };
}
