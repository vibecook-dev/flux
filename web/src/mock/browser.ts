/**
 * Boots the in-browser mock backend (only bundled when VITE_FLUX_API is unset).
 *
 * Scenario switches, read once from the URL and remembered for the tab:
 *   ?mock_tailnet=running   skip onboarding (server already on a tailnet)
 *   ?mock_path=tailnet-direct  how the "server" says you reached it
 *   ?mock_metadata=none      no TMDB key or exports (the Libraries notice)
 *   ?mock_latency=0          simulated JSON latency in ms (default 25)
 *   ?mock_demo=1             the product site's live demo: every drive answers,
 *                            and made-up descriptions carry no note to developers
 *
 * Framed by another page (the product site), it also starts the demo bridge
 * (demo-bridge.ts) that lets that page switch lenses and follow along.
 */
import { setupWorker } from 'msw/browser';
import { METADATA_SOURCES } from '../api/types';
import { createMockDb, type Fixtures } from './db';
import { startDemoBridge } from './demo-bridge';
import fixturesUrl from './fixtures.json?url';
import { createHandlers } from './handlers';
import { detectMockMedia } from './media';

function scenario(name: string): string | null {
  const fromUrl = new URLSearchParams(location.search).get(name);
  try {
    if (fromUrl !== null) sessionStorage.setItem(name, fromUrl);
    return fromUrl ?? sessionStorage.getItem(name);
  } catch {
    return fromUrl;
  }
}

export async function startMock(): Promise<void> {
  const [fixtures, media] = await Promise.all([
    fetch(fixturesUrl).then((r) => r.json() as Promise<Fixtures>),
    detectMockMedia(),
  ]);
  const db = createMockDb(fixtures, Date.now(), { demo: scenario('mock_demo') === '1' });
  const latency = Number(scenario('mock_latency') ?? 25);
  const metadata = scenario('mock_metadata');
  const worker = setupWorker(
    ...createHandlers(db, {
      media,
      latencyMs: Number.isFinite(latency) ? latency : 25,
      tailnet: scenario('mock_tailnet') === 'running' ? 'running' : 'off',
      path: scenario('mock_path') ?? 'localhost',
      metadata: METADATA_SOURCES.find((s) => s === metadata) ?? 'tmdb',
    }),
  );
  await worker.start({
    onUnhandledRequest: 'bypass',
    quiet: true,
    // The worker comes from the build's base, so a build served below a sub-path
    // (the product site's demo at /demo/) registers its own there. It still answers
    // the page's /api/… requests: a service worker sees every request of the pages
    // it controls, whatever their path.
    serviceWorker: { url: `${import.meta.env.BASE_URL}mockServiceWorker.js`, options: { scope: import.meta.env.BASE_URL } },
  });
  startDemoBridge();
}
