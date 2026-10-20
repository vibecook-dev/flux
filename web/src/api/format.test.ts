import { encode } from 'blurhash';
import { describe, expect, it } from 'vitest';
import { averageColor } from '../components/blurhash';
import { parseEvent } from './events';
import { foldForSearch, formatBitrate, formatBytes, formatClock, formatMs, formatRemaining, formatRuntime, titleSortKey } from './format';
import { median } from '../player/metrics';

describe('formatting', () => {
  it('formats runtimes and clocks', () => {
    expect(formatRuntime(9_960_000)).toBe('2h 46m');
    expect(formatRuntime(1_380_000)).toBe('23m');
    expect(formatClock(4_324_000)).toBe('1:12:04');
    expect(formatClock(83_000)).toBe('1:23');
    expect(formatRemaining(0, 23 * 60_000)).toBe('23 min left');
    // Clips under a minute (seen in the e2e library: "0M", and "1M" for 30 s).
    expect(formatRuntime(6_000)).toBe('6s');
    expect(formatRuntime(30_000)).toBe('30s');
    expect(formatRuntime(400)).toBe('1s');
    expect(formatRuntime(60_018)).toBe('1m');
  });

  it('rounds server timings (the stats overlay showed "196.53346 ms")', () => {
    expect(formatMs(196.53346)).toBe('197 ms');
    expect(formatMs(2.570417)).toBe('2.6 ms');
    expect(formatMs(1234.5)).toBe(`${(1235).toLocaleString()} ms`);
  });

  it('formats sizes and bitrates', () => {
    expect(formatBytes(58_400_000_000)).toBe('58.4 GB');
    expect(formatBitrate(640_000)).toBe('640 kb/s');
    expect(formatBitrate(8_000_000)).toBe('8.0 Mb/s');
  });

  it('folds accents and leading articles for search and sort', () => {
    expect(foldForSearch('Amélie')).toBe('amelie');
    expect(titleSortKey('The Matrix')).toBe('matrix');
  });
});

describe('events and helpers', () => {
  it('parses SSE payloads and ignores unknown ones', () => {
    expect(parseEvent('{"type":"source_availability","library_id":3,"root":"/nas","available":false}')).toEqual({
      type: 'source_availability',
      library_id: 3,
      root: '/nas',
      available: false,
    });
    expect(parseEvent('{"type":"later_event"}')).toBeNull();
    expect(parseEvent('not json')).toBeNull();
  });

  it('reads the average colour from a blurhash (its DC term)', () => {
    // A flat image encodes its colour exactly in the DC component.
    const px = new Uint8ClampedArray(4 * 4 * 4);
    for (let i = 0; i < px.length; i += 4) px.set([200, 30, 90, 255], i);
    expect(averageColor(encode(px, 4, 4, 3, 3))).toBe('rgb(200 30 90)');
    expect(averageColor('')).toBeNull();
  });

  it('takes medians', () => {
    expect(median([])).toBeNull();
    expect(median([5, 1, 3])).toBe(3);
    expect(median([4, 1, 3, 2])).toBe(3);
  });
});
