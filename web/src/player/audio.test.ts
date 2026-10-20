/**
 * Audio switching decisions (e2e P2 #4, PB-6): switch renditions inside the
 * session, and start a new one only where the player can't.
 */
import { describe, expect, it } from 'vitest';
import type { AudioPlan, AudioRef, PlayResponse } from '../api/types';
import { audioMenuNote, findHlsAudioTrack, findNativeAudioTrack, planAudioSwitch, playingTrack, renditionKey, startingRendition } from './audio';

const ref = (rendition: string, track: number, codec: AudioRef['codec'], label: string, language: string | null = 'eng', isDefault = false): AudioRef => ({
  rendition,
  track,
  label,
  language,
  codec,
  channels: 2,
  default: isDefault,
});

const response = (delivery: PlayResponse['plan']['delivery'], audio: AudioRef[], planAudio: AudioPlan[]): PlayResponse => ({
  session_id: 's1',
  asset_id: 10,
  plan: { delivery, video_track: 0, video: { op: 'copy', fixes: [] }, audio: planAudio, subtitles: [], demuxed_audio: delivery !== 'progressive_file', reasons: [], est_cpu_ms_per_min: 0 },
  url: delivery === 'progressive_file' ? '/media/10/file' : '/media/p1/master.m3u8',
  explain: [],
  start_ms: 0,
  duration_ms: 60_000,
  prefetch: [],
  audio,
  subtitles: [],
});

// The e2e multi-audio file on Chrome after picking Japanese: AC-3 5.1 → AAC stereo first, English AAC copied.
const jpnFirst = response(
  'hls_fmp4',
  [ref('2-aac2', 2, 'aac', 'Japanese 5.1 (AAC Stereo)', 'jpn', true), ref('1', 1, 'aac', 'English (AAC Stereo)')],
  [
    { track: 2, op: { op: 'transcode', codec: 'aac', channels: 2, bitrate: 192_000 } },
    { track: 1, op: { op: 'copy' } },
  ],
);

describe('planAudioSwitch', () => {
  it('switches between renditions of an HLS session in place (it used to always start a new /play)', () => {
    const start = startingRendition(jpnFirst);
    expect(start?.rendition).toBe('2-aac2');
    expect(planAudioSwitch(jpnFirst, start, 1, 'hls.js')).toEqual({ kind: 'in-session', ref: jpnFirst.audio[1] });
    // ...and back again, still in place.
    expect(planAudioSwitch(jpnFirst, jpnFirst.audio[1], 2, 'hls.js')).toEqual({ kind: 'in-session', ref: jpnFirst.audio[0] });
    expect(planAudioSwitch(jpnFirst, jpnFirst.audio[1], 2, 'native-hls')).toEqual({ kind: 'in-session', ref: jpnFirst.audio[0] });
  });

  it('does nothing for the track already playing', () => {
    expect(planAudioSwitch(jpnFirst, startingRendition(jpnFirst), 2, 'hls.js')).toEqual({ kind: 'none' });
  });

  it('direct play still needs a new session: the browser plays the file’s default track', () => {
    const direct = response('progressive_file', [], [{ track: 1, op: { op: 'copy' } }]);
    expect(planAudioSwitch(direct, null, 2, 'progressive')).toEqual({ kind: 'restart', reason: 'direct-play' });
    expect(planAudioSwitch(direct, null, 1, 'progressive')).toEqual({ kind: 'none' });
    expect(playingTrack(direct, null)).toBe(1);
    expect(audioMenuNote(direct)).toMatch(/restarts the stream/);
    expect(audioMenuNote(jpnFirst)).toBeNull();
  });

  it('a track the session has no rendition of takes a new session', () => {
    expect(planAudioSwitch(jpnFirst, startingRendition(jpnFirst), 3, 'hls.js')).toEqual({ kind: 'restart', reason: 'not-in-session' });
  });

  it('hls.js can’t leave a FLAC or Opus SourceBuffer for another codec (hls.md C.1 #3): that one restarts', () => {
    // Opus eng copied + AC-3 jpn → AAC: the case hls.md lists as still affected.
    const opus = response(
      'hls_fmp4',
      [ref('1', 1, 'opus', 'English (Opus Stereo)', 'eng', true), ref('2-aac2', 2, 'aac', 'Japanese (AAC Stereo)', 'jpn')],
      [
        { track: 1, op: { op: 'copy' } },
        { track: 2, op: { op: 'transcode', codec: 'aac', channels: 2, bitrate: 192_000 } },
      ],
    );
    expect(planAudioSwitch(opus, opus.audio[0], 2, 'hls.js')).toEqual({ kind: 'restart', reason: 'codec-change' });
    // Native HLS (Safari) has no such bug, and AAC → Opus is a changeType hls.js does make.
    expect(planAudioSwitch(opus, opus.audio[0], 2, 'native-hls')).toEqual({ kind: 'in-session', ref: opus.audio[1] });
    expect(planAudioSwitch(opus, opus.audio[1], 1, 'hls.js')).toEqual({ kind: 'in-session', ref: opus.audio[0] });
    // FLAC → a rendition of the same codec stays in place.
    const flac = response(
      'hls_fmp4',
      [ref('1', 1, 'flac', 'English (FLAC Stereo)', 'eng', true), ref('2', 2, 'flac', 'Commentary (FLAC Stereo)')],
      [
        { track: 1, op: { op: 'copy' } },
        { track: 2, op: { op: 'copy' } },
      ],
    );
    expect(planAudioSwitch(flac, flac.audio[0], 2, 'hls.js')).toEqual({ kind: 'in-session', ref: flac.audio[1] });
  });

  it('a track with several renditions gets the one the server lists first, unless hls.js is stuck on its codec', () => {
    // Apple TV-like plan: TrueHD → E-AC-3 5.1 first, AAC stereo fallback.
    const both = response(
      'hls_fmp4',
      [ref('1', 1, 'aac', 'English (AAC Stereo)', 'eng', true), ref('2-eac36', 2, 'eac3', 'French (E-AC-3 5.1)', 'fre'), ref('2-aac2', 2, 'aac', 'French (AAC Stereo)', 'fre')],
      [
        { track: 1, op: { op: 'copy' } },
        { track: 2, op: { op: 'transcode', codec: 'eac3', channels: 6, bitrate: 640_000 } },
        { track: 2, op: { op: 'transcode', codec: 'aac', channels: 2, bitrate: 192_000 } },
      ],
    );
    expect(planAudioSwitch(both, both.audio[0], 2, 'native-hls')).toEqual({ kind: 'in-session', ref: both.audio[1] });
    const fromOpus = { ...both, audio: [{ ...both.audio[0], codec: 'opus' as const }, ...both.audio.slice(1)] };
    expect(planAudioSwitch(fromOpus, fromOpus.audio[0], 2, 'hls.js')).toEqual({ kind: 'restart', reason: 'codec-change' });
  });
});

