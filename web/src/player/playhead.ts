/**
 * Where the scrub bar's playhead is. Three sources, in order:
 *
 * 1. A drag in progress (the scrubber holds that itself).
 * 2. A seek on its way: from the moment you click, the playhead sits at the
 *    target until the video has landed there. It never shows the old
 *    position again in between, which is what made it jump back and forth.
 * 3. The video's own clock, read every frame and smoothed: between two
 *    readings it runs on at the playback rate, and during playback it never
 *    steps back by a fraction of a second, so the playhead glides.
 *
 * Plain data and functions (no DOM), so it is tested on its own.
 */

/** How close the video must be to a seek's target to count as there (s). */
const LANDED_WITHIN = 0.75;
/** A seek that never lands (a stream that won't load) stops holding the playhead after this (ms). */
const GIVE_UP_MS = 12_000;
/** How far the smoothing may run ahead of the video's last reading (s). */
const RUN_AHEAD = 0.25;
/** A backwards move bigger than this during playback is a real jump, not jitter (s). */
const JUMP = 1;

export interface Clock {
  /** Where a seek is taking the video; null when none is on its way. */
  target: number | null;
  /** When that seek began (ms, performance.now()). */
  targetAt: number;
  /** The video said `seeked` since the target was set. */
  landed: boolean;
  /** The video's last different reading, and when it was taken (-1: not yet). */
  reported: number;
  reportedAt: number;
  /** What the playhead showed last. */
  shown: number;
}

export interface VideoClock {
  /** `currentTime` (s). */
  time: number;
  /** Playing and moving: not paused, not seeking, not waiting for data. */
  advancing: boolean;
  /** `playbackRate`. */
  rate: number;
}

export function createClock(start = 0): Clock {
  return { target: null, targetAt: 0, landed: false, reported: start, reportedAt: -1, shown: start };
}

/** A seek begins: the playhead shows `t` from now until the video gets there. */
export function seekStarted(c: Clock, t: number, now: number): void {
  c.target = t;
  c.targetAt = now;
  c.landed = false;
  c.shown = t;
}

/** The video finished a seek (its `seeked` event). */
export function seekLanded(c: Clock): void {
  if (c.target !== null) c.landed = true;
}

/** Where a seek that is still on its way is going, or null. */
export function pendingTarget(c: Clock): number | null {
  return c.target;
}

/** The time to show now. */
export function shownTime(c: Clock, v: VideoClock, now: number): number {
  if (c.target !== null) {
    const there = Math.abs(v.time - c.target) < LANDED_WITHIN;
    if ((there && (c.landed || v.advancing)) || now - c.targetAt > GIVE_UP_MS) {
      c.target = null;
      c.reported = v.time;
      c.reportedAt = now;
      c.shown = v.time;
      return c.shown;
    }
    c.shown = c.target;
    return c.shown;
  }
  if (v.time !== c.reported || c.reportedAt < 0) {
    c.reported = v.time;
    c.reportedAt = now;
  }
  let t = c.reported;
  if (v.advancing) t += Math.min(RUN_AHEAD, (Math.max(0, now - c.reportedAt) / 1000) * v.rate);
  // While playing, hold rather than step back a little (the smoothing ran ahead of a late reading).
  if (v.advancing && t < c.shown && c.shown - t < JUMP) t = c.shown;
  c.shown = t;
  return t;
}

/** The fraction of the bar a time fills, 0..1. */
export function fraction(t: number, duration: number): number {
  if (!(duration > 0)) return 0;
  return Math.min(1, Math.max(0, t / duration));
}

/** The end of the buffered range the playhead is in (s), or the playhead itself. */
export function bufferedEnd(ranges: TimeRanges, t: number): number {
  let end = t;
  for (let i = 0; i < ranges.length; i++) {
    if (ranges.start(i) <= t + 0.25 && ranges.end(i) > end) end = ranges.end(i);
  }
  return end;
}
