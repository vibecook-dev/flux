import { describe, expect, it } from 'vitest';
import type { ItemSummary } from '../api/types';
import { hasPosterColumn, isResumable, isWatched, resultEdge } from './card-utils';

const base: ItemSummary = {
  id: 1,
  kind: 'movie',
  title: 'Long 2h',
  parent_title: null,
  year: null,
  subtitle: null,
  poster: null,
  backdrop: null,
  progress: null,
  badges: [],
  child_count: null,
  unwatched_count: null,
  available: true,
  added_at: 0,
};
const progress = (position_ms: number, watched: boolean) => ({ position_ms, duration_ms: 7_200_000, watched, updated_at: 0 });

describe('watched and resume state', () => {
  it('a movie is watched by its progress; a show when no episode is left', () => {
    expect(isWatched(base)).toBe(false);
    expect(isWatched({ ...base, progress: progress(0, true) })).toBe(true);
    expect(isWatched({ ...base, kind: 'show', unwatched_count: 3 })).toBe(false);
    expect(isWatched({ ...base, kind: 'show', unwatched_count: 0 })).toBe(true);
  });

  it('a rewatch in progress is resumable (the e2e test resumed it at 0)', () => {
    expect(isResumable({ ...base, progress: progress(1_834_000, true) })).toBe(true);
    expect(isResumable({ ...base, progress: progress(1_834_000, false) })).toBe(true);
    expect(isResumable({ ...base, progress: progress(0, true) })).toBe(false);
    expect(isResumable(base)).toBe(false);
  });
});

describe('search result labels', () => {
  it('name the kind (episodes, seasons and extras were labelled "Film")', () => {
    expect(resultEdge({ ...base, year: 1982, badges: ['720p'] })).toBe('Film · 1982 · 720p');
    expect(resultEdge({ ...base, kind: 'episode', title: 'Pilot', parent_title: 'Breaking Bad', subtitle: 'S01E01' })).toBe('Episode · Breaking Bad · S01E01');
    expect(resultEdge({ ...base, kind: 'show', year: 2008, child_count: 5 })).toBe('Show · 2008 · 5 seasons');
    expect(resultEdge({ ...base, kind: 'extra', parent_title: 'Alien' })).toBe('Extra · Alien');
  });
});

describe('detail page layout', () => {
  it('an episode page has no (empty) poster column; films, shows and seasons keep theirs', () => {
    expect(hasPosterColumn('episode')).toBe(false);
    for (const kind of ['movie', 'show', 'season', 'extra'] as const) expect(hasPosterColumn(kind)).toBe(true);
  });
});
