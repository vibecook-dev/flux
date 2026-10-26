import '@fontsource-variable/jost';
import '@fontsource/dm-mono/400.css';
import '@fontsource/dm-mono/500.css';
import '@fontsource-variable/archivo/wdth.css';
import './styles/tokens.css';
import './styles/base.css';
import './styles/nav.css';
import './styles/hero.css';
import './styles/lenses.css';
import './styles/anywhere.css';
import './styles/speed.css';
import './styles/agents.css';
import './styles/download.css';
import './styles/closing.css';

import { startSession } from './agents/session';
import { startNet } from './anywhere/net';
import { copy, startDownload } from './download/release';
import { startLenses } from './lenses/lenses';
import { addScene, observeReveals, pageTop, progress, reducedMotion, splitHeadings } from './lib/motion';
import { startNav } from './lib/nav';
import { lightUp, startPointer } from './lib/pointer';
import { startMetrics, startRace } from './speed/race';

declare global {
  interface Window {
    __fluxSite?: boolean;
  }
}

window.__fluxSite = true;
document.documentElement.classList.add('js');

// Headlines with a light copy their text first, then their words are split for the entrance.
startPointer();
splitHeadings();

// The hero arrives once its face is in (or after a short wait), so nothing jumps.
const fontsIn = Promise.race([document.fonts?.ready ?? Promise.resolve(), new Promise((r) => setTimeout(r, 700))]);
void fontsIn.then(() => {
  document.documentElement.setAttribute('data-ready', '');
  observeReveals();
  const title = document.querySelector('.hero-title');
  if (title) lightUp(title, 1500);
});

const closing = document.querySelector<HTMLElement>('.closing h2');
closing?.querySelector('[data-split]')?.addEventListener('reveal', () => lightUp(closing, 1300), { once: true });

startNav();
startLenses();
startNet();
startRace();
startMetrics();
startSession();
void startDownload();

document.querySelectorAll<HTMLButtonElement>('[data-copy]').forEach((b) => b.addEventListener('click', () => void copy(b, b.dataset.copy ?? '')));

// ── The hero: the wall starts after first paint; scrolling away lifts the words and lets the wall fall behind ──

const hero = document.querySelector<HTMLElement>('.hero');
const host = document.querySelector<HTMLElement>('[data-hero-wall]');
const canvas = document.querySelector<HTMLCanvasElement>('[data-wall-canvas]');
const inner = document.querySelector<HTMLElement>('.hero-inner');
if (hero && host && canvas && inner) {
  let height = 1;
  let top = 0;
  let wall: { setScroll(p: number): void } | null = null;
  addScene({
    measure() {
      top = pageTop(hero);
      height = hero.offsetHeight;
    },
    update(y) {
      const p = progress(y, top, top + height);
      wall?.setScroll(p);
      if (reducedMotion()) return;
      host.style.transform = `translate3d(0, ${(p * height * 0.35).toFixed(1)}px, 0)`;
      inner.style.transform = `translate3d(0, ${(-p * 90).toFixed(1)}px, 0)`;
      inner.style.opacity = String(1 - Math.min(1, p * 1.5));
    },
  });
  const begin = () =>
    import('./hero/wall')
      .then((m) => m.startWall(host, canvas))
      .then((w) => {
        wall = w;
        if (!w) host.setAttribute('data-on', '');
      })
      .catch(() => host.setAttribute('data-on', ''));
  if ('requestIdleCallback' in window) requestIdleCallback(() => void begin(), { timeout: 600 });
  else setTimeout(() => void begin(), 200);
}
