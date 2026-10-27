/** Formatting helpers shared by the screens. Pure functions; locale-aware where it matters. */
import type { NetworkPath } from './types';

/** 9_960_000 → "2h 46m"; 1_380_000 → "23m"; 30_000 → "30s" (not "1m" or "0m"). */
export function formatRuntime(ms: number | null | undefined): string {
  if (!ms || ms <= 0) return '';
  if (ms < 59_500) return `${Math.max(1, Math.round(ms / 1000))}s`;
  const totalMin = Math.round(ms / 60000);
  const h = Math.floor(totalMin / 60);
  const m = totalMin % 60;
  if (h === 0) return `${m}m`;
  return m === 0 ? `${h}h` : `${h}h ${m}m`;
}

/** Media clock: 4_324_000 → "1:12:04"; 83_000 → "1:23". */
export function formatClock(ms: number): string {
  const total = Math.max(0, Math.floor(ms / 1000));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  const ss = String(s).padStart(2, '0');
  return h > 0 ? `${h}:${String(m).padStart(2, '0')}:${ss}` : `${m}:${ss}`;
}

/** "23 min left", "1h 12m left". */
export function formatRemaining(positionMs: number, durationMs: number): string {
  const left = Math.max(0, durationMs - positionMs);
  const min = Math.round(left / 60000);
  if (min < 60) return `${Math.max(1, min)} min left`;
  return `${formatRuntime(left)} left`;
}

/** Bytes → "58.4 GB" (decimal units, as file managers show them). */
export function formatBytes(bytes: number): string {
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let v = bytes;
  let i = 0;
  while (v >= 1000 && i < units.length - 1) {
    v /= 1000;
    i++;
  }
  return `${v >= 100 || i === 0 ? Math.round(v) : v.toFixed(1)} ${units[i]}`;
}

/** Server timings: 2.570417 → "2.6 ms", 196.53346 → "197 ms". */
export function formatMs(ms: number): string {
  return ms < 10 ? `${ms.toFixed(1)} ms` : `${formatCount(Math.round(ms))} ms`;
}

/** Bits/s → "8.0 Mb/s" or "640 kb/s". */
export function formatBitrate(bps: number): string {
  if (bps >= 1e6) return `${(bps / 1e6).toFixed(bps >= 1e7 ? 0 : 1)} Mb/s`;
  return `${Math.round(bps / 1e3)} kb/s`;
}

const numberFmt = new Intl.NumberFormat();
export const formatCount = (n: number): string => numberFmt.format(n);

// Formatters are costly to make (tens of microseconds each): one of each, made on first use.
let relative: Intl.RelativeTimeFormat | null = null;
let shortDate: Intl.DateTimeFormat | null = null;

/** "3 minutes ago", "yesterday" for epoch-ms timestamps. */
export function formatAgo(epochMs: number, now = Date.now()): string {
  const rtf = (relative ??= new Intl.RelativeTimeFormat(undefined, { numeric: 'auto' }));
  const diff = (epochMs - now) / 1000;
  const abs = Math.abs(diff);
  if (abs < 60) return rtf.format(Math.round(diff), 'second');
  if (abs < 3600) return rtf.format(Math.round(diff / 60), 'minute');
  if (abs < 86400) return rtf.format(Math.round(diff / 3600), 'hour');
  if (abs < 86400 * 30) return rtf.format(Math.round(diff / 86400), 'day');
  return (shortDate ??= new Intl.DateTimeFormat(undefined, { year: 'numeric', month: 'short', day: 'numeric' })).format(epochMs);
}

// Folding runs for every title on every keystroke of a search and every layout of a
// sectioned lens (10,000 titles): remember each result, and skip Unicode
// normalisation for plain-ASCII text, which has nothing to strip.
const folded = new Map<string, string>();
const sortKeys = new Map<string, string>();
const FOLD_CACHE = 40_000;
// eslint-disable-next-line no-control-regex
const ASCII = /^[\x00-\x7f]*$/;

/** Strip accents and case for search: "Amélie" matches "amelie". */
export function foldForSearch(s: string): string {
  let f = folded.get(s);
  if (f === undefined) {
    f = ASCII.test(s) ? s.toLowerCase() : s.normalize('NFKD').replace(/\p{M}/gu, '').toLowerCase();
    if (folded.size >= FOLD_CACHE) folded.clear();
    folded.set(s, f);
  }
  return f;
}

/** Sort key that ignores a leading article ("The Matrix" sorts under M). */
export function titleSortKey(title: string): string {
  let k = sortKeys.get(title);
  if (k === undefined) {
    k = foldForSearch(title).replace(/^(the|a|an)\s+/, '');
    if (sortKeys.size >= FOLD_CACHE) sortKeys.clear();
    sortKeys.set(title, k);
  }
  return k;
}

const PATH_LABELS: Record<string, string> = {
  lan: 'Local network',
  'tailnet-direct': 'Tailscale · direct',
  'tailnet-relay': 'Tailscale · relayed (DERP)',
  tailnet: 'Tailscale',
  localhost: 'This computer',
};

/** ServerInfo.path in words: "Tailscale · direct". */
export function pathLabel(path: NetworkPath | undefined): string {
  if (!path) return '—';
  return PATH_LABELS[path] ?? path;
}
