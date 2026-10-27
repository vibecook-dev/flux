import { describe, expect, it } from 'vitest';
import type { ItemSummary } from '../../api/types';
import { corners } from '../gpu/scene3d';
import { fallbackPalette } from '../palette';
import { planShelf, shelfLayout, shelfMetrics, spineLook } from './geometry';
import { asideFalloff, DWELL, FAST, intrusion, LIFT_C, LIFT_K, PEEK_C, PEEK_K, peekDepth, peekPath, peekWait, spring0, stepSpring, tapePose, TURN_FROM } from './peek';
import { backKey, backLines, grayHex, mixBlackOklab, spineKey } from './spine';
import { apply, boxCorners, euler, facing, mul, place, REST, rotX, rotY, rotZ, splitAtDepth, tapeFaces, type M3, type TapeMove, type V3 } from './transform';

// ── a CSS 3D reference: 4×4 matrices composed the way the DOM Shelf's CSS composes them ──

type M4 = number[];
const ident = (): M4 => [1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1];
function mul4(a: M4, b: M4): M4 {
  const o = new Array<number>(16).fill(0);
  for (let r = 0; r < 4; r++) for (let c = 0; c < 4; c++) for (let k = 0; k < 4; k++) o[r * 4 + c] += a[r * 4 + k] * b[k * 4 + c];
  return o;
}
const tr = (x: number, y: number, z: number): M4 => [1, 0, 0, x, 0, 1, 0, y, 0, 0, 1, z, 0, 0, 0, 1];
const sc = (s: number): M4 => [s, 0, 0, 0, 0, s, 0, 0, 0, 0, s, 0, 0, 0, 0, 1];
const from3 = (m: M3): M4 => [m[0], m[1], m[2], 0, m[3], m[4], m[5], 0, m[6], m[7], m[8], 0, 0, 0, 0, 1];
/** CSS `transform` about `transform-origin` o: T(o) · M · T(−o). */
const about = (o: V3, m: M4): M4 => mul4(mul4(tr(o[0], o[1], o[2]), m), tr(-o[0], -o[1], -o[2]));
const at = (m: M4, p: V3): V3 => [m[0] * p[0] + m[1] * p[1] + m[2] * p[2] + m[3], m[4] * p[0] + m[5] * p[1] + m[6] * p[2] + m[7], m[8] * p[0] + m[9] * p[1] + m[10] * p[2] + m[11]];

/** The tape's own transform, as `.tape` gets it: translate3d(0, ty, tz) rotateY(ry) rotateX(rx) scale(s), about 50% 100% −D/2. */
function cssTape(move: TapeMove, tw: number, H: number, D: number): M4 {
  const m = mul4(mul4(mul4(tr(0, move.ty, move.tz), from3(rotY(move.ry))), from3(rotX(move.rx))), sc(move.s));
  return about([tw / 2, H, -D / 2], m);
}

/** Each face element's box and transform (shelf.module.css), in the tape's space: corners TL, TR, BL, BR. */
function cssFace(face: 'spine' | 'top' | 'right' | 'left', tw: number, H: number, D: number): V3[] {
  let left = 0;
  let top = 0;
  let w = tw;
  let h = H;
  let m = ident();
  if (face === 'top') {
    top = -D;
    h = D;
    m = about([tw / 2, 0, 0], from3(rotX(Math.PI / 2)));
  } else if (face === 'right') {
    left = tw;
    w = D;
    m = about([tw, H / 2, 0], from3(rotY(Math.PI / 2)));
  } else if (face === 'left') {
    left = -D;
    w = D;
    m = about([0, H / 2, 0], from3(rotY(-Math.PI / 2)));
  }
  return [
    [left, top, 0],
    [left + w, top, 0],
    [left, top + h, 0],
    [left + w, top + h, 0],
  ].map((p) => at(m, p as V3));
}