describe('rendition keys and track lookup', () => {
  it('renditionKey names plan entries as flux-hls does ("1", "1-aac2", "1-eac36")', () => {
    expect(renditionKey({ track: 1, op: { op: 'copy' } })).toBe('1');
    expect(renditionKey({ track: 1, op: { op: 'transcode', codec: 'aac', channels: 2, bitrate: 192_000 } })).toBe('1-aac2');
    expect(renditionKey({ track: 3, op: { op: 'transcode', codec: 'eac3', channels: 6, bitrate: 640_000 } })).toBe('3-eac36');
    expect(jpnFirst.plan.audio.map(renditionKey)).toEqual(jpnFirst.audio.map((a) => a.rendition));
  });

  it('finds the hls.js track by its playlist URL, across groups, else by NAME', () => {
    const tracks = [
      { url: 'http://127.0.0.1:8096/media/p1/a/1/index.m3u8', name: 'English (AAC Stereo)', groupId: 'a0' },
      { url: 'http://127.0.0.1:8096/media/p1/a/2-aac2/index.m3u8', name: 'Japanese 5.1 (AAC Stereo)', groupId: 'a0' },
      { url: 'http://127.0.0.1:8096/media/p1/a/12/index.m3u8', name: 'Commentary', groupId: 'a1' },
    ];
    expect(findHlsAudioTrack(tracks, jpnFirst.audio[0])).toBe(tracks[1]);
    expect(findHlsAudioTrack(tracks, jpnFirst.audio[1])).toBe(tracks[0]); // "a/1/", not "a/12/"
    expect(findHlsAudioTrack(tracks, ref('9', 9, 'aac', 'Commentary'))).toBe(tracks[2]);
    expect(findHlsAudioTrack(tracks, ref('9', 9, 'aac', 'Nope'))).toBeUndefined();
  });

  it('finds the native (Safari) track by label, else the only one in that language', () => {
    const native = [
      { label: 'English (AAC Stereo)', language: 'en', enabled: true },
      { label: 'Japanese 5.1 (AAC Stereo)', language: 'ja', enabled: false },
    ];
    expect(findNativeAudioTrack(native, jpnFirst.audio[0])).toBe(native[1]);
    expect(findNativeAudioTrack(native, ref('5', 5, 'aac', 'Renamed', 'jpn'))).toBe(native[1]);
    expect(findNativeAudioTrack(native, ref('5', 5, 'aac', 'Renamed', 'fre'))).toBeUndefined();
  });
});
