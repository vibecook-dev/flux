import { describe, expect, it } from 'vitest';
import { bufferedEnd, createClock, fraction, seekLanded, seekStarted, shownTime } from './playhead';

const playing = (time: number) => ({ time, advancing: true, rate: 1 });
const seeking = (time: number) => ({ time, advancing: false, rate: 1 });
const paused = (time: number) => ({ time, advancing: false, rate: 1 });

describe('playhead', () => {
  it('holds a seek target until the video lands there, never showing the old position', () => {
    const c = createClock(100);
    expect(shownTime(c, playing(100), 0)).toBe(100);
    seekStarted(c, 400, 10);
    // The old clock still reports 100 for a while (a remote server, a slow segment).
    for (let now = 20; now < 1000; now += 16) expect(shownTime(c, seeking(100), now)).toBe(400);
    // The element has moved but hasn't said `seeked`: still the target.
    expect(shownTime(c, seeking(400), 1000)).toBe(400);
    seekLanded(c);
    expect(shownTime(c, paused(400), 1016)).toBe(400);
    expect(c.target).toBeNull();
    // And playback goes on from there.
    expect(shownTime(c, playing(400.5), 1500)).toBeCloseTo(400.5, 5);
  });

  it('a newer seek replaces one still on its way', () => {
    const c = createClock(10);
    seekStarted(c, 300, 0);
    seekStarted(c, 50, 40);
    // A late `seeked` from the first one changes nothing while the video isn't near the newer target.
    seekLanded(c);
    expect(shownTime(c, seeking(300), 60)).toBe(50);
    expect(shownTime(c, paused(50), 80)).toBe(50);
    expect(c.target).toBeNull();
  });

  it('counts a seek as landed when the video plays on near the target, even without `seeked`', () => {
    const c = createClock(0);
    seekStarted(c, 60, 0);
    expect(shownTime(c, playing(60.2), 200)).toBeCloseTo(60.2, 5);
    expect(c.target).toBeNull();
  });

  it('stops holding a seek that never lands', () => {
    const c = createClock(5);
    seekStarted(c, 500, 0);
    expect(shownTime(c, seeking(5), 11_000)).toBe(500);
    expect(shownTime(c, paused(5), 12_500)).toBe(5);
  });

  it('glides between readings while playing, at most a quarter second ahead', () => {
    const c = createClock(10);
    expect(shownTime(c, playing(10), 1000)).toBe(10);
    // No new reading for 100 ms: runs on at the playback rate.
    expect(shownTime(c, playing(10), 1100)).toBeCloseTo(10.1, 5);
    // A long stall in readings: capped.
    expect(shownTime(c, playing(10), 3000)).toBeCloseTo(10.25, 5);
  });

  it('never steps back a little during playback, but takes a real jump', () => {
    const c = createClock(20);
    shownTime(c, playing(20), 0);
    expect(shownTime(c, playing(20), 200)).toBeCloseTo(20.2, 5);
    // A reading slightly behind what was shown: hold.
    expect(shownTime(c, playing(20.1), 210)).toBeCloseTo(20.2, 5);
    // The video jumped back on its own (a loop, a restart): follow it.
    expect(shownTime(c, playing(5), 400)).toBe(5);
  });

  it('shows the exact time when paused', () => {
    const c = createClock(0);
    shownTime(c, playing(30), 0);
    expect(shownTime(c, paused(30.04), 5000)).toBe(30.04);
  });

  it('runs at the playback rate', () => {
    const c = createClock(0);
    shownTime(c, { time: 0, advancing: true, rate: 2 }, 0);
    expect(shownTime(c, { time: 0, advancing: true, rate: 2 }, 100)).toBeCloseTo(0.2, 5);
  });
});

describe('fraction and buffered', () => {
  it('clamps to the bar', () => {
    expect(fraction(30, 120)).toBe(0.25);
    expect(fraction(-1, 120)).toBe(0);
    expect(fraction(500, 120)).toBe(1);
    expect(fraction(10, 0)).toBe(0);
    expect(fraction(10, Number.NaN)).toBe(0);
  });

  it('finds the end of the range the playhead is in', () => {
    const ranges = (pairs: [number, number][]): TimeRanges => ({
      length: pairs.length,
      start: (i: number) => pairs[i][0],
      end: (i: number) => pairs[i][1],
    });
    expect(bufferedEnd(ranges([[0, 30], [60, 90]]), 10)).toBe(30);
    expect(bufferedEnd(ranges([[0, 30], [60, 90]]), 70)).toBe(90);
    expect(bufferedEnd(ranges([[0, 30], [60, 90]]), 45)).toBe(45);
    expect(bufferedEnd(ranges([]), 12)).toBe(12);
  });
});
