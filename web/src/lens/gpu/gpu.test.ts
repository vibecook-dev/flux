import { describe, expect, it } from 'vitest';
import { bezier, lerpRect, soft, spring, Tween } from './motion';
import { classFor, CLASSES } from './posters';
import { QUAD_FLOATS, QuadBatch } from './quads';
import { corners, inside, MAT3D, project, QUAD3D_FLOATS, Quad3DBatch, SCENE3D_WGSL } from './scene3d';
import { QUAD_WGSL } from './shaders';
import { Slots } from './slots';
import { ellipsize, wrap } from './text';

describe('Slots', () => {
  it('fills free slots first, then evicts the least recently drawn', () => {
    const s = new Slots(2);
    expect(s.claim('a', 1)).toEqual({ slot: 0, evicted: null });
    expect(s.claim('b', 2)).toEqual({ slot: 1, evicted: null });
    s.touch(0, 3);
    // b was drawn longest ago (frame 2), so it goes.
    expect(s.claim('c', 4)).toEqual({ slot: 1, evicted: 'b' });
    expect(s.find('b')).toBe(-1);
    expect(s.find('c')).toBe(1);
  });

  it('never evicts a poster drawn in this frame', () => {
    const s = new Slots(2);
    s.claim('a', 5);
    s.claim('b', 5);
    expect(s.claim('c', 5)).toEqual({ slot: -1, evicted: null });
    s.grow(3);
    expect(s.claim('c', 5)).toEqual({ slot: 2, evicted: null });
  });

  it('protects the last few frames when asked (lenses sharing the textures)', () => {
    const s = new Slots(2);
    s.claim('a', 1);
    s.claim('b', 3);
    expect(s.claim('c', 4, 2)).toEqual({ slot: 0, evicted: 'a' });
    expect(s.claim('d', 4, 2)).toEqual({ slot: -1, evicted: null });
  });

  it('reuses a released slot', () => {
    const s = new Slots(2);
    s.claim('a', 1);
    s.claim('b', 1);
    s.release('a');
    expect(s.claim('c', 2)).toEqual({ slot: 0, evicted: null });
    expect(s.size).toBe(2);
  });
});

describe('motion', () => {
  it('evaluates CSS cubic-bezier curves', () => {
    const linear = bezier(0, 0, 1, 1);
    expect(linear(0.3)).toBeCloseTo(0.3, 4);
    const ease = bezier(0.25, 0.1, 0.25, 1);
    // Reference values for CSS `ease`.
    expect(ease(0.5)).toBeCloseTo(0.8024, 3);
    expect(soft(0)).toBe(0);
    expect(soft(1)).toBe(1);
    expect(soft(0.5)).toBeGreaterThan(0.85);
  });

  it('springs past the target and settles on it', () => {
    expect(spring(0)).toBe(0);
    expect(Math.max(...Array.from({ length: 50 }, (_, i) => spring(i / 49)))).toBeGreaterThan(1.04);
    expect(spring(1)).toBe(1);
  });

  it('retargets a tween from where it is, so motion stays continuous', () => {
    const t = new Tween((p) => p, 0);
    t.set(1, 0, 1);
    expect(t.value(0.5)).toBeCloseTo(0.5);
    t.set(0, 0.5, 1);
    expect(t.value(0.5)).toBeCloseTo(0.5);
    expect(t.value(1)).toBeCloseTo(0.25);
    expect(t.done(1.5)).toBe(true);
    expect(t.value(2)).toBe(0);
  });

  it('interpolates rectangles', () => {
    expect(lerpRect({ x: 0, y: 0, w: 10, h: 20 }, { x: 10, y: 10, w: 30, h: 60 }, 0.5)).toEqual({ x: 5, y: 5, w: 20, h: 40 });
  });
});

describe('text', () => {
  // One unit per character: easy to reason about.
  const measure = (s: string) => s.length;

  it('ellipsises to the width, by whole code points', () => {
    expect(ellipsize('Heat', 10, measure)).toBe('Heat');
    expect(ellipsize('The Godfather Part II', 10, measure)).toBe('The Godfa…');
    expect(ellipsize('Amélie 🎬🎬🎬', 8, measure)).toBe('Amélie…');
  });

  it('wraps words into lines and ellipsises the last', () => {
    expect(wrap('The Lord of the Rings', 10, 3, measure)).toEqual(['The Lord', 'of the', 'Rings']);
    expect(wrap('The Lord of the Rings: The Return of the King', 10, 2, measure)).toEqual(['The Lord', 'of the Ri…']);
    expect(wrap('Supercalifragilistic', 8, 2, measure)).toEqual(['Superca…']);
  });
});

describe('posters', () => {
  it('picks the smallest texture class that is sharp at the size drawn', () => {
    // Overview, Grid, Posters and Details tiles on a 2× display.
    expect(CLASSES[classFor(62 * 2)].w).toBe(128);
    expect(CLASSES[classFor(112 * 2)].w).toBe(256);
    expect(CLASSES[classFor(180 * 2)].w).toBe(384);
    expect(CLASSES[classFor(300 * 2)].w).toBe(640);
    expect(CLASSES[classFor(5000)].w).toBe(640);
    for (const c of CLASSES) {
      expect(c.h / c.w).toBe(1.5);
      expect(c.w >> (c.mips - 1)).toBeGreaterThanOrEqual(16);
    }
  });
});

