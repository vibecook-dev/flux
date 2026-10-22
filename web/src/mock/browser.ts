/**
 * Boots the in-browser mock backend (only bundled when VITE_FLUX_API is unset).
 *
 * Scenario switches, read once from the URL and remembered for the tab:
 *   ?mock_tailnet=running   skip onboarding (server already on a tailnet)
 *   ?mock_path=tailnet-direct  how the "server" says you reached it
 *   ?mock_metadata=none      no TMDB key or exports (the Libraries notice)
 *   ?mock_latency=0          simulated JSON latency in ms (default 25)
 */
import { setupWorker } from 'msw/browser';
import { METADATA_SOURCES } from '../api/types';
import { createMockDb, type Fixtures } from './db';
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
  const db = createMockDb(fixtures);
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
  await worker.start({ onUnhandledRequest: 'bypass', quiet: true });
}
