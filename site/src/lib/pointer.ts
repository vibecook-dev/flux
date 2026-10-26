/**
 * What the pointer does to the page: it carries a light across glass rims
 * (as in the app), across headlines ([data-light]) and across cards, and
 * buttons marked [data-magnet] lean a little towards it. All reads happen
 * before all writes, once a frame at most.
 */
import { reducedMotion } from './motion';

const clamp = (v: number, a: number, b: number) => Math.max(a, Math.min(b, v));

let px = -1000;
let py = -1000;
let target: Element | null = null;
let queued = false;
const lights: { el: HTMLElement; layer: HTMLElement }[] = [];
let magnets: HTMLElement[] = [];
let leaning = new Set<HTMLElement>();

function frame() {
  queued = false;
  const vh = window.innerHeight;

  // Reads.
  const glass = [...document.querySelectorAll<HTMLElement>('.glass')]
    .map((el) => ({ el, r: el.getBoundingClientRect() }))
    .filter(({ r }) => r.width > 0 && r.bottom > -40 && r.top < vh + 40);
  const card = target?.closest<HTMLElement>('.card, .build') ?? null;
  const cardRect = card?.getBoundingClientRect();
  const lit = lights.map((l) => ({ ...l, r: l.el.getBoundingClientRect() })).filter(({ r }) => r.bottom > 0 && r.top < vh);
  const mags = reducedMotion() ? [] : magnets.map((el) => ({ el, r: el.getBoundingClientRect() }));

  // Writes.
  for (const { el, r } of glass) {
    el.style.setProperty('--lx', `${clamp(((px - r.left) / r.width) * 100, -40, 140).toFixed(1)}%`);
    el.style.setProperty('--ly', `${clamp(((py - r.top) / r.height) * 100, -120, 220).toFixed(1)}%`);
  }
  if (card && cardRect) {
    card.style.setProperty('--mx', `${(px - cardRect.left).toFixed(0)}px`);
    card.style.setProperty('--my', `${(py - cardRect.top).toFixed(0)}px`);
  }
  for (const { layer, r } of lit) {
    layer.style.setProperty('--px', `${(px - r.left).toFixed(0)}px`);
    layer.style.setProperty('--py', `${(py - r.top).toFixed(0)}px`);
  }
  const next = new Set<HTMLElement>();
  for (const { el, r } of mags) {
    const cx = r.left + r.width / 2;
    const cy = r.top + r.height / 2;
    const dx = px - cx;
    const dy = py - cy;
    const reach = Math.max(r.width, r.height) / 2 + 36;
    if (Math.hypot(dx, dy) < reach) {
      el.style.setProperty('--tx', `${clamp(dx * 0.16, -7, 7).toFixed(1)}px`);
      el.style.setProperty('--ty', `${clamp(dy * 0.24, -6, 6).toFixed(1)}px`);
      next.add(el);
    }
  }
  for (const el of leaning) {
    if (next.has(el)) continue;
    el.style.setProperty('--tx', '0px');
    el.style.setProperty('--ty', '0px');
  }
  leaning = next;
}

function queue() {
  if (queued) return;
  queued = true;
  requestAnimationFrame(frame);
}

/** Give a headline a copy of itself that only shows where the pointer's light falls. */
function addLight(el: HTMLElement) {
  const layer = document.createElement('span');
  layer.className = 'lit-layer';
  layer.setAttribute('aria-hidden', 'true');
  for (const line of el.querySelectorAll<HTMLElement>(':scope > .line, :scope > span')) {
    const copy = document.createElement('span');
    copy.className = line.className.replace(/\blit\b/, '').trim();
    copy.textContent = line.textContent;
    layer.append(copy, ' ');
  }
  el.append(layer);
  lights.push({ el, layer });
}

export function startPointer(): void {
  // Headlines with a light: built before the words are split (they copy the text).
  document.querySelectorAll<HTMLElement>('[data-light]').forEach(addLight);
  magnets = [...document.querySelectorAll<HTMLElement>('[data-magnet]')];

  window.addEventListener(
    'pointermove',
    (e) => {
      if (e.pointerType === 'touch') return;
      px = e.clientX;
      py = e.clientY;
      target = e.target instanceof Element ? e.target : null;
      queue();
    },
    { passive: true },
  );
  // Scrolling moves things under a still pointer.
  window.addEventListener('scroll', queue, { passive: true });
  window.addEventListener('resize', queue);
  document.documentElement.addEventListener('pointerleave', () => {
    target = null;
    for (const el of leaning) {
      el.style.setProperty('--tx', '0px');
      el.style.setProperty('--ty', '0px');
    }
    leaning.clear();
  });
  // Rims start lit from above, as if the light were over the page.
  px = window.innerWidth * 0.3;
  py = -80;
  queue();
}

/** Switch a headline's light on (after its words have arrived). */
export function lightUp(el: Element, delayMs: number): void {
  setTimeout(() => el.setAttribute('data-lit-on', ''), delayMs);
}
