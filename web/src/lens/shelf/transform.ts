/**
 * Rotations for the GPU Shelf. A tape's faces are hinged boxes (CSS
 * `rotateY(±90deg)` sides, a `rotateX(90deg)` top) inside a tape that itself
 * slides, tips and turns. The renderer (gpu/scene3d.ts) takes one rotation per
 * quad as angles applied X, then Y, then Z, about the quad's centre, so the two
 * rotations are composed here and turned back into those angles. Pure.
 *
 * Conventions match scene3d.ts (and CSS): x right, y down, z towards the viewer;
 * rotX(a): y' = cos·y − sin·z, z' = sin·y + cos·z; rotY(a): x' = cos·x + sin·z,
 * z' = −sin·x + cos·z; rotZ(a): x' = cos·x − sin·y, y' = sin·x + cos·y.
 */

/** A 3×3 matrix, row-major: [m00, m01, m02, m10, …, m22]. */
export type M3 = [number, number, number, number, number, number, number, number, number];
export type V3 = [number, number, number];

export const I3: M3 = [1, 0, 0, 0, 1, 0, 0, 0, 1];

export function rotX(a: number): M3 {
  const c = Math.cos(a);
  const s = Math.sin(a);
  return [1, 0, 0, 0, c, -s, 0, s, c];
}

export function rotY(a: number): M3 {
  const c = Math.cos(a);
  const s = Math.sin(a);
  return [c, 0, s, 0, 1, 0, -s, 0, c];
}

export function rotZ(a: number): M3 {
  const c = Math.cos(a);
  const s = Math.sin(a);
  return [c, -s, 0, s, c, 0, 0, 0, 1];
}

export function mul(a: M3, b: M3): M3 {
  return [
    a[0] * b[0] + a[1] * b[3] + a[2] * b[6],
    a[0] * b[1] + a[1] * b[4] + a[2] * b[7],
    a[0] * b[2] + a[1] * b[5] + a[2] * b[8],
    a[3] * b[0] + a[4] * b[3] + a[5] * b[6],
    a[3] * b[1] + a[4] * b[4] + a[5] * b[7],
    a[3] * b[2] + a[4] * b[5] + a[5] * b[8],
    a[6] * b[0] + a[7] * b[3] + a[8] * b[6],
    a[6] * b[1] + a[7] * b[4] + a[8] * b[7],
    a[6] * b[2] + a[7] * b[5] + a[8] * b[8],
  ];
}

export function apply(m: M3, v: V3): V3 {
  return [m[0] * v[0] + m[1] * v[1] + m[2] * v[2], m[3] * v[0] + m[4] * v[1] + m[5] * v[2], m[6] * v[0] + m[7] * v[1] + m[8] * v[2]];
}

/**
 * The angles (rx, ry, rz) for which rotZ(rz)·rotY(ry)·rotX(rx) = m: what the
 * renderer applies (X first). At ry = ±90° (a side face) X and Z turn about the
 * same axis; the turn is then put all in Z.
 */
export function euler(m: M3): V3 {
  const sy = Math.max(-1, Math.min(1, -m[6]));
  const ry = Math.asin(sy);
  if (Math.abs(sy) < 0.999999) return [Math.atan2(m[7], m[8]), ry, Math.atan2(m[3], m[0])];
  // Gimbal lock: rx and rz both spin about the same axis; take rx = 0.
  return [0, ry, Math.atan2(-m[1], m[4])];
}

/** How a tape is moved off the shelf: CSS `translate3d(0, ty, tz) rotateY(ry) rotateX(rx) scale(s)`. */
export interface TapeMove {
  ty: number;
  tz: number;
  rx: number;
  ry: number;
  s: number;
}

export const REST: TapeMove = { ty: 0, tz: 0, rx: 0, ry: 0, s: 1 };

/** A face of a tape, in the tape's own space (x right from its left edge, y down from its top, z out of the spine). */
export interface Face {
  /** Centre. */
  c: V3;
  w: number;
  h: number;
  /** Its own hinge (sides: rotY(±90°), top: rotX(90°)). */
  r: M3;
}

/** Where a face ends up: its centre in the world, its size, and the angles for the renderer. */
export interface Placed {
  x: number;
  y: number;
  z: number;
  w: number;
  h: number;
  rx: number;
  ry: number;
  rz: number;
  /** The face's full rotation (for placing things on it). */
  m: M3;
  s: number;
}

/**
 * A face of a tape sitting at `at` (its slot's top-left corner, on the spine plane),
 * moved by `move` about `origin` (CSS transform-origin, in the tape's space).
 */