const close = (a: number[], b: number[], eps = 1e-6) => a.every((v, i) => Math.abs(v - b[i]) < eps);

describe('transform: euler', () => {
  it('turns any rotation back into the angles the renderer applies (X, then Y, then Z)', () => {
    let seed = 7;
    const rnd = () => {
      seed = (seed * 16807) % 2147483647;
      return seed / 2147483647;
    };
    for (let k = 0; k < 200; k++) {
      const m = mul(rotZ((rnd() - 0.5) * 6), mul(rotY((rnd() - 0.5) * 3), rotX((rnd() - 0.5) * 6)));
      const [rx, ry, rz] = euler(m);
      expect(close(mul(rotZ(rz), mul(rotY(ry), rotX(rx))), m, 1e-9)).toBe(true);
    }
  });

  it('handles a quarter turn about Y (a side face), where X and Z turn about the same axis', () => {
    for (const ry of [Math.PI / 2, -Math.PI / 2]) {
      for (const other of [0, 0.3, -1.2]) {
        const m = mul(rotY(ry), rotX(other));
        const [rx, ry2, rz] = euler(m);
        expect(close(mul(rotZ(rz), mul(rotY(ry2), rotX(rx))), m, 1e-9)).toBe(true);
      }
    }
  });
});

describe('transform: place', () => {
  const tw = 39;
  const H = 294;
  const D = 162;
  const F = tapeFaces(tw, H, D);
  const slot: V3 = [120, 80, -1.5];
  const moves: Record<string, TapeMove> = {
    rest: REST,
    pulled: { ty: -16, tz: 35, rx: (-5 * Math.PI) / 180, ry: 0, s: 1 },
    out: { ty: -15, tz: 129, rx: 0, ry: 0, s: 1 },
    turning: { ty: -15, tz: 129, rx: -0.03, ry: -0.9, s: 1.03 },
    open: { ty: -15, tz: 129, rx: 0, ry: -Math.PI / 2, s: 1.05 },
  };

  for (const [name, move] of Object.entries(moves)) {
    it(`puts every face where the DOM Shelf's CSS puts it (${name})`, () => {
      const tape = mul4(tr(slot[0], slot[1], slot[2]), cssTape(move, tw, H, D));
      for (const face of ['spine', 'top', 'right', 'left'] as const) {
        const want = cssFace(face, tw, H, D).map((p) => at(tape, p));
        const p = place(F[face], slot, move, F.origin);
        // The renderer's own corner maths (scene3d.ts `corners`, X then Y then Z), with no perspective.
        const got = corners(p, { persp: 0, originX: 0, originY: 0 });
        want.forEach((w, k) => {
          expect(got[k]).not.toBeNull();
          expect(Math.abs((got[k]?.x ?? NaN) - w[0])).toBeLessThan(1e-6);
          expect(Math.abs((got[k]?.y ?? NaN) - w[1])).toBeLessThan(1e-6);
        });
        // And in depth: the face's corners through its rotation matrix.
        const c3 = [-1, 1].flatMap((sy) => [-1, 1].map((sx) => apply(p.m, [(sx * p.w) / 2, (sy * p.h) / 2, 0])));
        c3.forEach((d, k) => expect(Math.abs(p.z + d[2] - want[k][2])).toBeLessThan(1e-6));
      }
    });
  }
});