describe('quads', () => {
  it('lays a quad out as the shader reads it (six vec4s)', () => {
    expect(QUAD_FLOATS).toBe(24);
    const fields = /struct Quad \{([^}]*)\}/.exec(QUAD_WGSL)?.[1] ?? '';
    expect(fields.match(/: vec4f/g)).toHaveLength(6);
    const b = new QuadBatch(1);
    const o = b.add(1, 2, 3, 4, 5, 6);
    b.color(o, [0.1, 0.2, 0.3, 1], 0.5);
    b.texture(o, 4097, 1.5, 128);
    b.flags(o, 3);
    b.add(0, 0, 1, 1, 0);
    expect(b.n).toBe(2);
    expect(b.data.length).toBeGreaterThanOrEqual(2 * QUAD_FLOATS);
    const d = Array.from(b.data.slice(0, QUAD_FLOATS)).map((v) => Math.round(v * 1000) / 1000);
    // rect, start (unset), colour (alpha × 0.5), uv, radius/kind/texture/opacity, ready/–/extra/flags
    expect(d).toEqual([1, 2, 3, 4, 0, 0, 0, 0, 0.1, 0.2, 0.3, 0.5, 0, 0, 0, 0, 6, 5, 4097, 1, 1.5, 0, 128, 3]);
  });
});

describe('scene3d', () => {
  const cam = { persp: 900, originX: 400, originY: 230 };

  it('projects like CSS perspective', () => {
    const far = project(400, 230, -500, cam);
    expect(far?.x).toBe(400);
    expect(far?.y).toBe(230);
    expect(far?.scale).toBeCloseTo(900 / 1400, 10);
    const p = project(500, 230, 300, cam);
    expect(p?.x).toBeCloseTo(400 + 100 * 1.5);
    expect(p?.scale).toBeCloseTo(1.5);
    expect(project(0, 0, 900, cam)).toBeNull();
    expect(project(10, 20, 50, { persp: 0, originX: 0, originY: 0 })).toEqual({ x: 10, y: 20, scale: 1 });
  });

  it('places a rotated quad where CSS does (rotateY(35deg) rotateX(10deg), measured in Chrome)', () => {
    const c = corners({ x: 300, y: 250, z: -60, w: 200, h: 300, rx: (10 * Math.PI) / 180, ry: (35 * Math.PI) / 180 }, cam);
    const xs = c.map((p) => p?.x ?? NaN);
    const ys = c.map((p) => p?.y ?? NaN);
    expect(Math.min(...xs)).toBeCloseTo(208.3, 0);
    expect(Math.min(...ys)).toBeCloseTo(105.6, 0);
    expect(Math.max(...xs)).toBeCloseTo(397.2, 0);
    expect(Math.max(...ys)).toBeCloseTo(401.3, 0);
    expect(inside(300, 250, c)).toBe(true);
    expect(inside(20, 20, c)).toBe(false);
  });

  it('lays a quad out as the shader reads it (eight vec4s)', () => {
    expect(QUAD3D_FLOATS).toBe(32);
    const fields = /struct Q \{([^}]*)\}/.exec(SCENE3D_WGSL)?.[1] ?? '';
    expect(fields.match(/: vec4f/g)).toHaveLength(8);
    const b = new Quad3DBatch(1);
    const o = b.add(1, 2, 3, 4, 5, 6, 7);
    b.rotate(o, 0.1, 0.2, 0.3);
    b.mirror(o, 400, 0.5);
    b.add(0, 0, 0, 1, 1, 0);
    const d = Array.from(b.data.slice(0, QUAD3D_FLOATS)).map((v) => Math.round(v * 1000) / 1000);
    expect(d.slice(0, 11)).toEqual([1, 2, 3, 0, 4, 5, 7, 6, 0.1, 0.2, 0.3]);
    // texture none (ready-at -1), opacity 1, mirror flag 16 with its line and strength
    expect(d.slice(24, 31)).toEqual([0, 0, -1, 1, 16, 400, 0.5]);
  });

  it('carries a material in the quad’s spare floats, leaving everything else as it was', () => {
    const b = new Quad3DBatch(1);
    const o = b.add(1, 2, 3, 4, 5, 6, 7);
    b.rotate(o, 0.1, 0.2, 0.3);
    b.origin(o, 8, 9, 10);
    const before = Array.from(b.data.slice(0, QUAD3D_FLOATS));
    // No material: the spare floats are zero (Flow's and the Gallery's quads draw as before).
    expect([before[3], before[11], before[15]]).toEqual([0, 0, 0]);
    b.material(o, MAT3D.spine, 0.5, 0.25, 0.75);
    const after = Array.from(b.data.slice(0, QUAD3D_FLOATS));
    expect(after[3]).toBeCloseTo(MAT3D.spine + 0.5);
    expect(after[11]).toBe(0.25);
    expect(after[15]).toBe(0.75);
    after.forEach((v, k) => {
      if (k !== 3 && k !== 11 && k !== 15) expect(v).toBe(before[k]);
    });
    // Gloss stays below the next material's number.
    b.material(o, MAT3D.print, 1);
    expect(Math.floor(b.data[3])).toBe(MAT3D.print);
    // The shader reads them where they are, and the camera's lights after its first 32 bytes.
    expect(SCENE3D_WGSL).toMatch(/let mat = u32\(q\.pos\.w\)/);
    expect(SCENE3D_WGSL).toMatch(/let seed = q\.rot\.w/);
    expect(SCENE3D_WGSL).toMatch(/let shade = q\.origin\.w/);
    const cam = /struct Cam \{([^}]*)\}/.exec(SCENE3D_WGSL)?.[1] ?? '';
    expect(cam.match(/: vec4f/g)).toHaveLength(3);
  });
});
