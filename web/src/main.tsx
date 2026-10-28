import '@fontsource-variable/jost';
import '@fontsource-variable/archivo/wdth.css';
import '@fontsource/dm-mono/400.css';
import '@fontsource/dm-mono/500.css';
import './styles/tokens.css';
import './styles/base.css';
import './styles/glass.css';

import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { setRequestGate } from './api/client';
import { App } from './app/App';
import { CrashBoundary } from './app/Crash';
import { registerWorker } from './app/install';
import { startUpdateChecks } from './app/update-checks';
import { getClientCaps } from './caps/detect';

// Inline condition (not MOCK) so the bundler drops the mock chunk from real-server builds.
if (!import.meta.env.VITE_FLUX_API || import.meta.env.VITE_FLUX_API === 'mock') {
  // The mock backend lives in its own chunk; requests wait until it is up.
  setRequestGate(import('./mock/browser').then((m) => m.startMock()));
}

// Opened over plain HTTP on the tailnet: move to the server's HTTPS address when
// it has one (browsers keep WebGPU, and so the GPU lenses, for secure pages).
if (!window.isSecureContext && import.meta.env.VITE_FLUX_API && import.meta.env.VITE_FLUX_API !== 'mock') {
  void import('./app/secure').then(
    (m) => m.upgradeToHttps(),
    () => undefined,
  );
}

// Detect playback capabilities while the user browses, so Play needs no probing.
const idle = (cb: () => void) => ('requestIdleCallback' in window ? requestIdleCallback(cb, { timeout: 1500 }) : setTimeout(cb, 300));
idle(() => void getClientCaps());

// The installable app: a service worker on secure pages of a build for a real server
// (the mock runs on MSW's worker, and the dev server has none).
if (import.meta.env.PROD && import.meta.env.VITE_FLUX_API && import.meta.env.VITE_FLUX_API !== 'mock') registerWorker();
// Notice when flux is updated under this page.
startUpdateChecks();

const root = document.getElementById('root');
if (root) {
  createRoot(root).render(
    <StrictMode>
      <CrashBoundary place="app">
        <App />
      </CrashBoundary>
    </StrictMode>,
  );
}