describe('transform: facing', () => {
  const F = tapeFaces(39, 294, 162);
  const eye = { x: 720, y: 135, persp: 1400 };
  const show = (slot: V3, face: 'spine' | 'top' | 'right' | 'left', move = REST) => facing(place(F[face], slot, move, F.origin), eye.x, eye.y, eye.persp);

  it('shows the side that faces the eye, as backface-visibility: hidden does', () => {
    // Left of the eye: its right side (the sleeve) shows, its left side doesn't.
    expect(show([100, 400, 0], 'right')).toBe(true);
    expect(show([100, 400, 0], 'left')).toBe(false);
    // Right of the eye: the other way round.
    expect(show([1200, 400, 0], 'right')).toBe(false);
    expect(show([1200, 400, 0], 'left')).toBe(true);
    // The spine always; the top only from above.
    expect(show([100, 400, 0], 'spine')).toBe(true);
    expect(show([100, 400, 0], 'top')).toBe(true);
    expect(show([100, 60, 0], 'top')).toBe(false);
  });

  it('turns the sleeve to the eye and the back cover away when a tape opens', () => {
    const open: TapeMove = { ty: -15, tz: 129, rx: 0, ry: -Math.PI / 2, s: 1.05 };
    expect(show([700, 130, 0], 'right', open)).toBe(true);
    expect(show([700, 130, 0], 'left', open)).toBe(false);
  });
});

describe('transform: splitAtDepth', () => {
  const D = 162;
  const H = 294;
  const F = tapeFaces(39, H, D);
  const zAt = (p: ReturnType<typeof place>, u: number, v: number) => p.z + (p.m[6] * u + p.m[7] * v) * p.s;

  it('keeps a tape at rest wholly inside the shelf, and an opened sleeve wholly out of it', () => {
    const rest = place(F.right, [100, 100, 0], REST, F.origin);
    expect(splitAtDepth(rest, D, H, 0.5)).toEqual({ front: null, back: [-D / 2, D / 2] });
    const open = place(F.right, [100, 100, 0], { ty: -15, tz: 129, rx: 0, ry: -Math.PI / 2, s: 1.05 }, F.origin);
    expect(splitAtDepth(open, D, H, 0)).toEqual({ front: [-D / 2, D / 2], back: null });
  });

  it('splits a half-pulled tape side where it leaves the shelf, over its whole height', () => {
    for (const move of [
      { ty: -15, tz: 60, rx: 0, ry: 0, s: 1 },
      { ty: -16, tz: 35, rx: (-5 * Math.PI) / 180, ry: 0, s: 1 },
    ]) {
      for (const face of ['right', 'left'] as const) {
        const p = place(F[face], [100, 100, 0], move, F.origin);
        const { front, back } = splitAtDepth(p, D, H, 0);
        expect(front).not.toBeNull();
        expect(back).not.toBeNull();
        if (!front || !back) continue;
        // Together they cover the face once.
        expect(Math.min(front[0], back[0])).toBeCloseTo(-D / 2);
        expect(Math.max(front[1], back[1])).toBeCloseTo(D / 2);
        expect(Math.abs(front[1] - front[0]) + Math.abs(back[1] - back[0])).toBeCloseTo(D);
        // The front part is in front of the shelf at its top and bottom alike.
        for (const u of [front[0], (front[0] + front[1]) / 2, front[1]]) {
          expect(zAt(p, u, -H / 2)).toBeGreaterThanOrEqual(-1e-6);
          expect(zAt(p, u, H / 2)).toBeGreaterThanOrEqual(-1e-6);
        }
      }
    }
  });
});

describe('transform: boxCorners', () => {
  it('puts the box where its faces are (the spine and the sleeve corners among its corners)', () => {
    const tw = 39;
    const H = 294;
    const D = 162;
    const F = tapeFaces(tw, H, D);
    const at: V3 = [120, 80, -1.5];
    const move: TapeMove = { ty: -15, tz: 129, rx: -0.03, ry: -0.9, s: 1 };
    const box = boxCorners(at, move, F.origin, tw, H, D);
    expect(box).toHaveLength(8);
    for (const face of ['spine', 'right'] as const) {
      const p = place(F[face], at, move, F.origin);
      for (const [sx, sy] of [
        [-1, -1],
        [1, -1],
        [-1, 1],
        [1, 1],
      ]) {
        const d = apply(p.m, [(sx * p.w) / 2, (sy * p.h) / 2, 0]);
        const c: V3 = [p.x + d[0], p.y + d[1], p.z + d[2]];
        expect(box.some((b) => close(b, c, 1e-6))).toBe(true);
      }
    }
  });
});

