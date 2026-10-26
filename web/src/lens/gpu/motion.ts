/**
 * Motion for GPU lenses, evaluated on the CPU each frame: the same curves the
 * CSS uses (so a GPU lens moves like the rest of the app), plus small helpers
 * for rectangles. Pure functions.
 */

/** A CSS cubic-bezier(x1, y1, x2, y2) as a function of progress 0–1. */
export function bezier(x1: number, y1: number, x2: number, y2: number): (p: number) => number {
  const cx = 3 * x1;
  const bx = 3 * (x2 - x1) - cx;
  const ax = 1 - cx - bx;
  const cy = 3 * y1;
  const by = 3 * (y2 - y1) - cy;
  const ay = 1 - cy - by;
  const x = (t: number) => ((ax * t + bx) * t + cx) * t;
  const y = (t: number) => ((ay * t + by) * t + cy) * t;
  const dx = (t: number) => (3 * ax * t + 2 * bx) * t + cx;
  return (p: number) => {
    if (p <= 0) return 0;
    if (p >= 1) return 1;
    // Newton, then bisection if the slope is too flat.
    let t = p;
    for (let i = 0; i < 6; i++) {
      const err = x(t) - p;
      const d = dx(t);
      if (Math.abs(err) < 1e-5) return y(t);
      if (Math.abs(d) < 1e-6) break;
      t -= err / d;
    }
    let lo = 0;
    let hi = 1;
    t = p;
    for (let i = 0; i < 24; i++) {
      const v = x(t);
      if (Math.abs(v - p) < 1e-5) break;
      if (v < p) lo = t;
      else hi = t;
      t = (lo + hi) / 2;
    }
    return y(t);
  };
}

/** The app's soft ease (`SOFT` in dom.ts): zooms and reflows. */
export const soft = bezier(0.25, 0.85, 0.3, 1);

/** `--spring` from tokens.css (a little overshoot): hover lifts. */
const SPRING = [0, 0.033, 0.116, 0.227, 0.352, 0.478, 0.596, 0.702, 0.794, 0.869, 0.929, 0.974, 1.007, 1.029, 1.042, 1.049, 1.05, 1.048, 1.043, 1.038, 1.031, 1.025, 1.019, 1.014, 1.01, 1.006, 1.003, 1.001, 0.999, 0.998, 0.998, 0.998, 0.998, 0.998, 0.998, 0.998, 1];

export function spring(p: number): number {
  if (p <= 0) return 0;
  if (p >= 1) return 1;
  const f = p * (SPRING.length - 1);
  const i = Math.floor(f);
  return SPRING[i] + (SPRING[i + 1] - SPRING[i]) * (f - i);
}

export interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

export function lerp(a: number, b: number, t: number): number {
  return a + (b - a) * t;
}

export function lerpRect(a: Rect, b: Rect, t: number): Rect {
  return { x: a.x + (b.x - a.x) * t, y: a.y + (b.y - a.y) * t, w: a.w + (b.w - a.w) * t, h: a.h + (b.h - a.h) * t };
}

/**
 * A transition from `from` to `to` that can be retargeted mid-flight: `value(now)`
 * is where it is; starting a new one from `value(now)` keeps motion continuous.
 */
export class Tween {
  from = 0;
  to = 0;
  start = 0;
  dur = 0;
  curve: (p: number) => number;

  constructor(curve: (p: number) => number, value = 0) {
    this.curve = curve;
    this.from = value;
    this.to = value;
  }

  value(now: number): number {
    if (now >= this.start + this.dur || this.dur <= 0) return this.to;
    return lerp(this.from, this.to, this.curve((now - this.start) / this.dur));
  }

  done(now: number): boolean {
    return now >= this.start + this.dur;
  }

  set(to: number, now: number, dur: number): void {
    if (to === this.to) return;
    this.from = this.value(now);
    this.to = to;
    this.start = now;
    this.dur = dur;
  }
}
