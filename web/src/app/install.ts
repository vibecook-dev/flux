/**
 * flux as an installed app: the service worker (src/sw/) and the browser's
 * offer to install, for the corner menu.
 *
 * Chromium browsers say when flux can be installed (`beforeinstallprompt`) and
 * install it on request. Safari never says: on iPhone and iPad it's Share, then
 * Add to Home Screen; on a Mac (Safari 17 and later), File, then Add to Dock.
 */
import { useSyncExternalStore } from 'react';
import { APP_BASE } from './base';

interface InstallPromptEvent extends Event {
  prompt(): Promise<void>;
  readonly userChoice: Promise<{ outcome: 'accepted' | 'dismissed' }>;
}

/** How this browser installs flux: on request (`prompt`), by hand in Safari, or not at all (already installed, or no way). */
export type InstallWay = 'prompt' | 'ios' | 'mac-safari' | null;

let offer: InstallPromptEvent | null = null;
let installed = false;
const listeners = new Set<() => void>();
const emit = () => {
  for (const l of listeners) l();
};

/** Running as the installed app (its own window, or the home screen's). */
export function isStandalone(): boolean {
  if (typeof window === 'undefined') return false;
  return (
    matchMedia('(display-mode: standalone)').matches ||
    matchMedia('(display-mode: window-controls-overlay)').matches ||
    (navigator as Navigator & { standalone?: boolean }).standalone === true
  );
}

/**
 * The by-hand ways, from the user agent (`touchPoints` tells an iPad from a
 * Mac): every browser on iPhone and iPad adds to the Home Screen from its Share
 * menu; Safari 17 and later on a Mac adds to the Dock.
 */
export function safariWay(ua: string, touchPoints: number): InstallWay {
  if (/iPhone|iPad|iPod/.test(ua) || (/Macintosh/.test(ua) && touchPoints > 1)) return 'ios';
  const safari = /Safari\//.test(ua) && !/(Chrome|Chromium|Edg|OPR|Firefox|SamsungBrowser|DuckDuckGo)\//.test(ua);
  const version = Number(/Version\/(\d+)/.exec(ua)?.[1] ?? 0);
  return safari && /Macintosh/.test(ua) && version >= 17 ? 'mac-safari' : null;
}

export function installWay(): InstallWay {
  if (installed || isStandalone()) return null;
  if (offer) return 'prompt';
  return typeof navigator === 'undefined' ? null : safariWay(navigator.userAgent, navigator.maxTouchPoints ?? 0);
}

/** Shows the browser's install dialog (when it offered one); true once installed. */
export async function install(): Promise<boolean> {
  const o = offer;
  if (!o) return false;
  offer = null; // an offer is good for one prompt
  emit();
  await o.prompt();
  return (await o.userChoice).outcome === 'accepted';
}

export function useInstallWay(): InstallWay {
  return useSyncExternalStore(
    (cb) => {
      listeners.add(cb);
      return () => listeners.delete(cb);
    },
    installWay,
    () => null,
  );
}

if (typeof window !== 'undefined') {
  // The offer can come before React is up: hold it for the corner menu, instead of the browser's own banner.
  window.addEventListener('beforeinstallprompt', (e) => {
    e.preventDefault();
    offer = e as InstallPromptEvent;
    emit();
  });
  window.addEventListener('appinstalled', () => {
    installed = true;
    offer = null;
    emit();
  });
}

/** After the page has loaded and settled, so its first posters don't share the network with the worker's install. */
const SETTLE_MS = 4000;

/**
 * Registers the service worker, on secure pages only (this computer, or HTTPS
 * on the tailnet): browsers give workers to nothing else. Real-server builds
 * only; main.tsx decides.
 */
export function registerWorker(): void {
  if (typeof navigator === 'undefined' || !('serviceWorker' in navigator) || !window.isSecureContext) return;
  const go = () =>
    setTimeout(() => {
      void navigator.serviceWorker.register(`${APP_BASE}/sw.js`, { scope: `${APP_BASE}/`, updateViaCache: 'none' }).catch(() => undefined);
    }, SETTLE_MS);
  if (document.readyState === 'complete') go();
  else window.addEventListener('load', go, { once: true });
}