describe('peek: the turn to show a cover', () => {
  const sizes = [
    shelfMetrics({ vw: 1440, vh: 900, safeTop: 76, safeBottom: 88, mini: false }),
    shelfMetrics({ vw: 390, vh: 844, safeTop: 60, safeBottom: 90, mini: false }),
    shelfMetrics({ vw: 2560, vh: 1440, safeTop: 76, safeBottom: 88, mini: false }),
  ];

  it('never swings a tape into the tapes beside it, all the way out and round (pointed at or not, box sets too)', () => {
    for (const m of sizes) {
      for (const tw of [m.W, m.boxW]) {
        const d = { H: m.H, W: m.W, D: m.D, tw, gap: m.gap };
        for (let p = 0; p <= 1.08; p += 0.0025) {
          for (const lift of [0, 0.5, 1]) {
            const pose = tapePose(lift, p, d);
            expect(intrusion(tw, m.D, m.gap, -pose.ry, pose.tz)).toBe(0);
          }
        }
      }
    }
  });

  it('measures intrusion: a tape turned in its slot sweeps into its neighbours', () => {
    expect(intrusion(39, 162, 2, 0, 0)).toBe(0);
    expect(intrusion(39, 162, 2, 0.4, 0)).toBeGreaterThan(50);
    expect(intrusion(39, 162, 2, Math.PI / 2, 162 / 2 + 39 / 2)).toBeCloseTo(0, 6);
  });

  it('starts on the shelf, ends facing you, slides out before it turns', () => {
    const m = sizes[0];
    const d = { H: m.H, W: m.W, D: m.D, tw: m.W, gap: m.gap };
    const rest = tapePose(0, 0, d);
    for (const k of ['ty', 'tz', 'rx', 'ry'] as const) expect(Math.abs(rest[k])).toBe(0);
    expect(rest.s).toBe(1);
    const shown = tapePose(0, 1, d);
    expect(shown.ry).toBeCloseTo(-Math.PI / 2);
    expect(shown.tz).toBe(peekDepth(d));
    // Its sleeve faces the eye, its spine and back don't.
    const F = tapeFaces(m.W, m.H, m.D);
    const at: V3 = [700, 200, 0];
    expect(facing(place(F.right, at, shown, F.origin), 720, 135, m.persp)).toBe(true);
    expect(facing(place(F.left, at, shown, F.origin), 720, 135, m.persp)).toBe(false);
    // Nothing turns before it is most of the way out.
    expect(peekPath(TURN_FROM).turn).toBe(0);
    expect(peekPath(TURN_FROM).out).toBeGreaterThan(0.8);
    // Pointed at: the DOM Shelf's hover pose (lifted, a little out, tipped 5°).
    const lifted = tapePose(1, 0, d);
    expect(lifted.ty).toBeCloseTo(-m.H * 0.055);
    expect(lifted.tz).toBeCloseTo(m.W * 0.9);
    expect(lifted.rx).toBeCloseTo((-5 * Math.PI) / 180);
  });

  it('moves smoothly along the way: no jumps between nearby points of the turn', () => {
    const m = sizes[0];
    const d = { H: m.H, W: m.W, D: m.D, tw: m.W, gap: m.gap };
    let prev = tapePose(1, 0, d);
    for (let p = 0.002; p <= 1.05; p += 0.002) {
      const q = tapePose(1, p, d);
      expect(Math.abs(q.tz - prev.tz)).toBeLessThan(3);
      expect(Math.abs(q.ry - prev.ry)).toBeLessThan(0.03);
      expect(Math.abs(q.ty - prev.ty)).toBeLessThan(0.5);
      prev = { ...q };
    }
  });
});

