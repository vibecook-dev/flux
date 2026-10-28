/**
 * What the update panel (UpdatePanel.tsx) and the owner's note say about
 * flux's own updates, from the server's UpdateInfo: kept apart so it can be
 * tested.
 */
import type { UpdateInfo, UpdatePhase } from '../api/types';

/** An update is under way: nothing else to start, and the panel shows its progress. */
export function updating(phase: UpdatePhase): boolean {
  return phase === 'downloading' || phase === 'verifying' || phase === 'installing' || phase === 'restarting';
}

/** Megabytes, one decimal: 12582912 → "12.6". */
export function mb(bytes: number): string {
  return (bytes / 1e6).toFixed(1);
}

/** The step an update is at, for a person. */
export function stepLine(u: Pick<UpdateInfo, 'phase' | 'progress'>): string | null {
  switch (u.phase) {
    case 'checking':
      return 'Checking for updates…';
    case 'downloading':
      return u.progress && u.progress.total > 0 ? `Downloading ${mb(u.progress.done)} of ${mb(u.progress.total)} MB` : 'Downloading…';
    case 'verifying':
      return 'Checking the download and trying the new version…';
    case 'installing':
      return 'Putting the new version in place…';
    case 'restarting':
      return 'Restarting flux…';
    default:
      return null;
  }
}

/** How far the download is, 0 to 1 (null outside it). */
export function downloaded(u: Pick<UpdateInfo, 'phase' | 'progress'>): number | null {
  if (u.phase !== 'downloading' || !u.progress || u.progress.total <= 0) return null;
  return Math.min(1, Math.max(0, u.progress.done / u.progress.total));
}

/** The panel's headline: up to date, or what is out. */
export function headline(u: Pick<UpdateInfo, 'current' | 'available'>): string {
  return u.available ? `flux ${u.available.version} is out` : `flux ${u.current} is up to date`;
}

/** Under the headline: when flux last looked, and whether it looks by itself. */
export function checkedLine(u: Pick<UpdateInfo, 'check' | 'checked_at'>, ago: (ms: number) => string): string {
  const when = u.checked_at ? `Checked ${ago(u.checked_at)}` : 'Not checked yet';
  return u.check ? `${when} · checks daily` : `${when} · daily check off`;
}

/** The owner's note: shown for a release that is out, not yet dismissed, while nothing is under way. */
export function noteFor(u: UpdateInfo | undefined, dismissed: string | null): string | null {
  if (!u?.available || updating(u.phase) || u.available.version === dismissed) return null;
  return u.available.version;
}
