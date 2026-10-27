import { describe, expect, it } from 'vitest';
import { fallbackPalette } from '../palette';
import { shelfMetrics } from './geometry';
import { intrusion, peekDepth } from './peek';
import { planShelf, spineLook, spineTitleParts } from './shelf';

describe('the DOM Shelf’s turn', () => {
  it('brings every tape (plain or a box set) out far enough to turn clear of its neighbours at any angle', () => {
    // shelf.ts sets one --peek-z for the shelf, a box set's (the widest turn).
    for (const [vw, vh] of [
      [1440, 900],
      [390, 844],
    ]) {
      const m = shelfMetrics({ vw, vh, safeTop: 60, safeBottom: 90, mini: false });
      const z = peekDepth({ H: m.H, W: m.W, D: m.D, tw: m.boxW, gap: m.gap });
      for (const tw of [m.W, m.boxW]) {
        for (let a = 0; a <= 90; a += 1) expect(intrusion(tw, m.D, m.gap, (a * Math.PI) / 180, z)).toBe(0);
      }
    }
  });
});

const base = { rows: 2, fullW: 1000, bayW: 900, W: 30, gap: 2, secGap: 15 };

describe('planShelf', () => {
  it('puts a few tapes on one centred shelf', () => {
    const p = planShelf({ ...base, widths: [30, 30, 30], groups: ['', '', ''] });
    expect(p.oneBay).toBe(true);
    expect(p.rows).toBe(1);
    expect(p.bays).toBe(1);
    expect([...p.x]).toEqual([0, 32, 64]);
    expect(p.bayStart).toEqual([0, 3]);
  });

  it('fills a shelf, then the one below, then the next bay', () => {
    // 30 px tapes with 2 px gaps: 29 fit in 900 px (29 × 30 + 28 × 2 = 926 > 900 → 28).
    const n = 120;
    const p = planShelf({ ...base, widths: new Array<number>(n).fill(30), groups: new Array<string>(n).fill('') });
    expect(p.oneBay).toBe(false);
    const perRow = [...p.row].filter((r, i) => p.bay[i] === 0 && r === 0).length;
    expect(perRow).toBe(28);
    expect(p.bay[2 * perRow]).toBe(1);
    expect(p.row[2 * perRow]).toBe(0);
    expect(p.bayStart[1]).toBe(2 * perRow);
    expect(p.bayStart[p.bayStart.length - 1]).toBe(n);
    expect(p.bayStart.length).toBe(p.bays + 1);
    // Every tape sits inside its bay.
    for (let i = 0; i < n; i++) expect(p.x[i] + 30).toBeLessThanOrEqual(900);
  });

  it('leaves a gap and a tag where a section starts', () => {
    const p = planShelf({ ...base, rows: 1, widths: [30, 30, 30, 30], groups: ['A', 'A', 'B', 'B'] });
    expect(p.tags.map((t) => t.label)).toEqual(['A', 'B']);
    expect(p.x[2] - p.x[1]).toBe(30 + 2 + 15);
    expect(p.tags[1].x).toBe(p.x[2]);
  });

  it('copes with an empty shelf', () => {
    const p = planShelf({ ...base, widths: [], groups: [] });
    expect(p.n).toBe(0);
    expect(p.bays).toBe(1);
    expect(p.bayStart).toEqual([0, 0]);
    expect(p.bayLen).toEqual([0]);
  });

  it('gives box sets their own width', () => {
    const p = planShelf({ ...base, widths: [30, 56, 30], groups: ['', '', ''] });
    expect(p.x[2]).toBe(30 + 2 + 56 + 2);
  });
});

describe('spines', () => {
  it('sets a leading article and a subtitle smaller', () => {
    expect(spineTitleParts('The General')).toEqual({ article: 'THE', main: 'GENERAL', sub: null });
    expect(spineTitleParts('Dune: Part Two')).toEqual({ article: null, main: 'DUNE', sub: 'PART TWO' });
    expect(spineTitleParts('A')).toEqual({ article: null, main: 'A', sub: null });
  });

  it('picks the same look for the same title every time', () => {
    const p = fallbackPalette('Metropolis');
    const a = spineLook({ id: 42, kind: 'movie', year: 1927 }, p);
    const b = spineLook({ id: 42, kind: 'movie', year: 1927 }, p);
    expect(a).toEqual(b);
    expect(spineLook({ id: 7, kind: 'show', year: 2008 }, p).style).toBe('box');
  });
});
