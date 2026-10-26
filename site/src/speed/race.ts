/**
 * The cold-scan race, replayed from the benchmark's medians (flux 3.24 s,
 * Jellyfin 12.2 1,055 s; docs/prototype/benchmarks.md §1). It runs in real
 * time until flux is done, then fast-forwards to Jellyfin's finish. The bars
 * show each server's elapsed share of its own scan time; nothing in between
 * the start and the finish is made up.
 */
import { countUp, reducedMotion, whenVisible } from '../lib/motion';

const FLUX_S = 3.24;
const THEM_S = 1055;
const FAST = 120;

function clock(t: number): string {
  const m = Math.floor(t / 60);
  const s = t - m * 60;
  return `${String(m).padStart(2, '0')}:${s.toFixed(2).padStart(5, '0')}`;
}

export function startRace(): void {
  const race = document.querySelector<HTMLElement>('[data-race]');
  if (!race) return;
  const clockEl = race.querySelector<HTMLElement>('[data-race-clock]')!;
  const modeEl = race.querySelector<HTMLElement>('[data-race-mode]')!;
  const flux = race.querySelector<HTMLElement>('[data-lane="flux"]')!;
  const them = race.querySelector<HTMLElement>('[data-lane="them"]')!;
  const fluxFill = flux.querySelector<HTMLElement>('[data-fill]')!;
  const themFill = them.querySelector<HTMLElement>('[data-fill]')!;
  const again = race.querySelector<HTMLButtonElement>('[data-race-again]')!;

  let run = 0;

  const finish = () => {
    fluxFill.style.setProperty('--p', '1');
    themFill.style.setProperty('--p', '1');
    flux.setAttribute('data-done', '');
    them.setAttribute('data-done', '');
    clockEl.textContent = clock(THEM_S);
    modeEl.textContent = 'Finished';
    race.dataset.phase = 'done';
  };

  const start = () => {
    const id = ++run;
    flux.removeAttribute('data-done');
    them.removeAttribute('data-done');
    race.dataset.phase = 'real';
    modeEl.textContent = 'Real time';
    if (reducedMotion()) return finish();

    let sim = 0;
    let last = performance.now();
    let fastSince = -1;
    const step = (now: number) => {
      if (id !== run) return;
      const dt = Math.min(0.1, (now - last) / 1000);
      last = now;
      // Real time until a beat after flux is done, then ease up to 120× and hold.
      let speed = 1;
      if (sim > FLUX_S + 1.1) {
        if (fastSince < 0) {
          fastSince = now;
          race.dataset.phase = 'fast';
          modeEl.textContent = `Fast-forward · ${FAST}×`;
        }
        const ramp = Math.min(1, (now - fastSince) / 1300);
        speed = 1 + (FAST - 1) * ramp * ramp;
      }
      sim = Math.min(THEM_S, sim + dt * speed);
      fluxFill.style.setProperty('--p', String(Math.min(1, sim / FLUX_S)));
      themFill.style.setProperty('--p', String(sim / THEM_S));
      clockEl.textContent = clock(sim);
      if (sim >= FLUX_S && !flux.hasAttribute('data-done')) flux.setAttribute('data-done', '');
      if (sim >= THEM_S) return finish();
      requestAnimationFrame(step);
    };
    fluxFill.style.setProperty('--p', '0');
    themFill.style.setProperty('--p', '0');
    requestAnimationFrame((now) => {
      last = now;
      requestAnimationFrame(step);
    });
  };

  again.addEventListener('click', start);
  whenVisible(race, start, '0px 0px -25% 0px');
}

/** Metric cards: bars to scale (the flux bar at least a few pixels), and numbers that count up. */
export function startMetrics(): void {
  for (const card of document.querySelectorAll<HTMLElement>('[data-metric]')) {
    const f = Number(card.dataset.flux) / Number(card.dataset.them);
    const setF = () => {
      const bar = card.querySelector<HTMLElement>('.bar-them');
      const w = bar?.offsetWidth || 300;
      card.style.setProperty('--f', String(Math.max(f, 7 / w)));
    };
    setF();
    new ResizeObserver(setF).observe(card);
  }
  for (const el of document.querySelectorAll<HTMLElement>('.metric, .wide')) {
    el.addEventListener('reveal', () => el.querySelectorAll<HTMLElement>('[data-count]').forEach((n) => countUp(n)), { once: true });
  }
}
