import react from '@vitejs/plugin-react';
import { defineConfig, loadEnv } from 'vite';
import { mockMediaPlugin } from './scripts/mock-media-plugin.ts';
import { pwaPlugin } from './scripts/pwa-plugin.ts';

// https://vite.dev/config/
export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, process.cwd(), '');
  const api = env.VITE_FLUX_API?.trim();
  // As src/config.ts decides it: no VITE_FLUX_API, or `mock`, is the in-browser mock backend.
  const mock = !api || api === 'mock';
  // `VITE_FLUX_API=http://host:port pnpm dev` → proxy API + media to that server (no CORS).
  const proxyTarget = api && /^https?:\/\//.test(api) ? api : undefined;
  const proxy = proxyTarget
    ? {
        '/api': { target: proxyTarget, changeOrigin: true },
        '/media': { target: proxyTarget, changeOrigin: true },
      }
    : undefined;

  return {
    plugins: [react(), mockMediaPlugin(), pwaPlugin({ mock })],
    server: { proxy },
    preview: { proxy },
    build: {
      target: 'es2022',
      // Report sizes ourselves (scripts/bundle-sizes.mjs) with gzip + brotli.
      reportCompressedSize: false,
      // hls.js (~560 kB raw) and the MSW mock are lazy chunks, never on the initial path.
      chunkSizeWarningLimit: 600,
    },
  };
});
