/**
 * The installable app (PWA) at build time:
 *
 * - **Build id.** A hash of every emitted file's name (the names carry content
 *   hashes) and of index.html, so it changes exactly when the client does.
 *   index.html gets `<meta name="flux-build" content="…">`: flux-server reads it
 *   (ServerInfo.web_build), and an open page compares the server's with its own
 *   to know flux was updated (src/app/update.ts).
 * - **Service worker.** src/sw/sw.ts becomes dist/sw.js, with
 *   `self.__FLUX_SW__ = {build, precache}` in front: the files it fetches when
 *   it installs, which is everything under assets/ but the .woff fonts (only
 *   browsers without woff2 load those, and they have no service workers).
 * - **Mock builds** (`VITE_FLUX_API` unset or `mock`: the dev mock and the
 *   product site's demo) get no worker, and builds for a real server leave out
 *   the mock's own worker (public/mockServiceWorker.js).
 */
import { createHash } from 'node:crypto';
import { rmSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import type { Plugin } from 'vite';

export const SW_FILE = 'sw.js';
const SW_SOURCE = fileURLToPath(new URL('../src/sw/sw.ts', import.meta.url));
const MOCK_WORKER = 'mockServiceWorker.js';

/** The parts of a bundle entry this plugin reads or writes (Rollup's and Rolldown's output types both fit). */
export interface BundleEntry {
  type: 'chunk' | 'asset';
  fileName: string;
  code?: string;
  source?: string | Uint8Array;
  imports?: string[];
  dynamicImports?: string[];
  exports?: string[];
}

export interface Stamp {
  build: string;
  precache: string[];
}

/** Stamps a finished bundle in place: the build id into index.html and, with a worker, the id and file list into sw.js. */
export function stampBundle(bundle: Record<string, BundleEntry>, withWorker: boolean): Stamp {
  const html = bundle['index.html'];
  if (!html || html.type !== 'asset') throw new Error('flux-pwa: the bundle has no index.html');
  const page = typeof html.source === 'string' ? html.source : new TextDecoder().decode(html.source);
  const files = Object.keys(bundle)
    .filter((f) => f !== SW_FILE && f !== 'index.html')
    .sort();
  const hash = createHash('sha256');
  for (const f of files) hash.update(`${f}\n`);
  hash.update(page);
  const build = hash.digest('hex').slice(0, 16);
  const precache = files.filter((f) => f.startsWith('assets/') && !f.endsWith('.woff') && !f.endsWith('.map'));

  if (!page.includes('</head>')) throw new Error('flux-pwa: index.html has no </head>');
  html.source = page.replace('</head>', `  <meta name="flux-build" content="${build}" />\n  </head>`);

  const sw = bundle[SW_FILE];
  if (withWorker) {
    if (!sw || sw.type !== 'chunk' || typeof sw.code !== 'string') throw new Error(`flux-pwa: ${SW_FILE} was not built`);
    const links = [...(sw.imports ?? []), ...(sw.dynamicImports ?? [])];
    if (links.length > 0 || (sw.exports ?? []).length > 0) {
      throw new Error(`flux-pwa: ${SW_FILE} must stand alone (a classic worker script), but it links ${links.join(', ') || 'exports'}`);
    }
    sw.code = `self.__FLUX_SW__ = ${JSON.stringify({ build, precache })};\n${sw.code}`;
  } else if (sw) {
    throw new Error(`flux-pwa: a mock build has a ${SW_FILE}`);
  }
  return { build, precache };
}

export function pwaPlugin({ mock }: { mock: boolean }): Plugin {
  let outDir = 'dist';
  return {
    name: 'flux-pwa',
    apply: 'build',
    enforce: 'post',
    configResolved(config) {
      outDir = resolve(config.root, config.build.outDir);
    },
    buildStart() {
      if (!mock) this.emitFile({ type: 'chunk', id: SW_SOURCE, fileName: SW_FILE });
    },
    generateBundle(_options, bundle) {
      stampBundle(bundle as unknown as Record<string, BundleEntry>, !mock);
    },
    closeBundle() {
      if (!mock) rmSync(join(outDir, MOCK_WORKER), { force: true });
    },
  };
}
