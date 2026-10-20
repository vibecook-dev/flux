/**
 * Client-side playback measurements, as defined in research/03 §13:
 *
 * - TTFF: the user's Play click (t0) → the first video frame presented,
 *   taken from `requestVideoFrameCallback` metadata.presentationTime.
 * - Seek latency: `seeking` → the first frame presented after `seeked`.
 * - Stalls: `waiting` events after the first frame, outside seeks.
 * - Dropped frames: `getVideoPlaybackQuality()`.
 *
 * Without rVFC (old Firefox) the fallback is `playing`/`seeked` + one rAF,
 * which slightly under-reports; `precise` says which one was used.
 */

type VideoWithRvfc = HTMLVideoElement & {
  requestVideoFrameCallback?: (cb: (now: number, meta: { presentationTime?: number; mediaTime?: number }) => void) => number;
  cancelVideoFrameCallback?: (handle: number) => void;
};

export interface MetricsSnapshot {
  ttffMs: number | null;
  lastSeekMs: number | null;
  seekSamples: number[];
  stalls: number;
  droppedFrames: number;
  totalFrames: number;
  bufferAheadS: number;
  precise: boolean;
}

export class PlaybackMetrics {
  private video: VideoWithRvfc | null = null;
  private t0: number;
  private ttff: number | null = null;
  private seekStart: number | null = null;
  private lastSeek: number | null = null;
  private seeks: number[] = [];
  private stalls = 0;
  private listeners: [string, EventListener][] = [];
  private onChange: () => void;
  readonly precise: boolean;

  constructor(t0: number, onChange: () => void = () => {}) {
    this.t0 = t0;
    this.onChange = onChange;
    this.precise = typeof HTMLVideoElement !== 'undefined' && 'requestVideoFrameCallback' in HTMLVideoElement.prototype;
  }

  /** Measure from a new origin (e.g. after switching audio, which restarts the stream). */
  restart(t0: number): void {
    this.t0 = t0;
    this.ttff = null;
    this.armFirstFrame();
  }

  attach(video: HTMLVideoElement): void {
    this.video = video;
    const on = (type: string, fn: EventListener) => {
      video.addEventListener(type, fn);
      this.listeners.push([type, fn]);
    };
    this.armFirstFrame();
    on('seeking', () => {
      if (this.ttff === null) return; // the initial resume seek is part of TTFF
      this.seekStart = performance.now();
    });
    on('seeked', () => {
      if (this.seekStart === null) return;
      const started = this.seekStart;
      this.nextFrame((at) => {
        if (this.seekStart !== started) return; // superseded by a newer seek
        this.seekStart = null;
        this.lastSeek = Math.max(0, Math.round(at - started));
        this.seeks.push(this.lastSeek);
        this.onChange();
      });
    });
    on('waiting', () => {
      if (this.ttff !== null && this.seekStart === null) {
        this.stalls++;
        this.onChange();
      }
    });
  }

  private armFirstFrame() {
    this.nextFrame((at) => {
      if (this.ttff === null) {
        this.ttff = Math.max(0, Math.round(at - this.t0));
        this.onChange();
      }
    }, 'playing');
  }

  /** Call `cb` with the presentation time of the next frame. */
  private nextFrame(cb: (at: number) => void, fallbackEvent = 'seeked') {
    const v = this.video;
    if (!v) return;
    if (v.requestVideoFrameCallback) {
      v.requestVideoFrameCallback((now, meta) => cb(meta.presentationTime ?? now));
      return;
    }
    if (fallbackEvent === 'seeked') {
      requestAnimationFrame(() => cb(performance.now()));
      return;
    }
    const once = () => {
      v.removeEventListener(fallbackEvent, once);
      requestAnimationFrame(() => cb(performance.now()));
    };
    v.addEventListener(fallbackEvent, once);
  }

  snapshot(): MetricsSnapshot {
    const v = this.video;
    let dropped = 0;
    let total = 0;
    let ahead = 0;
    if (v) {
      const q = typeof v.getVideoPlaybackQuality === 'function' ? v.getVideoPlaybackQuality() : null;
      dropped = q?.droppedVideoFrames ?? 0;
      total = q?.totalVideoFrames ?? 0;
      for (let i = 0; i < v.buffered.length; i++) {
        if (v.buffered.start(i) <= v.currentTime + 0.25 && v.buffered.end(i) >= v.currentTime) {
          ahead = v.buffered.end(i) - v.currentTime;
        }
      }
    }
    return {
      ttffMs: this.ttff,
      lastSeekMs: this.lastSeek,
      seekSamples: [...this.seeks],
      stalls: this.stalls,
      droppedFrames: dropped,
      totalFrames: total,
      bufferAheadS: ahead,
      precise: this.precise,
    };
  }

  detach(): void {
    for (const [type, fn] of this.listeners) this.video?.removeEventListener(type, fn);
    this.listeners = [];
    this.video = null;
  }
}

/** Median of a sample (for the overlay's "p50 of N seeks"). */
export function median(xs: number[]): number | null {
  if (xs.length === 0) return null;
  const s = [...xs].sort((a, b) => a - b);
  const m = Math.floor(s.length / 2);
  return s.length % 2 ? s[m] : Math.round((s[m - 1] + s[m]) / 2);
}
