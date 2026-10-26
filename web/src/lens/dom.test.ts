import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { prefs } from './dom';

describe('prefs', () => {
  const store = new Map<string, string>();
  beforeEach(() => {
    store.clear();
    vi.stubGlobal('localStorage', {
      getItem: (k: string) => store.get(k) ?? null,
      setItem: (k: string, v: string) => void store.set(k, v),
    });
  });
  afterEach(() => vi.unstubAllGlobals());

  it('round-trips values under the flux. prefix', () => {
    prefs.set('lens.choice', { all: 'flow' });
    expect(store.get('flux.lens.choice')).toBe('{"all":"flow"}');
    expect(prefs.get('lens.choice', {})).toEqual({ all: 'flow' });
  });

  it('falls back when a stored value has another shape, so a bad value cannot break the page', () => {
    store.set('flux.lens.choice', 'null');
    expect(prefs.get('lens.choice', {})).toEqual({});
    store.set('flux.lens.pins', '"wall"');
    expect(prefs.get('lens.pins', ['wall'])).toEqual(['wall']);
    store.set('flux.coach.done', '1');
    expect(prefs.get('coach.done', false)).toBe(false);
    store.set('flux.wall.level', '{oops');
    expect(prefs.get('wall.level', 2)).toBe(2);
  });

  it('falls back when storage is blocked', () => {
    vi.stubGlobal('localStorage', {
      getItem: () => {
        throw new Error('blocked');
      },
      setItem: () => {
        throw new Error('blocked');
      },
    });
    expect(prefs.get('wall.level', 2)).toBe(2);
    expect(() => prefs.set('wall.level', 3)).not.toThrow();
  });
});
