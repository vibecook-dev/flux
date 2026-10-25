import { describe, expect, it } from 'vitest';
import { bezier, lerpRect, soft, spring, Tween } from './motion';
import { classFor, CLASSES } from './posters';
import { QUAD_FLOATS, QuadBatch } from './quads';
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