describe('peek: springs', () => {
  it('settles on its target, a touch past it on the way (the cover), and stops', () => {
    const s = spring0();
    s.to = 1;
    let peak = 0;
    let t = 0;
    while (stepSpring(s, 1 / 120, PEEK_K, PEEK_C) && t < 5) {
      peak = Math.max(peak, s.x);
      t += 1 / 120;
    }
    expect(s.x).toBe(1);
    expect(s.v).toBe(0);
    expect(t).toBeLessThan(1.2);
    expect(peak).toBeGreaterThan(1);
    expect(peak).toBeLessThan(1.06);
  });

  it('turns back from where it is, at the speed it has (a retarget keeps its velocity)', () => {
    const s = spring0();
    s.to = 1;
    for (let k = 0; k < 12; k++) stepSpring(s, 1 / 120, LIFT_K, LIFT_C);
    const x = s.x;
    const v = s.v;
    expect(v).toBeGreaterThan(0);
    s.to = 0;
    expect(s.x).toBe(x);
    expect(s.v).toBe(v);
    stepSpring(s, 1 / 120, LIFT_K, LIFT_C);
    // Still moving on a little before it comes back: no jump, no stop.
    expect(s.x).toBeGreaterThan(x);
    expect(Math.abs(s.x - x)).toBeLessThan(v / 60);
  });

  it('copes with a slow frame (steps it in small pieces)', () => {
    const a = spring0();
    const b = spring0();
    a.to = 1;
    b.to = 1;
    stepSpring(a, 1 / 15, PEEK_K, PEEK_C);
    for (let k = 0; k < 8; k++) stepSpring(b, 1 / 120, PEEK_K, PEEK_C);
    expect(a.x).toBeCloseTo(b.x, 2);
  });
});

describe('peek: intent and neighbours', () => {
  it('turns a tape the pointer rests on, waits longer for one it is moving over, never for a sweep', () => {
    expect(peekWait(0, 0)).toBe(DWELL);
    expect(peekWait(DWELL, 0)).toBe(0);
    expect(peekWait(DWELL, 0.3)).toBeGreaterThan(0);
    expect(peekWait(DWELL + 200, 0.3)).toBe(0);
    expect(peekWait(10_000, FAST * 1.5)).toBe(Number.POSITIVE_INFINITY);
  });

  it('eases neighbours aside without ever closing a gap past nothing', () => {
    const gaps = [2, 2, 2, 2, 2];
    const offs = asideFalloff(6, gaps);
    expect(offs[0]).toBe(6);
    for (let k = 1; k < offs.length; k++) expect(offs[k - 1] - offs[k]).toBeLessThanOrEqual(gaps[k - 1] + 1e-9);
    expect(offs).toEqual([6, 4, 2]);
    // A section's wider gap takes up the rest: nothing beyond it moves.
    expect(asideFalloff(6, [2, 15, 2])).toEqual([6, 4]);
    expect(asideFalloff(6, [])).toEqual([6]);
    expect(asideFalloff(0, [2, 2])).toEqual([]);
  });
});

