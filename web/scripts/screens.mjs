#!/usr/bin/env node
// Screenshots of every screen, dark + light, desktop + phone, against the mock
// backend. Uses the installed Google Chrome via playwright-core (no browser
// download); Chrome has the proprietary codecs the player needs.
//
//   pnpm build && pnpm preview --port 4199 &
//   BASE=http://localhost:4199 node scripts/screens.mjs [filter]
//
// Output: $OUT (default $FLUX_WORK/bench/web-screens)/<screen>-<theme>-<device>.png
import { mkdirSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium } from 'playwright-core';
import { fluxWork } from './work.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const BASE = process.env.BASE ?? 'http://localhost:4199';
const OUT = process.env.OUT ?? join(fluxWork('screenshots'), 'bench', 'web-screens');
const filter = process.argv[2] ?? '';
mkdirSync(OUT, { recursive: true });

// Item ids follow src/mock/db.ts: movies 10_000 + index, shows 1_000_000 + index.
const fx = JSON.parse(readFileSync(join(here, '..', 'src', 'mock', 'fixtures.json'), 'utf8'));
const movie = (t) => 10_000 + fx.movies.findIndex((m) => m[0] === t);
const show = (t) => 1_000_000 + fx.shows.findIndex((s) => s[0] === t);

const DEVICES = {
  desktop: { viewport: { width: 1440, height: 900 }, deviceScaleFactor: 1 },
  phone: { viewport: { width: 390, height: 844 }, deviceScaleFactor: 2, isMobile: true, hasTouch: true },
};

// Each screen opens with a lens chosen (stored per space, as the app does) and the first-run hint dismissed.
const SCREENS = [
  { name: 'home', path: '/' },
  {
    name: 'home-overview',
    path: '/?sort=-year',
    act: async (p) => {
      await p.keyboard.press('-');
      await p.keyboard.press('-');
      await p.waitForTimeout(700);
    },
  },
  {
    name: 'library-details',
    path: '/library/1?sort=-year',
    act: async (p) => {
      await p.keyboard.press('+');
      await p.waitForTimeout(700);
    },
  },
  { name: 'library-10k', path: '/library/3', act: async (p) => p.mouse.wheel(0, 2400) },
  { name: 'lens-flow', path: '/', lens: 'flow' },
  { name: 'lens-shelf', path: '/', lens: 'shelf' },
  { name: 'lens-gallery', path: '/?sort=-year', lens: 'gallery' },
  { name: 'lens-tonight', path: '/?sort=-added', lens: 'tonight' },
  {
    name: 'island',
    path: '/',
    act: async (p) => {
      await p.keyboard.press('/');
      await p.waitForTimeout(700);
    },
  },
  {
    name: 'search',
    path: '/',
    act: async (p) => {
      await p.waitForTimeout(300);
      await p.keyboard.type('blade', { delay: 30 });
      await p.waitForTimeout(700);
    },
  },
  {
    name: 'lens-gallery-screen',
    path: '/',
    act: async (p) => {
      await p.keyboard.press('\\');
      await p.waitForTimeout(1200);
    },
  },
  { name: 'item-movie', path: `/item/${movie('Blade Runner 2049')}` },
  {
    name: 'item-versions',
    path: `/item/${movie('Spirited Away')}`,
    act: async (p) => {
      await p.locator('article').first().evaluate((a) => a.scrollTo(0, a.scrollHeight));
      await p.waitForTimeout(400);
    },
  },
  {
    name: 'item-show',
    path: `/item/${show('Severance')}`,
    act: async (p) => {
      await p.locator('article').first().evaluate((a) => a.scrollTo(0, window.innerHeight * 0.8));
      await p.waitForTimeout(400);
    },
  },
  {
    name: 'player',
    path: `/item/${movie('Blade Runner 2049')}`,
    act: async (p) => {
      await p.locator('button', { hasText: /^(Resume|Play)/ }).first().click();
      await p.waitForTimeout(2500);
      await p.keyboard.press('i');
      await p.waitForTimeout(1500);
      await p.mouse.move(400, 300);
    },
  },
  {
    name: 'connect',
    path: '/connect',
    act: async (p) => {
      await p.getByRole('button', { name: 'Connect with Tailscale' }).click();
      await p.waitForTimeout(600);
    },
  },
  {
    name: 'connect-invite',
    path: '/connect?mock_tailnet=running',
    act: async (p) => {
      await p.getByRole('button', { name: /Create invite link/ }).click();
      await p.waitForTimeout(500);
    },
  },
  {
    name: 'admin',
    path: '/admin',
    act: async (p) => {
      await p.getByRole('button', { name: /Scan now/ }).first().click();
      await p.waitForTimeout(900);
    },
  },
  {
    // Keyboard / TV remote: arrow keys move a visible focus through the wall.
    name: 'focus-wall',
    path: '/library/1',
    act: async (p) => {
      await p.waitForTimeout(400);
      for (const k of ['ArrowRight', 'ArrowRight', 'ArrowDown']) {
        await p.keyboard.press(k);
        await p.waitForTimeout(160);
      }
      await p.waitForTimeout(900);
    },
  },
];

const browser = await chromium.launch({ channel: 'chrome', headless: true, args: ['--autoplay-policy=no-user-gesture-required'] });
let failures = 0;
for (const s of SCREENS) {
  if (filter && !s.name.includes(filter)) continue;
  for (const theme of ['dark', 'light']) {
    for (const [device, opts] of Object.entries(DEVICES)) {
      const ctx = await browser.newContext({ ...opts, colorScheme: theme, reducedMotion: 'reduce' });
      await ctx.addInitScript((lens) => {
        localStorage.setItem('flux.coach.done', 'true');
        if (lens) localStorage.setItem('flux.lens.choice', JSON.stringify({ all: lens, lib1: lens, lib3: lens }));
      }, s.lens ?? null);
      const page = await ctx.newPage();
      const errors = [];
      page.on('pageerror', (e) => errors.push(e.message));
      try {
        await page.goto(BASE + s.path, { waitUntil: 'networkidle' });
        await page.waitForTimeout(700);
        if (s.act) await s.act(page);
        await page.waitForTimeout(600);
        const file = join(OUT, `${s.name}-${theme}-${device}.png`);
        await page.screenshot({ path: file });
        console.log(`${errors.length ? 'WARN' : 'ok  '} ${file}${errors.length ? ` (${errors.join('; ')})` : ''}`);
      } catch (e) {
        failures++;
        console.log(`FAIL ${s.name}-${theme}-${device}: ${e.message.split('\n')[0]}`);
      }
      await ctx.close();
    }
  }
}
await browser.close();
process.exit(failures ? 1 : 0);
