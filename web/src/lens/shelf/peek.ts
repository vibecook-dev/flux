/**
 * Pointing at a tape on the Shelf: it lifts at once, and when the pointer rests on
 * it, it comes off the shelf and turns to show its cover. Pure maths, shared by the
 * GPU Shelf (gpu-shelf.ts); the DOM Shelf plays the same moves with CSS.
 *
 * - **Springs, not tweens.** A tape's lift and its turn are springs (`Spring`):
 *   pointing elsewhere mid-turn reverses it from where it is, at the speed it has.
 * - **Out, then round.** A tape is deeper than it is wide, so turning it in its slot
 *   would swing its back corners through its neighbours. The turn waits until it is
 *   most of the way out (`peekPath`), and it comes out far enough to clear them at
 *   every angle (`peekDepth`, checked against `intrusion` in the tests).
 * - **Intent.** A pointer sweeping past lifts tapes but turns none; one that rests,
 *   or moves slowly, turns the tape under it (`peekWait`).
 */
import type { TapeMove } from './transform';

export interface Spring {
  x: number;
  v: number;
  to: number;
}

export const spring0 = (x = 0): Spring => ({ x, v: 0, to: x });

/**
 * Advance a damped spring (stiffness k, damping c, unit mass) by `dt` seconds, in
 * small steps so a slow frame can't upset it. False once it has settled on its target.
 */
export function stepSpring(s: Spring, dt: number, k: number, c: number): boolean {
  let t = Math.min(Math.max(dt, 0), 0.1);
  while (t > 1e-6) {
    const h = Math.min(t, 1 / 240);
    s.v += (-k * (s.x - s.to) - c * s.v) * h;
    s.x += s.v * h;
    t -= h;
  }
  if (Math.abs(s.x - s.to) < 5e-4 && Math.abs(s.v) < 5e-3) {
    s.x = s.to;
    s.v = 0;
    return false;
  }
  return true;
}

/** The lift when pointed at: quick, a touch of overshoot. */
export const LIFT_K = 320;
export const LIFT_C = 27;
/** The turn out to the cover (a little overshoot) and back onto the shelf (none: it seats). */
export const PEEK_K = 175;
export const PEEK_C = 21;
export const BACK_K = 210;
export const BACK_C = 29;

/** A tape's size on this shelf: height, plain spine width, depth, its own spine width, the gap between tapes. */
export interface PoseDims {
  H: number;
  W: number;
  D: number;
  tw: number;
  gap: number;
}

/** How far out a tape comes to show its cover: its turning box clears the tapes beside it at every angle. */
export function peekDepth(d: PoseDims): number {
  // The box turns about its centre, D/2 behind the spines; turned by φ it reaches
  // D/2 + (tw/2)·sin φ + (D/2)·cos φ back, at most half its diagonal.
  return Math.ceil(d.D / 2 + Math.hypot(d.tw, d.D) / 2 + Math.max(6, d.W * 0.3));
}

const smoother = (t: number) => {
  const x = Math.min(1, Math.max(0, t));
  return x * x * x * (x * (x * 6 - 15) + 10);
};

/** Where the turn starts and the slide out ends, along the peek (0–1). */
export const TURN_FROM = 0.3;
const OUT_BY = 0.58;

/**
 * Along a peek p (0 on the shelf, 1 showing its cover; a spring may overshoot): how
 * far out the tape is (0–1 of `peekDepth`) and how far it has turned (0–1 of 90°).
 */
export function peekPath(p: number): { out: number; turn: number } {
  const q = Math.max(0, p);
  const t = Math.min(1, q / OUT_BY);
  // Beyond 1 the spring's overshoot carries it a touch further out and round.
  const over = Math.max(0, q - 1);
  const out = 1 - (1 - t) ** 3 + over * 0.6;
  const turn = smoother((q - TURN_FROM) / (1 - TURN_FROM)) + over * 1.2;
  return { out, turn };
}

const TIP = (-5 * Math.PI) / 180;

/**
 * The tape's pose for its lift (0–1, pointed at) and its peek (0–1, see `peekPath`),
 * as the DOM Shelf's classes would move it: the lift raises it, draws it out a little
 * and tips its top towards you; the peek brings it out and turns its cover to you.
 */
export function tapePose(lift: number, peek: number, d: PoseDims, out: TapeMove = { ty: 0, tz: 0, rx: 0, ry: 0, s: 1 }): TapeMove {
  const { out: o, turn } = peekPath(peek);
  const oo = Math.min(1, o);
  const l = Math.max(0, lift) * (1 - oo);
  out.tz = d.W * 0.9 * l + peekDepth(d) * o;
  out.ty = -d.H * 0.055 * l - d.H * 0.05 * oo;
  out.rx = TIP * Math.max(0, lift) * (1 - Math.min(1, turn));
  out.ry = (-Math.PI / 2) * turn;
  out.s = 1;
  return out;
}

/**
 * How far a turning tape reaches into its neighbours' slots behind the spines (CSS px,
 * 0: clear): the footprint of a tape tw wide and D deep, turned by `phi` about its
 * centre and `tz` out, against the slots beside its own (from `gap` away).
 */
export function intrusion(tw: number, D: number, gap: number, phi: number, tz: number): number {
  const c = Math.cos(phi);
  const s = Math.sin(phi);
  // Corners about the centre (x across, z out of the shelf), turned (ry = −φ), in the world (spines at z = 0).
  const pts: [number, number][] = [];
  for (const [x, z] of [
    [-tw / 2, -D / 2],
    [tw / 2, -D / 2],
    [tw / 2, D / 2],
    [-tw / 2, D / 2],
  ]) {
    pts.push([c * x - s * z, s * x + c * z - D / 2 + tz]);
  }
  const edge = tw / 2 + gap;
  let worst = 0;
  const see = (x: number, z: number) => {
    if (Math.abs(x) >= edge - 1e-9 && z < 0) worst = Math.max(worst, -z);
  };
  for (let k = 0; k < 4; k++) {
    const [x0, z0] = pts[k];
    const [x1, z1] = pts[(k + 1) % 4];
    see(x0, z0);
    // Where an edge of the footprint crosses into a neighbour's slot.
    for (const e of [edge, -edge]) {
      if ((x0 - e) * (x1 - e) < 0) {
        const t = (e - x0) / (x1 - x0);
        see(e, z0 + (z1 - z0) * t);
      }
    }
  }
  return worst;
}

/**
 * The tapes beside a peeking one ease aside: `amount` at the next tape, then less, each
 * gap closing at most to nothing (so no two tapes ever overlap). `gaps[k]`: the gap
 * after the k-th tape out from it. Returns the offsets, nearest first.
 */
export function asideFalloff(amount: number, gaps: readonly number[]): number[] {
  const out: number[] = [];
  let s = amount;
  for (let k = 0; s > 1e-3; k++) {
    out.push(s);
    if (k >= gaps.length) break;
    s = Math.max(0, s - gaps[k]);
  }
  return out;
}

/** Resting this long on a tape turns it (ms); moving makes it wait longer, a fast sweep never. */
export const DWELL = 90;
export const FAST = 0.9;

/**
 * How much longer (ms) before the tape under the pointer turns, after `dwell` ms on it
 * with the pointer moving at `speed` px/ms; 0: now. Infinity: too fast, ask again later.
 */
export function peekWait(dwell: number, speed: number): number {
  if (speed > FAST) return Number.POSITIVE_INFINITY;
  const need = DWELL + Math.min(speed, FAST) * 160;
  return Math.max(0, need - dwell);
}