describe('geometry: shelfMetrics and shelfLayout', () => {
  it('sizes a tape like a VHS box for the screen', () => {
    const m = shelfMetrics({ vw: 1440, vh: 900, safeTop: 76, safeBottom: 88, mini: false });
    expect(m.rows).toBe(2);
    expect(m.W).toBe(Math.round((m.H * 25) / 187));
    expect(m.D).toBe(Math.round((m.H * 103) / 187));
    expect(m.persp).toBe(1400);
    const phone = shelfMetrics({ vw: 390, vh: 844, safeTop: 60, safeBottom: 90, mini: false });
    expect(phone.phone).toBe(true);
    expect(phone.persp).toBe(1100);
    expect(phone.H).toBeLessThanOrEqual(250);
    const mini = shelfMetrics({ vw: 1440, vh: 900, safeTop: 0, safeBottom: 0, mini: true });
    expect(mini.rows).toBe(1);
    expect(mini.persp).toBe(760);
  });

  it('centres a short shelf and scrolls a long one from the left padding', () => {
    const m = shelfMetrics({ vw: 1440, vh: 900, safeTop: 76, safeBottom: 88, mini: false });
    const plan = (n: number) => planShelf({ widths: new Array<number>(n).fill(m.W), groups: new Array<string>(n).fill(''), rows: m.rows, fullW: m.fullW, bayW: m.bayW, W: m.W, gap: m.gap, secGap: m.secGap });
    const short = plan(8);
    const s = shelfLayout(m, short);
    expect(s.x0).toBeCloseTo((m.vw - short.bayLen[0]) / 2);
    expect(s.maxX).toBe(0);
    const long = plan(10_000);
    const l = shelfLayout(m, long);
    expect(l.x0).toBe(m.padX);
    expect(l.stride).toBe(m.bayW + m.bayGap);
    // Scrolled to the end, the last bay ends one padding from the right edge.
    expect(l.x0 + (long.bays - 1) * l.stride + long.bayLen[long.bays - 1] - l.maxX).toBeCloseTo(m.vw - m.padX);
    expect(l.pitch).toBe(m.H + m.head + m.bh);
  });
});

describe('spine painting keys', () => {
  const item = (o: Partial<ItemSummary> = {}): ItemSummary => ({
    id: 42,
    kind: 'movie',
    title: 'The General',
    parent_title: null,
    year: 1926,
    subtitle: null,
    poster: null,
    backdrop: null,
    progress: null,
    badges: ['4K'],
    child_count: null,
    unwatched_count: null,
    available: true,
    added_at: 0,
    ...o,
  });
  const look = spineLook(item(), fallbackPalette('x'));
  const spec = { tw: 30, H: 220, W: 30 };

  it('paints a spine again only when what it shows changes', () => {
    const k = spineKey(item(), look, spec, 2);
    expect(spineKey(item(), look, spec, 2)).toBe(k);
    expect(spineKey(item({ added_at: 1 }), look, spec, 2)).toBe(k);
    expect(spineKey(item({ title: 'Sherlock Jr.' }), look, spec, 2)).not.toBe(k);
    expect(spineKey(item({ available: false }), look, spec, 2)).not.toBe(k);
    expect(spineKey(item(), look, { ...spec, H: 221 }, 2)).not.toBe(k);
    expect(spineKey(item(), look, spec, 1.5)).not.toBe(k);
    expect(spineKey(item(), look, spec, 2, 1)).not.toBe(k);
  });

  it('writes the back cover as the DOM Shelf does', () => {
    expect(backLines(item())).toEqual({ title: 'The General', line: 'flux video', meta: '1926 · FEATURE · 4K' });
    expect(backLines(item({ kind: 'show', parent_title: 'x', year: null, badges: [] }))).toEqual({ title: 'The General', line: 'flux video', meta: 'SERIES' });
    expect(backLines(item({ kind: 'episode', parent_title: 'Columbo', badges: [] })).line).toBe('Columbo');
    expect(backKey(item(), look, 120, 220, 2)).not.toBe(backKey(item({ year: 1927 }), look, 120, 220, 2));
  });

  it('greys and darkens colours the way the CSS filters and color-mix do', () => {
    expect(grayHex('#ff0000', false)).toBe('#ff0000');
    // grayscale(0.9): the filter's matrix, e.g. R' = 0.29134 R + 0.64368 G + 0.06498 B.
    expect(grayHex('#ff0000')).toBe('#4a3131');
    expect(grayHex('#808080')).toBe('#808080');
    // color-mix(in oklab, c 55%, #000): linear light × 0.55³; black and white stay put at the ends.
    expect(mixBlackOklab('#000000', 0.55)).toBe('#000000');
    expect(mixBlackOklab('#ffffff', 1)).toBe('#ffffff');
    expect(mixBlackOklab('#ffffff', 0.55)).toBe('#717171');
  });
});
