/**
 * This browser's owner-device key (flux-server's `devices`; an owner link
 * makes it). The page keeps it in its own storage, which belongs to flux's
 * address and port (a cookie would go to every server on the same host), and
 * sends it as `X-Flux-Device` with every API call. Without storage (some
 * private windows), it lasts as long as the page.
 */
export const DEVICE_HEADER = 'x-flux-device';

const STORAGE_KEY = 'flux.device';

let kept: string | null = null;

/** The key, if this browser is an owner device. */
export function deviceKey(): string | null {
  try {
    return localStorage.getItem(STORAGE_KEY) ?? kept;
  } catch {
    return kept;
  }
}

/** Keeps the key a claim answered with; `null` forgets it. */
export function setDeviceKey(key: string | null): void {
  kept = key;
  try {
    if (key) localStorage.setItem(STORAGE_KEY, key);
    else localStorage.removeItem(STORAGE_KEY);
  } catch {
    // No storage: the page keeps it until it closes.
  }
}

/** The header to send with an API call, if any. */
export function deviceHeaders(): Record<string, string> {
  const key = deviceKey();
  return key ? { [DEVICE_HEADER]: key } : {};
}
