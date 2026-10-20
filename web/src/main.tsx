import '@fontsource-variable/archivo/wdth.css';
import '@fontsource-variable/atkinson-hyperlegible-next/index.css';
import '@fontsource-variable/atkinson-hyperlegible-mono/index.css';
import './styles/tokens.css';
import './styles/base.css';

import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { setRequestGate } from './api/client';
import { App } from './app/App';
import { getClientCaps } from './caps/detect';

// Inline condition (not MOCK) so the bundler drops the mock chunk from real-server builds.
if (!import.meta.env.VITE_FLUX_API || import.meta.env.VITE_FLUX_API === 'mock') {
  // The mock backend lives in its own chunk; requests wait until it is up.
  setRequestGate(import('./mock/browser').then((m) => m.startMock()));
}

// Detect playback capabilities while the user browses, so Play needs no probing.
const idle = (cb: () => void) => ('requestIdleCallback' in window ? requestIdleCallback(cb, { timeout: 1500 }) : setTimeout(cb, 300));
idle(() => void getClientCaps());

const root = document.getElementById('root');
if (root) {
  createRoot(root).render(
    <StrictMode>
      <App />
    </StrictMode>,
  );
}
