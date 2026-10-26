/**
 * Browsers offer some features only to secure pages (WebGPU, so the GPU-drawn
 * lenses), and plain HTTP from another device isn't one, even over the
 * tailnet. When the server also serves HTTPS there (`ServerInfo.https_url`),
 * a page opened over plain HTTP on the tailnet moves to the same route at that
 * address, once this device has shown it can reach it: the name has to resolve
 * here (MagicDNS) and the certificate has to check out. Anywhere else (LAN,
 * localhost, HTTPS already) nothing happens.
 */
import { api } from '../api/client';
import type { ServerInfo } from '../api/types';

interface Here {
  protocol: string;
  pathname: string;
  search: string;
  hash: string;
}

/** The same route at the server's HTTPS address, or null to stay here. */
export function secureTarget(server: Pick<ServerInfo, 'https_url' | 'path'>, here: Here, secure: boolean): URL | null {
  if (secure || here.protocol !== 'http:' || !server.https_url || !server.path.startsWith('tailnet')) return null;
  let base: URL;
  try {
    base = new URL(server.https_url);
  } catch {
    return null;
  }
  if (base.protocol !== 'https:') return null;
  return new URL(here.pathname + here.search + here.hash, base.origin);
}

/**
 * Whether `origin` answers from this device. An opaque (no-cors) answer means
 * the name resolved, the connection opened and the certificate was valid; any
 * of those failing rejects. The page's CSP names the address for this check.
 */
export async function reachable(origin: string, timeoutMs = 3000): Promise<boolean> {
  const abort = new AbortController();
  const timer = setTimeout(() => abort.abort(), timeoutMs);
  try {
    await fetch(new URL('/api/v1/server', origin), { mode: 'no-cors', cache: 'no-store', credentials: 'omit', signal: abort.signal });
    return true;
  } catch {
    return false;
  } finally {
    clearTimeout(timer);
  }
}

/** Move this page to the server's HTTPS address, when it has one that answers from here. */
export async function upgradeToHttps(): Promise<void> {
  if (window.isSecureContext) return;
  let server: ServerInfo;
  try {
    server = await api.server();
  } catch {
    return;
  }
  const target = secureTarget(server, window.location, window.isSecureContext);
  if (target && (await reachable(target.origin))) window.location.replace(target.href);
}