export function place(face: Face, at: V3, move: TapeMove, origin: V3): Placed {
  const r = mul(rotY(move.ry), rotX(move.rx));
  const rel: V3 = [(face.c[0] - origin[0]) * move.s, (face.c[1] - origin[1]) * move.s, (face.c[2] - origin[2]) * move.s];
  const d = apply(r, rel);
  const m = mul(r, face.r);
  const [rx, ry, rz] = euler(m);
  return {
    x: at[0] + origin[0] + d[0],
    y: at[1] + origin[1] + move.ty + d[1],
    z: at[2] + origin[2] + move.tz + d[2],
    w: face.w * move.s,
    h: face.h * move.s,
    rx,
    ry,
    rz,
    m,
    s: move.s,
  };
}

/** A point on a placed face, `u` right and `v` down from its centre (face units), in the world. */
export function onFace(p: Placed, u: number, v: number): V3 {
  const d = apply(p.m, [u * p.s, v * p.s, 0]);
  return [p.x + d[0], p.y + d[1], p.z + d[2]];
}

/** The way a placed face points (its outward normal). */
export function normal(p: Placed): V3 {
  return [p.m[2], p.m[5], p.m[8]];
}

/**
 * Whether a face is turned towards a CSS perspective camera at (ox, oy) with
 * perspective `persp` (backface-visibility: hidden culls the rest). The eye sits
 * at (ox, oy, persp) in the world.
 */
export function facing(p: Placed, ox: number, oy: number, persp: number): boolean {
  const n = normal(p);
  const toEye: V3 = [ox - p.x, oy - p.y, persp - p.z];
  return n[0] * toEye[0] + n[1] * toEye[1] + n[2] * toEye[2] > 1e-6;
}

/**
 * A tape's four visible faces at rest, in its own space (CSS: the spine, the top
 * hinged back with `rotateX(90deg)`, the sleeve on its right and the back cover on
 * its left, both `rotateY(±90deg)`), and the point it moves about
 * (`transform-origin: 50% 100% calc(var(--d) * -0.5)`).
 */
export interface TapeFaces {
  spine: Face;
  top: Face;
  right: Face;
  left: Face;
  origin: V3;
}

export function tapeFaces(tw: number, H: number, D: number): TapeFaces {
  return {
    spine: { c: [tw / 2, H / 2, 0], w: tw, h: H, r: I3 },
    top: { c: [tw / 2, 0, -D / 2], w: tw, h: D, r: rotX(Math.PI / 2) },
    right: { c: [tw, H / 2, -D / 2], w: D, h: H, r: rotY(Math.PI / 2) },
    left: { c: [0, H / 2, -D / 2], w: D, h: H, r: rotY(-Math.PI / 2) },
    origin: [tw / 2, H, -D / 2],
  };
}

/**
 * The eight corners of a tape's box (tw × H × D, its spine at z = 0 of its slot) in
 * the world: the slot's top-left `at`, moved by `move` about `origin` (as `place`).
 */
export function boxCorners(at: V3, move: TapeMove, origin: V3, tw: number, H: number, D: number): V3[] {
  const r = mul(rotY(move.ry), rotX(move.rx));
  const out: V3[] = [];
  for (const x of [0, tw]) {
    for (const y of [0, H]) {
      for (const z of [-D, 0]) {
        const d = apply(r, [(x - origin[0]) * move.s, (y - origin[1]) * move.s, (z - origin[2]) * move.s]);
        out.push([at[0] + origin[0] + d[0], at[1] + origin[1] + move.ty + d[1], at[2] + origin[2] + move.tz + d[2]]);
      }
    }
  }
  return out;
}

/**
 * Splits a placed face (unscaled size fw×fh) at the plane z = z0 across its width:
 * `front` is the span [ua, ub] (face units from its centre) that is in front of the
 * plane over the face's whole height, `back` the rest. The renderer has no depth
 * buffer, so a tape pulled half out of the shelf draws its sides in two parts: the
 * part inside the shelf before the tapes beside it, the part out in front after them.
 */
export function splitAtDepth(p: Placed, fw: number, fh: number, z0: number): { front: [number, number] | null; back: [number, number] | null } {
  const half = fw / 2;
  const ez = p.m[6] * p.s;
  const k = z0 - p.z + Math.abs(p.m[7]) * p.s * (fh / 2);
  let a = -half;
  let b = half;
  if (Math.abs(ez) < 1e-6) {
    if (k > 0) return { front: null, back: [-half, half] };
    return { front: [-half, half], back: null };
  }
  const u = k / ez;
  if (ez > 0) a = Math.max(a, u);
  else b = Math.min(b, u);
  if (b - a <= 1e-6) return { front: null, back: [-half, half] };
  if (a <= -half + 1e-6 && b >= half - 1e-6) return { front: [-half, half], back: null };
  return { front: [a, b], back: ez > 0 ? [-half, a] : [b, half] };
}
