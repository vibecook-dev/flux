/** The GPU Gallery's placement maths: walls as the DOM hangs them, decal gaps, text resolution. */
import { describe, expect, it } from 'vitest';
import type { ItemSummary } from '../../api/types';
import { depthMargin } from '../gpu/room3d';
import { FRAME, MAT, MAT_B, PW, POSTER_H, WIDE_W, W, pieceLayout } from './core';
import { FARTHEST, MIN_GAP, NEAR, OFF, textLod, wallQuad } from './gpu-gallery';

const item = (o: Partial<ItemSummary> = {}): ItemSummary => ({
  id: 1,
  kind: 'movie',
  title: 'Arrival',
  parent_title: null,
  year: 2016,
  subtitle: null,
  poster: null,
  backdrop: null,
  progress: null,
  badges: [],
  child_count: null,
  unwatched_count: null,
  available: true,
  added_at: 0,
  ...o,
});

describe('wall placement', () => {
  it('runs along the corridor as CSS wallT does: away from you on the left, toward you on the right', () => {
    // A 10 px rect at the start of a 780 px plane that begins at d = 1000.
    const left = wallQuad(-1, 1000, PW, 0, 0, 10, 10, 2);
    const right = wallQuad(1, 1000, PW, 0, 0, 10, 10, 2);
    expect([left.x, left.z, left.ry]).toEqual([-W / 2 + 2, -1005, Math.PI / 2]);
    expect([right.x, right.z, right.ry]).toEqual([W / 2 - 2, -(1000 + PW - 5), -Math.PI / 2]);
  });
});

describe('z-fighting', () => {
  it('keeps every layer far enough in front of its wall for float32 depth at the end of the corridor', () => {
    expect(MIN_GAP).toBeGreaterThan(0.5);
    for (const gap of Object.values(OFF)) expect(gap).toBeGreaterThanOrEqual(MIN_GAP);
    expect(depthMargin(FARTHEST, MIN_GAP, NEAR)).toBeGreaterThan(64);
  });
});

describe('text resolution', () => {
  it('steps in √2 and keeps its step while it still fits', () => {
    expect(textLod(1)).toBe(1);
    expect(textLod(1.2)).toBe(1.41);
    expect(textLod(9)).toBe(4);
    expect(textLod(0.1)).toBe(0.35);
    // Walking a little closer or further keeps the raster: no redraw at every step.
    expect(textLod(1.5, 1.41)).toBe(1.41);
    expect(textLod(0.9, 1.41)).toBe(1.41);
    expect(textLod(1.7, 1.41)).toBe(2);
    expect(textLod(0.7, 1.41)).toBe(0.71);
  });
});

describe('pieceLayout', () => {
  it('frames a poster by its height and a wide still by its width, mat weighted at the bottom', () => {
    const p = pieceLayout(item({ poster: { url: '/a', width: 2000, height: 3000, blurhash: null, source: 'provider', attribution: 'TMDB', generator: null } }), false);
    expect(p.wide).toBe(false);
    expect(p.ph).toBe(POSTER_H);
    expect(p.fw).toBe(p.pw + 2 * MAT + 2 * FRAME);
    expect(p.fh).toBe(p.ph + MAT + MAT_B + 2 * FRAME);
    expect(p.lx).toBe(p.fl + p.fw + 40);
    const wide = pieceLayout(item({ kind: 'episode' }), true);
    expect(wide.wide).toBe(true);
    expect(wide.pw).toBe(WIDE_W);
    // On a phone the label goes under the frame.
    expect([wide.lx, wide.ly]).toEqual([wide.fl, wide.ft + wide.fh + 28]);
  });
});
