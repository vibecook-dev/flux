import { describe, expect, it } from 'vitest';
import type { ItemSummary } from '../api/types';
import { colourKey, foldWords, isNew, matchesShow, matchesText, parseShow, parseSort, progressFraction, sectionOf, serverSort, visibleItems } from './sections';

const NOW = Date.UTC(2026, 9, 25);
const DAY = 24 * 3600 * 1000;

function item(over: Partial<ItemSummary> = {}): ItemSummary {
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
    added_at: NOW - 100 * DAY,
    ...over,
  };
}

describe('show filters', () => {
  it('splits unwatched, in progress, new, 4K and HDR', () => {
    const watched = item({ progress: { position_ms: 0, duration_ms: 1000, watched: true, updated_at: 0 } });
    const partway = item({ progress: { position_ms: 400, duration_ms: 1000, watched: false, updated_at: 0 } });
    const fresh = item({ added_at: NOW - 2 * DAY });
    const hdr = item({ badges: ['4K', 'DV'] });
    expect(matchesShow(watched, 'unwatched', NOW)).toBe(false);
    expect(matchesShow(partway, 'unwatched', NOW)).toBe(true);
    expect(matchesShow(partway, 'progress', NOW)).toBe(true);
    expect(matchesShow(watched, 'progress', NOW)).toBe(false);
    expect(matchesShow(fresh, 'new', NOW)).toBe(true);
    expect(matchesShow(item(), 'new', NOW)).toBe(false);
    expect(matchesShow(hdr, '4k', NOW)).toBe(true);
    expect(matchesShow(hdr, 'hdr', NOW)).toBe(true);
    expect(matchesShow(item({ badges: ['HDR10'] }), 'hdr', NOW)).toBe(true);
    expect(matchesShow(item(), 'hdr', NOW)).toBe(false);
  });

  it('treats a show as watched only when no episode is left', () => {
    expect(matchesShow(item({ kind: 'show', unwatched_count: 0 }), 'unwatched', NOW)).toBe(false);
    expect(matchesShow(item({ kind: 'show', unwatched_count: 3 }), 'unwatched', NOW)).toBe(true);
  });

  it('knows progress and newness', () => {
    expect(progressFraction(item({ progress: { position_ms: 250, duration_ms: 1000, watched: false, updated_at: 0 } }))).toBe(0.25);
    expect(progressFraction(item())).toBeNull();
    expect(isNew(item({ added_at: NOW - DAY }), NOW)).toBe(true);
  });
});

describe('search as you type', () => {
  it('folds accents and case, and needs every word', () => {
    const amelie = item({ title: 'Amélie', year: 2001 });
    expect(matchesText(amelie, foldWords('amelie'))).toBe(true);
    expect(matchesText(amelie, foldWords('AMÉ 2001'))).toBe(true);
    expect(matchesText(amelie, foldWords('amelie 1999'))).toBe(false);
    expect(matchesText(item({ kind: 'episode', title: 'Pilot', parent_title: 'Severance' }), foldWords('severance'))).toBe(true);
  });
});

describe('sections', () => {
  it('groups by letter, decade and when added', () => {
    expect(sectionOf(item({ title: 'The Matrix' }), 'title', NOW)).toBe('M');
    expect(sectionOf(item({ title: '2001: A Space Odyssey' }), 'title', NOW)).toBe('#');
    expect(sectionOf(item({ year: 1994 }), '-year', NOW)).toBe('1990s');
    expect(sectionOf(item({ year: null }), 'year', NOW)).toBe('Undated');
    expect(sectionOf(item({ added_at: NOW - 3 * DAY }), '-added', NOW)).toBe('This week');
    expect(sectionOf(item({ added_at: NOW - 20 * DAY }), '-added', NOW)).toBe('This month');
    expect(sectionOf(item({ added_at: NOW - 400 * DAY }), '-added', NOW)).toBe('Earlier');
    expect(sectionOf(item(), 'colour', NOW)).toBe('');
  });
});

describe('sorts and parsing', () => {
  it('maps colour to a server sort and rejects unknown values', () => {
    expect(serverSort('colour')).toBe('title');
    expect(serverSort('-year')).toBe('-year');
    expect(parseSort('nope')).toBe('title');
    expect(parseSort('-added')).toBe('-added');
    expect(parseShow('hdr')).toBe('hdr');
    expect(parseShow(null)).toBe('all');
  });

  it('orders by colour round the hue circle, greys last', () => {
    const items = [item({ id: 1, title: 'B' }), item({ id: 2, title: 'A' })];
    const sorted = visibleItems(items, { show: 'all', text: '', sort: 'colour' });
    expect(sorted.map((i) => colourKey(i))).toEqual([...sorted.map((i) => colourKey(i))].sort((a, b) => a - b));
  });

  it('keeps the server order when nothing filters', () => {
    const items = [item({ id: 1, title: 'B' }), item({ id: 2, title: 'A' })];
    expect(visibleItems(items, { show: 'all', text: '', sort: 'title' }).map((i) => i.id)).toEqual([1, 2]);
    expect(visibleItems(items, { show: 'all', text: 'a', sort: 'title' }).map((i) => i.id)).toEqual([2]);
  });
});
