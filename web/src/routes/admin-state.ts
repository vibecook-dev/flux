/**
 * Pure helpers for the Libraries & scanning screen (unit-tested in
 * admin-state.test.ts): applying live scan events and the words for a
 * library's state. Kept out of Admin.tsx so the copy for an offline drive and
 * the event bookkeeping can be checked without a browser.
 */
import type { Library, LibraryKind, MetadataSource, ScanStatus } from '../api/types';

export const STATE_LABEL: Record<ScanStatus['state'], string> = {
  idle: 'Not scanned yet',
  scanning: 'Scanning',
  watching: 'Watching for changes',
  unavailable: 'Partly offline',
  error: 'Scan failed',
};

/** Roots that don't answer: what the library reports, overridden by live SSE events. */
export function offlineRoots(libs: Library[], live: Map<string, boolean>): Set<string> {
  const set = new Set<string>();
  for (const l of libs) for (const r of l.roots) if (!(live.get(r.path) ?? r.available)) set.add(r.path);
  return set;
}

/** "Offline" when none of a library's folders answer, "Partly offline" when some don't. */
export function stateLabel(lib: Library, offline: Set<string>): string {
  if (lib.scan.state === 'unavailable' && lib.roots.length > 0 && lib.roots.every((r) => offline.has(r.path))) return 'Offline';
  return STATE_LABEL[lib.scan.state];
}

/**
 * The note under an offline library, in words (the scanner's `last_error`,
 * e.g. "root unreachable: io error (ENOENT) at …", goes in a tooltip).
 */
export function offlineNote(lib: Library, offline: Set<string>): string | null {
  if (lib.scan.state !== 'unavailable') return null;
  const down = lib.roots.filter((r) => offline.has(r.path)).length;
  const all = down > 0 && down === lib.roots.length;
  const what = all ? (lib.roots.length === 1 ? 'This library’s folder isn’t' : 'None of this library’s folders are') : 'Some of this library’s folders aren’t';
  return `${what} responding. flux kept every item; they play again as soon as the folder is back.`;
}

/**
 * Why titles look like file names, when the server has nothing to match
 * them against (no TMDB key, no offline exports); `null` otherwise.
 */
export function metadataNote(source: MetadataSource): string | null {
  if (source !== 'none') return null;
  return 'Titles, years and posters come from the file names and the videos themselves: flux has no TMDB key to look them up. To match them, set TMDB_READ_TOKEN (or TMDB_API_KEY) in flux-server’s environment and restart it.';
}

const TITLES: Record<LibraryKind, [one: string, many: string]> = { movies: ['film', 'films'], shows: ['show', 'shows'], mixed: ['title', 'titles'] };

/** "300 shows", "1 film". */
function titles(kind: LibraryKind, n: number, count: (n: number) => string): string {
  return `${count(n)} ${TITLES[kind][n === 1 ? 0 : 1]}`;
}

/**
 * A scan that read every file it saw and removed nothing: a library's first
 * scan. Its item counters read oddly there (e2e P2 #6: "12,658 added, 465
 * updated" for 10,949 files): `items_added` counts shows, seasons and
 * episodes, and `items_updated` counts parents touched again by later
 * batches. Such a scan is described by files and titles instead.
 */
export function isFirstScan(scan: ScanStatus): boolean {
  return scan.files_seen > 0 && scan.files_probed >= scan.files_seen && scan.items_removed === 0 && scan.items_added > 0;
}

/**
 * The live counters while a library scans: files (found, read) and titles,
 * counted separately. Item-level added/updated counts are left to the line
 * at rest; mid-scan they mix shows, seasons and episodes.
 */
export function scanCounters(lib: Library, count: (n: number) => string): [label: string, value: string][] {
  const s = lib.scan;
  const many = TITLES[lib.kind][1];
  // `files_seen` counts files whose fate is decided; a file being read is found too.
  const found = Math.max(s.files_seen, s.files_probed);
  return [
    ['Files found', `${count(found)}${s.files_total !== null && s.files_total !== found ? ` of ${count(s.files_total)}` : ''}`],
    ['Files read', count(s.files_probed)],
    [many.charAt(0).toUpperCase() + many.slice(1), count(lib.item_count)],
  ];
}

/** The line under a library at rest. */
export function lastScanLine(lib: Library, ago: (ms: number) => string, count: (n: number) => string): string | null {
  const scan = lib.scan;
  if (!scan.finished_at) return null;
  if (scan.state === 'unavailable') return `Last checked ${ago(scan.finished_at)} · it didn’t answer, so nothing was removed`;
  const took = scan.started_at ? ` in ${((scan.finished_at - scan.started_at) / 1000).toFixed(1)} s` : '';
  const head = `Last scan ${ago(scan.finished_at)}${took}`;
  const nFiles = (n: number) => `${count(n)} ${n === 1 ? 'file' : 'files'}`;
  if (isFirstScan(scan)) return `${head} · ${nFiles(scan.files_seen)} read · ${titles(lib.kind, lib.item_count, count)} added`;
  const files = scan.files_probed > 0 ? `${nFiles(scan.files_seen)} checked, ${count(scan.files_probed)} read` : `${nFiles(scan.files_seen)} checked`;
  const changes = (
    [
      [scan.items_added, 'added'],
      [scan.items_updated, 'updated'],
      [scan.items_removed, 'removed'],
    ] as const
  )
    .filter(([n]) => n > 0)
    .map(([n, what]) => `${count(n)} ${what}`);
  if (changes.length === 0) return `${head} · ${files} · ${scan.files_probed > 0 ? 'nothing changed in the library' : 'nothing changed, no files opened'}`;
  return `${head} · ${files} · ${changes.join(', ')}`;
}

/**
 * Applies a `scan_progress` event to the cached library list. Returns the new
 * list and what the library was doing before (for the live announcement and
 * the item-count refresh when a scan ends).
 */
export function applyScanProgress(
  libs: Library[] | undefined,
  libraryId: number,
  status: ScanStatus,
): { libs: Library[]; previous: ScanStatus['state'] | null; name: string | null } {
  let previous: ScanStatus['state'] | null = null;
  let name: string | null = null;
  const next = (libs ?? []).map((l) => {
    if (l.id !== libraryId) return l;
    previous = l.scan.state;
    name = l.name;
    return { ...l, scan: status };
  });
  return { libs: next, previous, name };
}
