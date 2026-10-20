/**
 * Attaches a PlayResponse to a <video> element with the right engine
 * (caps/engine.ts): progressive file, hls.js over MSE, or native HLS.
 *
 * hls.js settings are tuned for time-to-first-frame on a VOD stream from a
 * nearby server (research/03 §11):
 * - `startPosition` begins at the resume point (no seek after start);
 * - `testBandwidth: false` skips the throw-away bandwidth probe;
 * - `startFragPrefetch` fetches the first fragment while the init loads;
 * - `progressive: false`: flux produces each segment whole before sending
 *   it, and progressive parsing made Chrome slower on large segments (4K:
 *   first frame 1.7 s vs 0.59 s, seeks 1-3 s vs 0.09-0.28 s;
 *   docs/prototype/hls.md).
 */
import type Hls from 'hls.js';
import type { ErrorData } from 'hls.js';
import { resolveUrl } from '../config';
import type { AudioRef, PlayResponse } from '../api/types';
import { findHlsAudioTrack, findNativeAudioTrack, type NativeAudioTrack } from './audio';
import { isAppleWebKit } from '../caps/detect';
import { choosePlayerEngine, type PlayerEngine } from '../caps/engine';

export interface AttachedEngine {
  engine: PlayerEngine;
  hls: Hls | null;
  /** hls.js version, for the stats overlay. */
  version: string | null;
  /**
   * Switch to another audio rendition of this session in place (no new
   * /play). `false` when the player can't: no such rendition, or no audio
   * track API (the caller then starts a new session). See player/audio.ts.
   */
  selectAudio(ref: AudioRef): boolean;
  destroy(): void;
}

export interface AttachOptions {
  onFatal: (message: string) => void;
  /** The rendition asked for with `selectAudio` is now the one playing (hls.js: its audio is buffered). */
  onAudioSwitched?: (ref: AudioRef) => void;
}

/** Safari's `HTMLMediaElement.audioTracks` (not in TypeScript's DOM types). */
type NativeAudioTrackList = ArrayLike<NativeAudioTrack> & EventTarget;

function nativeAudioTracks(video: HTMLVideoElement): NativeAudioTrackList | null {
  const list = (video as HTMLVideoElement & { audioTracks?: NativeAudioTrackList }).audioTracks;
  return list && typeof list.length === 'number' ? list : null;
}

/** In-place audio switching on the `<video>` element's own track list (native HLS). */
function nativeSelectAudio(video: HTMLVideoElement, opts: AttachOptions): (ref: AudioRef) => boolean {
  return (ref) => {
    const list = nativeAudioTracks(video);
    if (!list || list.length < 2) return false;
    const tracks = Array.from(list);
    const target = findNativeAudioTrack(tracks, ref);
    if (!target) return false;
    // Enable first, then disable the rest, so there is never a moment with no audio track.
    target.enabled = true;
    for (const t of tracks) if (t !== target) t.enabled = false;
    opts.onAudioSwitched?.(ref);
    return true;
  };
}

export async function attachEngine(video: HTMLVideoElement, res: PlayResponse, opts: AttachOptions): Promise<AttachedEngine> {
  const url = resolveUrl(res.url);
  const startS = res.start_ms / 1000;
  const nativeHls = video.canPlayType('application/vnd.apple.mpegurl') !== '';
  // Load hls.js only if this browser will use it (preloaded on Play intent).
  const needHlsJs = res.plan.delivery !== 'progressive_file' && !(isAppleWebKit(navigator.userAgent) && nativeHls);
  const HlsCtor = needHlsJs ? (await import('hls.js')).default : null;
  const engine = choosePlayerEngine(res.plan.delivery, {
    userAgent: navigator.userAgent,
    hlsJsSupported: HlsCtor?.isSupported() ?? false,
    nativeHls,
  });

  if (engine === null) {
    opts.onFatal('This browser can’t play HLS streams. Try a recent Chrome, Firefox, Edge or Safari.');
    return { engine: 'progressive', hls: null, version: null, selectAudio: () => false, destroy() {} };
  }

  if (engine === 'progressive' || engine === 'native-hls') {
    if (startS > 0) {
      const seekOnce = () => {
        video.removeEventListener('loadedmetadata', seekOnce);
        // A resume point at or past the end (stale progress, a shorter file) starts from the top.
        if (Number.isFinite(video.duration) && startS >= video.duration - 1) return;
        video.currentTime = startS;
      };
      video.addEventListener('loadedmetadata', seekOnce);
    }
    video.src = url;
    return {
      engine,
      hls: null,
      version: null,
      // A progressive file plays its default track; another one needs a new session.
      selectAudio: engine === 'native-hls' ? nativeSelectAudio(video, opts) : () => false,
      destroy() {
        video.removeAttribute('src');
        video.load();
      },
    };
  }

  const HlsClass = HlsCtor as typeof Hls;
  const hls = new HlsClass({
    startPosition: startS > 0 ? startS : -1,
    testBandwidth: false,
    startFragPrefetch: true,
    progressive: false,
    enableWorker: true,
    lowLatencyMode: false,
    maxBufferLength: 30,
    // hls.js grows the buffer up to maxMaxBufferLength (600 s) for low bitrates;
    // keep it to a minute so a paused remote viewer doesn't pull a whole film.
    maxMaxBufferLength: 60,
    backBufferLength: 30,
  });
  let recoveredOnce = false;
  hls.on(HlsClass.Events.ERROR, (_evt, data: ErrorData) => {
    if (!data.fatal) return;
    if (data.type === HlsClass.ErrorTypes.MEDIA_ERROR && !recoveredOnce) {
      recoveredOnce = true;
      hls.recoverMediaError();
      return;
    }
    opts.onFatal(`${data.type === HlsClass.ErrorTypes.NETWORK_ERROR ? 'The stream stopped loading' : 'The stream could not be decoded'} (${data.details}).`);
  });
  // In-session audio switching: `setAudioOption` takes a track from any audio
  // group (flux-hls makes one group per codec) and switches the variant when
  // the group changes. AUDIO_TRACK_SWITCHED fires once the new track's audio
  // is buffered; that is when the switch is reported (and timed).
  let pendingAudio: AudioRef | null = null;
  hls.on(HlsClass.Events.AUDIO_TRACK_SWITCHED, (_evt, data) => {
    if (pendingAudio && findHlsAudioTrack([data], pendingAudio)) {
      const done = pendingAudio;
      pendingAudio = null;
      opts.onAudioSwitched?.(done);
    }
  });
  hls.loadSource(url);
  hls.attachMedia(video);
  return {
    engine,
    hls,
    version: HlsClass.version,
    selectAudio(ref) {
      const target = findHlsAudioTrack(hls.allAudioTracks, ref);
      if (!target) return false;
      pendingAudio = ref;
      if (hls.setAudioOption(target) === null) {
        pendingAudio = null;
        return false;
      }
      return true;
    },
    destroy() {
      hls.destroy();
    },
  };
}
