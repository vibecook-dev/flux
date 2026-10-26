/**
 * The shell's side of the live demo's bridge (src/mock/demo-bridge.ts). The
 * product site embeds a mock build of this client in an iframe and switches its
 * lens from outside: the shell says which lens it shows and hands over how to
 * pick one. Only mock builds call these (behind `MOCK`), so real builds drop
 * the module.
 */

export interface ShellReport {
  /** The lens the shell draws now. */
  lens: string;
  /** Titles in the space (0 while its first page loads). */
  items: number;
}

type Picker = (lens: string) => void;

let picker: Picker | null = null;
let report: ShellReport | null = null;
const watchers = new Set<() => void>();

function changed(): void {
  for (const w of watchers) w();
}

/** Called by the shell with how to pick a lens, and with null when it unmounts (the player replaces it). */
export function setShellPicker(fn: Picker | null): void {
  picker = fn;
  changed();
}

/** Called by the shell whenever its lens or its number of titles changes. */
export function reportShell(r: ShellReport): void {
  if (report && report.lens === r.lens && report.items === r.items) return;
  report = r;
  changed();
}

export function shellPicker(): Picker | null {
  return picker;
}

/** What the shell last reported; it stays while the player is up. */
export function shellReport(): ShellReport | null {
  return report;
}

/** Called after every report and every change of picker. */
export function watchShell(fn: () => void): () => void {
  watchers.add(fn);
  return () => watchers.delete(fn);
}
