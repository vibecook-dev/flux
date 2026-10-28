#!/usr/bin/env node
// Renders the app icons in public/icons/ from the flux mark (the two slashes of
// public/favicon.svg) with the system Google Chrome:
//
//   icon-192.png, icon-512.png   the tile with rounded corners (manifest, purpose "any")
//   maskable-512.png             full bleed, the mark inside the 80% safe circle
//                                (Android crops it to its own shape)
//   apple-touch-icon.png         180 px, full bleed (iOS rounds the corners itself)
//
//   node scripts/gen-icons.mjs
//
// The PNGs are committed; run this again only when the mark changes.
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium } from 'playwright-core';

const here = dirname(fileURLToPath(import.meta.url));
const out = join(here, '..', 'public', 'icons');

const TILE = '#0b0b0e';
const INK = '#f3f1ec';
// The mark in its own units (favicon.svg): its box is x 2..33, y 2..26.
const MARK = `<path d="M2 26 L20 2 L27 2 L9 26 Z" fill="${INK}"/><path d="M16 26 L28 10 L33 10 L21 26 Z" fill="${INK}" fill-opacity=".45"/>`;
const MARK_W = 31;
const MARK_H = 24;

/** An icon `size` px square: the mark `markShare` of the width, centred, on a tile with corner radius `radius` (0: full bleed). */
function svg(size, markShare, radius) {
  const scale = (size * markShare) / MARK_W;
  const x = (size - MARK_W * scale) / 2 - 2 * scale;
  const y = (size - MARK_H * scale) / 2 - 2 * scale;
  return `<svg xmlns="http://www.w3.org/2000/svg" width="${size}" height="${size}" viewBox="0 0 ${size} ${size}">
  <rect width="${size}" height="${size}" rx="${radius}" fill="${TILE}"/>
  <g transform="translate(${x.toFixed(3)} ${y.toFixed(3)}) scale(${scale.toFixed(5)})">${MARK}</g>
</svg>`;
}

// favicon.svg: a 64 px tile, rx 14, the mark 56.5% of the width. The "any" icons keep it.
const ICONS = [
  { file: 'icon-192.png', size: 192, mark: 0.565, radius: (192 * 14) / 64 },
  { file: 'icon-512.png', size: 512, mark: 0.565, radius: (512 * 14) / 64 },
  // The mark's diagonal stays well inside the safe circle (80% of the width).
  { file: 'maskable-512.png', size: 512, mark: 0.46, radius: 0 },
  { file: 'apple-touch-icon.png', size: 180, mark: 0.5, radius: 0 },
];

mkdirSync(out, { recursive: true });
const browser = await chromium.launch({ channel: 'chrome', headless: true });
try {
  // One roomy window for every size (Chrome won't make a window as small as 180 px).
  const page = await browser.newPage({ deviceScaleFactor: 1, viewport: { width: 640, height: 640 } });
  for (const icon of ICONS) {
    await page.setContent(`<!doctype html><style>html,body{margin:0;background:transparent}svg{display:block}</style>${svg(icon.size, icon.mark, icon.radius)}`);
    const png = await page.locator('svg').screenshot({ omitBackground: true });
    writeFileSync(join(out, icon.file), png);
    console.log(`${icon.file}  ${icon.size}px  ${png.length} bytes`);
  }
} finally {
  await browser.close();
}
