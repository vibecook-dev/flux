#!/usr/bin/env node
// Reports raw / gzip / brotli sizes of the built assets and what the first
// paint of Home needs (entry chunk + its static imports + CSS), then writes
// $RESULTS, default $FLUX_WORK/bench/results/flux-web-bundle.json.
//
//   pnpm build && node scripts/bundle-sizes.mjs [distDir]
import { mkdirSync, readFileSync, readdirSync, statSync, writeFileSync } from 'node:fs';
import { join, relative } from 'node:path';
import { brotliCompressSync, constants, gzipSync } from 'node:zlib';
import { resultFile } from './work.mjs';

const dist = process.argv[2] ?? new URL('../dist', import.meta.url).pathname;
const out = process.env.RESULTS ?? resultFile('flux-web-bundle.json');

const files = [];
const walk = (dir) => {
  for (const name of readdirSync(dir)) {
    const p = join(dir, name);
    if (statSync(p).isDirectory()) walk(p);
    else files.push(p);
  }
};
walk(dist);

const sizes = {};
for (const f of files) {
  if (!/\.(js|css|html|json|woff2|svg)$/.test(f)) continue;
  const buf = readFileSync(f);
  const compressible = !f.endsWith('.woff2');
  sizes[relative(dist, f)] = {
    raw: buf.length,
    gzip: compressible ? gzipSync(buf, { level: 9 }).length : buf.length,
    brotli: compressible ? brotliCompressSync(buf, { params: { [constants.BROTLI_PARAM_QUALITY]: 11 } }).length : buf.length,
  };
}

// The initial load is index.html's module script + modulepreloads + stylesheet.
const html = readFileSync(join(dist, 'index.html'), 'utf8');
const initial = [...html.matchAll(/(?:src|href)="\/([^"]+\.(?:js|css))"/g)].map((m) => m[1]);
const sum = (list, k) => list.reduce((a, f) => a + (sizes[f]?.[k] ?? 0), 0);
const js = Object.keys(sizes).filter((f) => f.endsWith('.js') && !f.startsWith('mockServiceWorker'));
const chunk = (re) => js.filter((f) => re.test(f));
const kb = (n) => Math.round((n / 1024) * 10) / 10;

const report = {
  generated_at: new Date().toISOString(),
  dist,
  initial: {
    files: initial,
    raw_kb: kb(sum(initial, 'raw')),
    gzip_kb: kb(sum(initial, 'gzip')),
    brotli_kb: kb(sum(initial, 'brotli')),
    js_gzip_kb: kb(sum(initial.filter((f) => f.endsWith('.js')), 'gzip')),
    css_gzip_kb: kb(sum(initial.filter((f) => f.endsWith('.css')), 'gzip')),
  },
  routes: Object.fromEntries(
    ['Home', 'Library', 'Item', 'Player', 'Connect', 'Admin'].map((r) => [r, kb(sum(chunk(new RegExp(`assets/${r}-`)), 'gzip'))]),
  ),
  hls_js_gzip_kb: kb(sum(chunk(/assets\/hls-/), 'gzip')),
  mock_gzip_kb: kb(sum(chunk(/assets\/browser-/), 'gzip') + sum(Object.keys(sizes).filter((f) => /fixtures-.*\.json$/.test(f)), 'gzip')),
  total_js_gzip_kb: kb(sum(js, 'gzip')),
  files: sizes,
};

mkdirSync(join(out, '..'), { recursive: true });
writeFileSync(out, JSON.stringify(report, null, 2));
const rows = Object.entries(sizes)
  .filter(([f]) => /\.(js|css)$/.test(f))
  .sort((a, b) => b[1].gzip - a[1].gzip);
for (const [f, s] of rows) console.log(`${String(kb(s.gzip)).padStart(7)} kB gz  ${String(kb(s.raw)).padStart(7)} kB raw  ${f}${initial.includes(f) ? '  (initial)' : ''}`);
console.log(`\ninitial load: ${report.initial.gzip_kb} kB gzip (${report.initial.js_gzip_kb} kB JS + ${report.initial.css_gzip_kb} kB CSS), ${report.initial.brotli_kb} kB brotli`);
console.log(`routes (gzip kB): ${JSON.stringify(report.routes)}; hls.js ${report.hls_js_gzip_kb} kB; mock ${report.mock_gzip_kb} kB`);
console.log(`wrote ${out}`);
