import { describe, expect, it } from 'vitest';
import { layoutWall, rowAt } from './layout';

const base = { inner: 1000, padL: 20, w: 180, gap: 16, cap: 0, head: 64, top: 80, bottom: 100 };

describe('layoutWall', () => {
  it('fits columns to the width and sizes 2:3 tiles', () => {
    const l = layoutWall({ ...base, count: 0, total: 0, sectionAt: () => '' });
    expect(l.cols).toBe(5);
    expect(l.tileW).toBeCloseTo((1000 - 16 * 4) / 5);
    expect(l.tileH).toBeCloseTo(l.tileW * 1.5);
  });

  it('starts every section on a new row under its header', () => {
    const sections = ['1990s', '1990s', '1990s', '2000s', '2000s', '2000s', '2000s', '2000s', '2000s', '2000s'];
    const l = layoutWall({ ...base, count: sections.length, total: sections.length, sectionAt: (i) => sections[i] });
    expect(l.rows.map((r) => r.kind)).toEqual(['head', 'items', 'head', 'items', 'items']);
    expect(l.rows[0]).toMatchObject({ section: '1990s', count: 3, y: 80, h: 64 });
    // The 2000s start a fresh row, at the left margin.
    expect(l.px[3]).toBe(20);
    expect(l.py[3]).toBeGreaterThan(l.py[2]);
    expect(l.rowOf[9]).toBe(4);
    expect(l.height).toBe(l.rows[4].y + l.rows[4].h + 100);
  });

  it('adds placeholder rows for titles still loading', () => {
    const l = layoutWall({ ...base, count: 5, total: 18, sectionAt: () => '' });
    const skel = l.rows.filter((r) => r.kind === 'skel');
    expect(skel.reduce((n, r) => n + r.count, 0)).toBe(13);
    expect(skel.length).toBe(3);
  });

  it('puts margin labels a little apart when headers have no height', () => {
    const l = layoutWall({ ...base, head: 0, count: 2, total: 2, sectionAt: (i) => (i === 0 ? 'A' : 'B') });
    expect(l.rows.filter((r) => r.kind === 'head').map((r) => r.h)).toEqual([0, 12]);
  });
});

describe('rowAt', () => {
  it('finds the first row reaching a position', () => {
    const l = layoutWall({ ...base, count: 50, total: 50, sectionAt: () => '' });
    expect(rowAt(l.rows, 0)).toBe(0);
    const third = l.rows[2];
    expect(rowAt(l.rows, third.y + 1)).toBe(2);
    expect(rowAt(l.rows, 1e9)).toBe(l.rows.length);
  });
});
