/** The Libraries & scanning screen's words and event bookkeeping. */
import { describe, expect, it } from 'vitest';
import type { Library, ScanStatus } from '../api/types';
import { applyScanProgress, isFirstScan, lastScanLine, offlineNote, offlineRoots, scanCounters, stateLabel } from './admin-state';

const status = (o: Partial<ScanStatus> = {}): ScanStatus => ({
  state: 'watching',
  files_seen: 20,
  files_total: 20,
  files_probed: 0,
  items_added: 0,
  items_updated: 0,
  items_removed: 0,
  started_at: 1_000,
  finished_at: 1_070,
  last_error: null,
  dirs_unavailable: 0,
  ...o,
});
const lib = (o: Partial<Library> = {}): Library => ({
  id: 5,
  name: 'Scratch',
  kind: 'movies',
  roots: [{ path: '/m/scratch', available: true, last_seen: null }],
  item_count: 0,
  scan: status(),
  ...o,
});
const ago = () => 'now';
const count = (n: number) => String(n);

describe('an offline drive in words (e2e: the card said “Partly offline” and showed the raw io error)', () => {
  const away = lib({
    roots: [{ path: '/m/scratch', available: false, last_seen: 1_000 }],
    item_count: 16,
    scan: status({ state: 'unavailable', files_seen: 0, last_error: 'root unreachable: io error (ENOENT) at "/m/scratch": No such file or directory (os error 2)' }),
  });
  const offline = offlineRoots([away], new Map());

  it('a library whose only folder is gone is “Offline”, not “Partly offline”', () => {
    expect(stateLabel(away, offline)).toBe('Offline');
    const two = lib({ ...away, roots: [...away.roots, { path: '/m/other', available: true, last_seen: null }] });
    expect(stateLabel(two, offlineRoots([two], new Map()))).toBe('Partly offline');
  });

  it('explains what happens instead of printing the io error', () => {
    const note = offlineNote(away, offline) ?? '';
    expect(note).toMatch(/isn’t responding/);
    expect(note).not.toMatch(/ENOENT|io error/);
    expect(offlineNote(lib(), new Set())).toBeNull();
  });

  it('does not claim “nothing changed, no files opened” for a drive that did not answer', () => {
    expect(lastScanLine(away, ago, count)).toBe('Last checked now · it didn’t answer, so nothing was removed');
    expect(lastScanLine(lib(), ago, count)).toBe('Last scan now in 0.1 s · 20 files checked · nothing changed, no files opened');
  });

  it('live availability events override what the list says', () => {
    expect(offlineRoots([lib()], new Map([['/m/scratch', false]])).has('/m/scratch')).toBe(true);
    expect(offlineRoots([away], new Map([['/m/scratch', true]])).size).toBe(0);
  });
});

describe('scan progress events', () => {
  it('replace the library’s status and report what it was doing', () => {
    const scanning = lib({ scan: status({ state: 'scanning', finished_at: null }) });
    const r = applyScanProgress([scanning, lib({ id: 6, name: 'Other' })], 5, status());
    expect(r.previous).toBe('scanning');
    expect(r.name).toBe('Scratch');
    expect(r.libs[0].scan.state).toBe('watching');
    expect(r.libs[1].name).toBe('Other');
    expect(applyScanProgress(undefined, 5, status()).previous).toBeNull();
  });
});

describe('scan counters count files and titles separately (e2e P2 #6)', () => {
  // The e2e TV library's first scan: 10,949 files became 300 shows, and the
  // card said "12,658 added, 465 updated" (shows + seasons + episodes, and
  // parents touched again by later batches).
  const tvFirst = lib({
    kind: 'shows',
    item_count: 300,
    scan: status({ files_seen: 10_949, files_total: 10_949, files_probed: 10_949, items_added: 12_658, items_updated: 465, started_at: 1_000, finished_at: 6_200 }),
  });

  it('a first scan reads as files read and titles added, with no "updated"', () => {
    expect(isFirstScan(tvFirst.scan)).toBe(true);
    const line = lastScanLine(tvFirst, ago, count) ?? '';
    expect(line).toBe('Last scan now in 5.2 s · 10949 files read · 300 shows added');
    expect(line).not.toMatch(/12658|updated/);
    expect(lastScanLine(lib({ kind: 'movies', item_count: 1, scan: status({ files_seen: 1, files_probed: 1, items_added: 1 }) }), ago, count)).toBe(
      'Last scan now in 0.1 s · 1 file read · 1 film added',
    );
  });

  it('a rescan names files checked and read, then only the changes there were', () => {
    const rescan = lib({ scan: status({ files_seen: 3_811, files_probed: 12, items_added: 10, items_updated: 2 }) });
    expect(isFirstScan(rescan.scan)).toBe(false);
    expect(lastScanLine(rescan, ago, count)).toBe('Last scan now in 0.1 s · 3811 files checked, 12 read · 10 added, 2 updated');
    const touched = lib({ scan: status({ files_seen: 20, files_probed: 2 }) });
    expect(lastScanLine(touched, ago, count)).toBe('Last scan now in 0.1 s · 20 files checked, 2 read · nothing changed in the library');
  });

  it('the live readout shows files found and read, and the library’s titles, not item-level added/updated', () => {
    const scanning = lib({ ...tvFirst, scan: { ...tvFirst.scan, state: 'scanning', files_total: null, finished_at: null } });
    const counters = scanCounters(scanning, count);
    expect(counters).toEqual([
      ['Files found', '10949'],
      ['Files read', '10949'],
      ['Shows', '300'],
    ]);
    expect(counters.map(([l]) => l)).not.toContain('Updated');
    expect(scanCounters(lib({ scan: status({ files_seen: 40, files_total: 180 }) }), count)[0]).toEqual(['Files found', '40 of 180']);
    // Files in flight are read before they count as seen (decided): never "read" more than "found".
    expect(scanCounters(lib({ scan: status({ files_seen: 160, files_probed: 169, files_total: null }) }), count).slice(0, 2)).toEqual([
      ['Files found', '169'],
      ['Files read', '169'],
    ]);
  });
});
