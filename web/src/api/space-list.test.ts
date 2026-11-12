import { describe, expect, it } from 'vitest';
import { allLibraries, mergeSorted, settledPrefix } from './space-list';
import type { ItemSummary, Library } from './types';

const MIN = 60 * 1000;

const it0 = (id: number, title: string, year: number | null, added = 0, duration: number | null = null): ItemSummary => ({
  id,
  kind: 'movie',
  title,
  parent_title: null,
  year,
  subtitle: null,
  poster: null,
  backdrop: null,
  progress: null,
  badges: [],
  child_count: null,
  unwatched_count: null,
  available: true,
  added_at: added,
  duration_ms: duration,
});

describe('mergeSorted', () => {
  it('merges per-library lists in the server order (articles ignored)', () => {
    const a = [it0(1, 'Alien', 1979), it0(2, 'The Matrix', 1999)];
    const b = [it0(3, 'Heat', 1995), it0(4, 'Zodiac', 2007)];
    expect(mergeSorted([a, b], 'title').map((i) => i.title)).toEqual(['Alien', 'Heat', 'The Matrix', 'Zodiac']);
  });

  it('merges by year, newest first, undated last', () => {
    const a = [it0(1, 'B', 2010), it0(2, 'C', null)];
    const b = [it0(3, 'A', 2020), it0(4, 'D', 1990)];
    expect(mergeSorted([a, b], '-year').map((i) => i.id)).toEqual([3, 1, 4, 2]);
  });

  it('merges by date added, newest first', () => {
    const a = [it0(1, 'A', 2000, 50), it0(2, 'B', 2000, 10)];
    const b = [it0(3, 'C', 2000, 40)];
    expect(mergeSorted([a, b], '-added').map((i) => i.id)).toEqual([1, 3, 2]);
  });

  it('merges by length, shortest first, unknown lengths last, ties by title', () => {
    const a = [it0(1, 'B', 2000, 0, 90 * MIN), it0(2, 'C', 2000, 0, 120 * MIN), it0(3, 'Z', 2000, 0, null)];
    const b = [it0(4, 'A', 2000, 0, 90 * MIN), it0(5, 'D', 2000, 0, null)];
    expect(mergeSorted([a, b], 'duration').map((i) => i.id)).toEqual([4, 1, 2, 5, 3]);
  });

  it('merges by length, longest first, unknown lengths still last', () => {
    const a = [it0(2, 'C', 2000, 0, 120 * MIN), it0(1, 'B', 2000, 0, 90 * MIN), it0(3, 'Z', 2000, 0, null)];
    const b = [it0(4, 'A', 2000, 0, 90 * MIN), it0(5, 'D', 2000, 0, null)];
    expect(mergeSorted([a, b], '-duration').map((i) => i.id)).toEqual([2, 4, 1, 5, 3]);
  });

  it('handles empty lists', () => {
    expect(mergeSorted([[], []], 'title')).toEqual([]);
  });
});

describe('allLibraries', () => {
  it('leaves videos libraries out of All', () => {
    const lib = (id: number, kind: Library['kind']) => ({ id, kind }) as Library;
    expect(allLibraries([lib(1, 'movies'), lib(2, 'videos'), lib(3, 'shows')]).map((l) => l.id)).toEqual([1, 3]);
  });
});

describe('settledPrefix', () => {
  it('holds back titles that a library still loading could come before', () => {
    const done = { items: [it0(1, 'Alien', 1979), it0(2, 'Zodiac', 2007)], done: true };
    const loading = { items: [it0(3, 'Heat', 1995)], done: false };
    const merged = mergeSorted([done.items, loading.items], 'title');
    expect(settledPrefix(merged, [done, loading], 'title').map((i) => i.title)).toEqual(['Alien', 'Heat']);
  });

  it('shows nothing until every list has a first page, and everything once all are done', () => {
    const a = { items: [it0(1, 'Alien', 1979)], done: false };
    const b = { items: [] as ReturnType<typeof it0>[], done: false };
    expect(settledPrefix(mergeSorted([a.items, b.items], 'title'), [a, b], 'title')).toEqual([]);
    const merged = mergeSorted([a.items], 'title');
    expect(settledPrefix(merged, [{ ...a, done: true }], 'title')).toBe(merged);
  });
});
