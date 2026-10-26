/**
 * The live demo's bridge. The product site embeds a mock build of this client
 * in an iframe and drives it with postMessage; startMock starts this only when
 * the page is framed. Messages from the site, taken only from the parent window:
 *
 *   {source: 'flux-site', type: 'lens', lens: 'wall' | 'flow' | 'shelf' | 'gallery'}
 *       Pick the space's lens, as the lens bar does. A page open over the
 *       library closes first; from the player, the demo goes back to the space.
 *   {source: 'flux-site', type: 'go', to: '/changes'}
 *       Go to an app path ('/', '/library/1?sort=-year', '/item/10023', …).
 *
 * And to the site:
 *
 *   {source: 'flux-demo', type: 'ready', lens, path, search, color}
 *       Once, when the library's first titles are on screen.
 *   {source: 'flux-demo', type: 'state', lens, path, search}
 *       Whenever the lens or the route changes after that.
 *   {source: 'flux-demo', type: 'ambient', color}
 *       Whenever the room light changes (a CSS colour, as the app applies it).
 *
 * `path` is the app path ('/item/10023'), `search` its query ('' or '?sort=-year'),
 * `color` the room light or null. A mock build holds nothing but mock data, so
 * posting to '*' gives nothing away.
 */
import { ambientColor, watchAmbient } from '../app/ambient';
import { currentLocation, navigate, navigateWhenLoaded, watchLocation } from '../app/router';
import { shellPicker, shellReport, watchShell } from '../shell/embed';
import { lastSpaceHref } from '../shell/space';

type Outgoing =
  | { type: 'ready'; lens: string | null; path: string; search: string; color: string | null }
  | { type: 'state'; lens: string | null; path: string; search: string }
  | { type: 'ambient'; color: string };

let started = false;

export function startDemoBridge(): void {
  if (started || window.parent === window) return;
  started = true;
  const post = (msg: Outgoing) => window.parent.postMessage({ source: 'flux-demo', ...msg }, '*');
  const where = () => {
    const loc = currentLocation();
    return { lens: shellReport()?.lens ?? null, path: loc.pathname, search: loc.search };
  };

  let ready = false;
  let readyAsked = false;
  let lastState = '';
  // A lens asked for while the player had replaced the shell: picked once the shell is back.
  let pending: string | null = null;

  const update = () => {
    const pick = shellPicker();
    if (pending && pick) {
      const lens = pending;
      pending = null;
      pick(lens);
    }
    if (!readyAsked && (shellReport()?.items ?? 0) > 0) {
      readyAsked = true;
      // Two frames: the lens has drawn its first titles by then.
      requestAnimationFrame(() =>
        requestAnimationFrame(() => {
          ready = true;
          const now = where();
          lastState = JSON.stringify(now);
          post({ type: 'ready', ...now, color: ambientColor() });
        }),
      );
    }
    if (!ready) return;
    const now = where();
    const key = JSON.stringify(now);
    if (key === lastState) return;
    lastState = key;
    post({ type: 'state', ...now });
  };
  watchShell(update);
  watchLocation(update);
  watchAmbient((color) => {
    if (ready) post({ type: 'ambient', color });
  });
  update();

  window.addEventListener('message', (e: MessageEvent<unknown>) => {
    if (e.source !== window.parent || !e.data || typeof e.data !== 'object') return;
    const msg = e.data as { source?: unknown; type?: unknown; lens?: unknown; to?: unknown };
    if (msg.source !== 'flux-site') return;
    if (msg.type === 'lens' && typeof msg.lens === 'string' && /^[\w:.-]{1,80}$/.test(msg.lens)) {
      const pick = shellPicker();
      if (pick) pick(msg.lens);
      else {
        pending = msg.lens;
        navigate(lastSpaceHref());
      }
    } else if (msg.type === 'go' && typeof msg.to === 'string' && msg.to.startsWith('/') && !msg.to.startsWith('//')) {
      navigateWhenLoaded(msg.to);
    }
  });
}
