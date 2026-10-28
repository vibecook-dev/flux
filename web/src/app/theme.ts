/** Theme setting: follow the system, or force dark/light. Resolved onto <html data-theme>. */
import { useSyncExternalStore } from 'react';

export type ThemeSetting = 'system' | 'dark' | 'light';

const KEY = 'flux.theme';
const listeners = new Set<() => void>();

function read(): ThemeSetting {
  try {
    const v = localStorage.getItem(KEY);
    return v === 'dark' || v === 'light' ? v : 'system';
  } catch {
    return 'system';
  }
}

let setting: ThemeSetting = typeof window === 'undefined' ? 'system' : read();
const media = typeof window === 'undefined' ? null : matchMedia('(prefers-color-scheme: light)');

/** The browser's (or the installed app's) title bar, in the page's colour; index.html sets it before first paint. */
const BAR = { dark: '#0b0b0e', light: '#ebeaee' } as const;

function apply() {
  const dark = setting === 'dark' || (setting === 'system' && !media?.matches);
  document.documentElement.dataset.theme = dark ? 'dark' : 'light';
  for (const m of document.querySelectorAll<HTMLMetaElement>('meta[name="theme-color"]')) m.content = BAR[dark ? 'dark' : 'light'];
  for (const l of listeners) l();
}

media?.addEventListener('change', () => {
  if (setting === 'system') apply();
});

export function setTheme(next: ThemeSetting): void {
  setting = next;
  try {
    if (next === 'system') localStorage.removeItem(KEY);
    else localStorage.setItem(KEY, next);
  } catch {
    // private mode: the choice lasts for this page only
  }
  apply();
}

export function useTheme(): { setting: ThemeSetting; resolved: 'dark' | 'light' } {
  const s = useSyncExternalStore(
    (cb) => {
      listeners.add(cb);
      return () => listeners.delete(cb);
    },
    () => setting,
    () => setting,
  );
  return { setting: s, resolved: document.documentElement.dataset.theme === 'light' ? 'light' : 'dark' };
}
