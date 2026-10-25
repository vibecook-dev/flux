/** The Gallery lens's show: how titles are hung, labelled and windowed. */
import { describe, expect, it } from 'vitest';
import type { ItemSummary } from '../../api/types';
import { hangShow, isGrowth, roomByline, slotRange, wallLines, type RoomSlot, type WorkSlot } from './gallery';

const DAY = 24 * 3600 * 1000;
const NOW = 1_800_000_000_000;

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
  added_at: NOW - 400 * DAY,
  ...o,
});

const decade = (it: ItemSummary) => (it.year ? `${Math.floor(it.year / 10) * 10}s` : 'Undated');

describe('hangShow', () => {
  it('opens a room per section and alternates walls within it', () => {
    const items = [item({ id: 1, year: 1991 }), item({ id: 2, year: 1994 }), item({ id: 3, year: 1999 }), item({ id: 4, year: 2003 })];
    const { slots, works } = hangShow(items, decade);
    expect(slots.map((s) => s.type)).toEqual(['room', 'work', 'work', 'work', 'room', 'work', 'end']);
    const rooms = slots.filter((s): s is RoomSlot => s.type === 'room');
    expect(rooms.map((r) => [r.label, r.sub, r.portal])).toEqual([
      ['1990s', 'Room 1 · 3 works', true],
      ['2000s', 'Room 2 · 1 work', false],
    ]);
    expect(works.map((w) => w.side)).toEqual([1, -1, 1, 1]);
    expect(works.map((w) => w.i)).toEqual([0, 1, 2, 3]);
  });

  it('hangs slots back to back along the corridor', () => {
    const { slots } = hangShow([item({ id: 1 }), item({ id: 2 }), item({ id: 3 })], () => '');
    for (let s = 1; s < slots.length; s++) expect(slots[s].d0).toBe(slots[s - 1].d1);
    expect(slots[0]).toMatchObject({ type: 'room', label: 'The collection', portal: true });
  });

  it('keeps every work where it was when more titles arrive', () => {
    const first = [item({ id: 1, year: 1991 }), item({ id: 2, year: 1992 })];
    const more = [...first, item({ id: 3, year: 1993 }), item({ id: 4, year: 2001 })];
    const a = hangShow(first, decade, 4);
    const b = hangShow(more, decade, 4);
    expect(b.works.slice(0, 2).map((w) => [w.d0, w.side, w.key])).toEqual(a.works.map((w) => [w.d0, w.side, w.key]));
  });

  it('says when the collection is still arriving', () => {
    const { slots } = hangShow([item()], () => '', 120);
    const end = slots[slots.length - 1];
    expect(end).toMatchObject({ type: 'end', count: 1, pending: 119 });
    const loading = hangShow([], () => '', 50).slots;
    expect(loading[0]).toMatchObject({ type: 'room', label: 'Hanging', sub: '50 works on the way' });
    // Nothing matches: the shell explains, the room stays quiet.
    expect(hangShow([], () => '').slots[0]).toMatchObject({ type: 'room', label: '' });
  });
});

describe('slotRange', () => {
  it('finds the slots overlapping a stretch of corridor', () => {
    const { slots } = hangShow(
      Array.from({ length: 40 }, (_, i) => item({ id: i + 1 })),
      () => '',
    );
    const [a, b] = slotRange(slots, 2000, 4000);
    expect(a).toBeGreaterThan(0);
    for (let s = a; s < b; s++) expect(slots[s].d1 > 2000 && slots[s].d0 < 4000).toBe(true);
    expect(slots[a - 1].d1).toBeLessThanOrEqual(2000);
    expect(slots[b].d0).toBeGreaterThanOrEqual(4000);
  });

  it('is empty past the end', () => {
    const { slots } = hangShow([item()], () => '');
    expect(slotRange(slots, 1e7, 2e7)).toEqual([slots.length, slots.length]);
  });
});

describe('isGrowth', () => {
  const a = [item({ id: 1 }), item({ id: 2 })];
  it('is true when titles were only added after the ones shown', () => {
    expect(isGrowth(a, [...a, item({ id: 3 })])).toBe(true);
    expect(isGrowth([], a)).toBe(true);
  });
  it('is false after a search, a filter or a new sort', () => {
    expect(isGrowth(a, [a[1], a[0]])).toBe(false);
    expect(isGrowth(a, [a[0]])).toBe(false);
  });
});

describe('wallLines', () => {
  it('labels a film with its badges and viewing state', () => {
    const l = wallLines(item({ badges: ['4K', 'HDR10', 'Atmos'], progress: { position_ms: 32 * 60_000, duration_ms: 100 * 60_000, watched: false, updated_at: 0 } }), NOW);
    expect(l).toMatchObject({ title: 'Arrival', year: '2016', what: 'Film', medium: '4K · HDR10 · Atmos', status: 'Partly viewed, 32%', tone: '' });
  });

  it('labels shows, episodes and recent or offline titles', () => {
    expect(wallLines(item({ kind: 'show', child_count: 3, unwatched_count: 0 }), NOW)).toMatchObject({ what: 'Show · 3 seasons', status: 'Viewed' });
    expect(wallLines(item({ kind: 'episode', parent_title: 'Severance', subtitle: 'S01E03' }), NOW).what).toBe('Severance · S01E03');
    expect(wallLines(item({ added_at: NOW - DAY }), NOW)).toMatchObject({ status: 'Recent acquisition', tone: 'new' });
    expect(wallLines(item({ available: false, added_at: NOW - DAY }), NOW)).toMatchObject({ status: 'On loan: its drive is offline', tone: 'offline' });
  });
});

describe('roomByline', () => {
  it('says what hangs in a room and when it was made', () => {
    expect(roomByline([item({ year: 1994 }), item({ year: 1991 }), item({ kind: 'show', year: 1999 })])).toBe('2 films and 1 show, 1991–1999');
    expect(roomByline([item({ year: null })])).toBe('1 film');
  });

  it('keeps the work slots typed', () => {
    const w = hangShow([item()], () => '').works[0] satisfies WorkSlot;
    expect(w.item.title).toBe('Arrival');
  });
});
