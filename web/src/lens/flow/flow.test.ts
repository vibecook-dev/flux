import { describe, expect, it } from 'vitest';
import type { ItemSummary } from '../../api/types';
import { coverPose, edgeOpacity, shadeOf } from './core';
import { ANGLE, measure, metaLine, pose, titleLine, windowAround } from './flow';

const M = { gap1: 200, spacing: 40, depth: 180, lift: 12 };

function item(over: Partial<ItemSummary>): ItemSummary {
  return {
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
    ...over,
  };
}

describe('pose', () => {
  it('faces you at the centre, a little forward', () => {
    const p = pose(0, M);
    expect(p.x).toBe(0);
    expect(p.ry).toBeCloseTo(0);
    expect(p.z).toBe(M.lift);
  });

  it('turns the stacks away and recedes them', () => {
    expect(pose(1, M)).toEqual({ x: 200, z: -180, ry: -ANGLE });
    expect(pose(-1, M)).toEqual({ x: -200, z: -180, ry: ANGLE });
    expect(pose(3.5, M).x).toBe(200 + 2.5 * 40);
    expect(pose(-3.5, M).x).toBe(-(200 + 2.5 * 40));
  });

  it('moves continuously through the turn (no jump at one place away)', () => {
    const near = pose(0.999, M);
    const far = pose(1.001, M);
    expect(Math.abs(near.x - far.x)).toBeLessThan(1);
    expect(Math.abs(near.ry - far.ry)).toBeLessThan(0.5);
    expect(Math.abs(near.z - far.z)).toBeLessThan(1);
  });

  it('swings away fast as it leaves the centre', () => {
    // ease(t) = t(2 − t): a quarter of the way is already well over a quarter of the turn.
    expect(Math.abs(pose(0.25, M).ry)).toBeGreaterThan(ANGLE * 0.4);
  });
});

describe('windowAround', () => {
  it('draws only the covers near the position', () => {
    expect(windowAround(6000.4, 13298, 12)).toEqual([5988, 6013]);
  });

  it('clips at the ends of the library', () => {
    expect(windowAround(0, 13298, 12)).toEqual([0, 12]);
    expect(windowAround(13297, 13298, 12)).toEqual([13285, 13297]);
    expect(windowAround(2, 5, 12)).toEqual([0, 4]);
  });

  it('is empty for an empty list', () => {
    const [lo, hi] = windowAround(0, 0, 12);
    expect(hi).toBeLessThan(lo);
  });
});

describe('the lines under the poster', () => {
  it('says the year and the first badges', () => {
    expect(metaLine(item({ badges: ['4K', 'HDR10', 'Atmos'] }))).toBe('2016 · 4K · HDR10');
  });

  it('counts a show’s seasons', () => {
    expect(metaLine(item({ kind: 'show', title: 'Severance', year: 2022, child_count: 2 }))).toBe('2022 · 2 seasons');
    expect(metaLine(item({ kind: 'show', title: 'Chernobyl', year: 2019, child_count: 1 }))).toBe('2019 · 1 season');
  });

  it('names an episode by its show and code', () => {
    const ep = item({ kind: 'episode', title: 'Good News About Hell', parent_title: 'Severance', subtitle: 'S01E01' });
    expect(titleLine(ep)).toBe('Severance: Good News About Hell');
    expect(metaLine(ep)).toBe('S01E01');
  });

  it('says when a title is offline', () => {
    expect(metaLine(item({ available: false }))).toBe('2016 · offline');
  });
});

describe('measure', () => {
  const safe = { top: 68, bottom: 84 };

  it('fits a 2:3 poster on a desktop window with room for its title', () => {
    const m = measure(1440, 900, safe, false);
    expect(m.cw / m.ch).toBeCloseTo(2 / 3);
    expect(m.ch).toBeLessThanOrEqual(500);
    // Poster, reflection and title stay between the island and the lens bar.
    expect(m.floor - m.ch).toBeGreaterThanOrEqual(safe.top);
    expect(m.floor + m.reflGap + 60).toBeLessThanOrEqual(900 - safe.bottom + 1);
  });

  it('draws enough covers to reach the edges, and no more than fourteen a side', () => {
    const wide = measure(2560, 1440, safe, false);
    const phone = measure(390, 844, safe, true);
    expect(wide.R).toBeLessThanOrEqual(14);
    expect(phone.R).toBeGreaterThanOrEqual(4);
    // The stacks reach past the edge of the window.
    expect(wide.gap1 + (wide.R - 1) * wide.spacing).toBeGreaterThan(wide.W / 2 - wide.cw);
  });
});

describe('coverPose', () => {
  const m = measure(1440, 900, { top: 68, bottom: 84 }, false);
  const still = { x: 0, y: 0, tx: 0, ty: 0 };
  const lean = { x: 3, y: -4, tx: 3, ty: -4 };

  it('stands each cover where pose() puts it, from its visual index', () => {
    const p = coverPose({ id: 1, vi: 7.5, vv: 0 }, 6, m, still, false);
    const q = pose(1.5, m);
    expect(p).toMatchObject({ d: 1.5, ad: 1.5, x: q.x, z: q.z, ry: q.ry, rx: 0 });
  });

  it('leans only the poster in front toward the pointer, and not while dragging', () => {
    expect(coverPose({ id: 1, vi: 6, vv: 0 }, 6, m, lean, false)).toMatchObject({ rx: 3, ry: -4 });
    expect(coverPose({ id: 1, vi: 6.25, vv: 0 }, 6, m, lean, false).rx).toBeCloseTo(1.5);
    expect(coverPose({ id: 1, vi: 7, vv: 0 }, 6, m, lean, false).rx).toBe(0);
    expect(coverPose({ id: 1, vi: 6, vv: 0 }, 6, m, lean, true)).toMatchObject({ rx: 0, ry: pose(0, m).ry });
  });
});

describe('edge fade and haze', () => {
  it('fades covers out only at the far ends of the stacks', () => {
    expect(edgeOpacity(0, 12)).toBe(1);
    expect(edgeOpacity(10.4, 12)).toBe(1);
    expect(edgeOpacity(11, 12)).toBeCloseTo(0.5);
    expect(edgeOpacity(11.5, 12)).toBe(0);
  });

  it('sinks the stacks into the room, up to a limit', () => {
    expect(shadeOf(0)).toBe(0);
    expect(shadeOf(1)).toBe(0.36);
    expect(shadeOf(5)).toBe(0.62);
  });
});
