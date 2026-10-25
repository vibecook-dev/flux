import { afterEach, describe, expect, it, vi } from 'vitest';
import { dismissActivity, liveState, postActivity, resetLive, setIslandReady, setRing } from './live';

afterEach(() => {
  resetLive();
  vi.useRealTimers();
});

describe('live events', () => {
  it('holds events until the island can show them, one at a time', () => {
    vi.useFakeTimers();
    postActivity({ id: 'a', title: 'A' });
    postActivity({ id: 'b', title: 'B' });
    expect(liveState().current).toBeNull();
    setIslandReady(true);
    expect(liveState().current?.id).toBe('a');
    dismissActivity();
    expect(liveState().current).toBeNull();
    vi.advanceTimersByTime(500);
    expect(liveState().current?.id).toBe('b');
  });

  it('says when an event is first shown, not when it is queued (a sheet may cover the island)', () => {
    const shown = vi.fn();
    postActivity({ id: 'offline', title: 'Part of Films is offline', onShown: shown });
    expect(shown).not.toHaveBeenCalled();
    setIslandReady(true);
    expect(shown).toHaveBeenCalledTimes(1);
    setIslandReady(true);
    expect(shown).toHaveBeenCalledTimes(1);
  });

  it('replaces a queued event with the same id, and clears when covered', () => {
    postActivity({ id: 'scan', title: 'old' });
    postActivity({ id: 'scan', title: 'new' });
    setIslandReady(true);
    expect(liveState().current?.title).toBe('new');
    setIslandReady(false);
    expect(liveState().current).toBeNull();
  });

  it('keeps one ring', () => {
    setRing({ kind: 'scanning', progress: 0.5, label: '5 / 10', aria: 'Scanning' });
    expect(liveState().ring?.label).toBe('5 / 10');
    setRing(null);
    expect(liveState().ring).toBeNull();
  });
});
