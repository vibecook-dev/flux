/**
 * Lenses: the app itself, living in the page. The web client, built in mock
 * mode and served at <base>demo/, runs in an iframe; the tabs below it drive it
 * through its bridge (web/src/mock/demo-bridge.ts), which tells us back which
 * lens and page it shows and its room light, which becomes the glow under it.
 *
 * - On a computer it loads while the hero is being read, and it rises flat as
 *   you scroll to it.
 * - While you watch, the lenses take turns, the showing tab counting down to
 *   the next; the first touch stops that for good.
 * - There is no overlay: the pointer goes straight into the app. Until you
 *   click into it, the wheel over it scrolls the page (no scroll trap); after,
 *   it is the app's, until you click elsewhere or scroll it out of view.
 * - On a touch screen a tap opens it full screen.
 */
import { addScene, easeOut, pageTop, progress, reducedMotion } from '../lib/motion';

const TURNS = ['wall', 'flow', 'shelf', 'gallery', 'corkboard'];
const DWELL = 7000;

type FromApp =
  | { source: 'flux-demo'; type: 'ready'; lens: string | null; path: string; search: string; color: string | null }
  | { source: 'flux-demo'; type: 'state'; lens: string | null; path: string; search: string }
  | { source: 'flux-demo'; type: 'ambient'; color: string };

export function startLenses(): void {
  const stage = document.querySelector<HTMLElement>('[data-app-stage]');
  const frame = document.querySelector<HTMLElement>('[data-app-frame]');
  const iframe = document.querySelector<HTMLIFrameElement>('[data-app-iframe]');
  const glow = document.querySelector<HTMLElement>('[data-app-glow]');
  const tabsRoot = document.querySelector<HTMLElement>('[data-lens-tabs]');
  const line = document.querySelector<HTMLElement>('[data-lens-line]');
  const tap = document.querySelector<HTMLButtonElement>('[data-app-tap]');
  const expand = document.querySelector<HTMLButtonElement>('[data-app-full]');
  const close = document.querySelector<HTMLButtonElement>('[data-app-close]');
  if (!stage || !frame || !iframe || !glow || !tabsRoot || !line || !tap || !expand || !close) return;
  const tabs = [...tabsRoot.querySelectorAll<HTMLButtonElement>('[data-lens]')];

  const src = import.meta.env.VITE_DEMO_URL ?? `${import.meta.env.BASE_URL}demo/?mock_tailnet=running&mock_demo=1&sort=-added`;
  const coarse = matchMedia('(pointer: coarse)');
  let loaded = false;
  let ready = false;
  let engaged = false;
  let full = false;
  let current = 'wall';
  let queue: object[] = [];

  // ── loading ──

  const load = () => {
    if (loaded) return;
    loaded = true;
    // The app shares this page's storage: start it dark, without first-run hints, on the Wall's Grid.
    try {
      if (localStorage.getItem('flux.theme') === null) localStorage.setItem('flux.theme', 'dark');
      if (localStorage.getItem('flux.coach.done') === null) localStorage.setItem('flux.coach.done', 'true');
      if (localStorage.getItem('flux.wall.level') === null) localStorage.setItem('flux.wall.level', '1');
    } catch {
      // Storage blocked: the app still runs, with its defaults.
    }
    iframe.addEventListener('load', onFrameLoad);
    iframe.src = src;
  };

  const markReady = () => {
    if (ready) return;
    ready = true;
    frame.setAttribute('data-ready', '');
    for (const m of queue) send(m);
    queue = [];
    clock();
  };

  const send = (msg: object) => {
    if (!ready) {
      queue.push(msg);
      load();
      return;
    }
    iframe.contentWindow?.postMessage({ source: 'flux-site', ...msg }, '*');
  };

  if ('IntersectionObserver' in window) {
    const io = new IntersectionObserver(
      (entries) => {
        if (entries.some((e) => e.isIntersecting)) {
          io.disconnect();
          load();
        }
      },
      { rootMargin: '120% 0px' },
    );
    io.observe(stage);
  } else load();
  // A same-origin frame boots on this page's own thread: on a computer, start it while the
  // visitor is still reading the hero, so it never boots in the middle of a scroll.
  if (!coarse.matches) {
    const soon = () => ('requestIdleCallback' in window ? requestIdleCallback(load, { timeout: 2000 }) : setTimeout(load, 300));
    if (document.readyState === 'complete') setTimeout(soon, 2200);
    else window.addEventListener('load', () => setTimeout(soon, 2200), { once: true });
  }

  // ── tabs ──

  let shownLine = line.textContent ?? '';
  const select = (lens: string | null) => {
    if (lens) current = lens;
    for (const t of tabs) t.setAttribute('aria-selected', String(t.dataset.lens === lens));
    const text = tabs.find((t) => t.dataset.lens === lens)?.querySelector('.tab-line')?.textContent;
    if (!text || text === shownLine) return;
    shownLine = text;
    line.setAttribute('data-swap', '');
    setTimeout(() => {
      line.textContent = text;
      line.removeAttribute('data-swap');
    }, 180);
  };

  for (const t of tabs) {
    t.addEventListener('click', () => {
      stopTurns();
      const lens = t.dataset.lens ?? 'wall';
      select(lens);
      send({ type: 'lens', lens });
    });
  }
  tabsRoot.addEventListener('keydown', (e) => {
    if (e.key !== 'ArrowLeft' && e.key !== 'ArrowRight') return;
    const i = tabs.findIndex((b) => b === document.activeElement);
    if (i < 0) return;
    const next = tabs[(i + (e.key === 'ArrowRight' ? 1 : tabs.length - 1)) % tabs.length];
    next.focus();
    next.click();
    e.preventDefault();
  });

  // ── taking turns ──

  let turning = !reducedMotion();
  let inView = false;
  let running = false;
  let timer = 0;
  let bar: Animation | null = null;
  let lastScroll = 0;
  window.addEventListener('scroll', () => (lastScroll = performance.now()), { passive: true });

  /** Start or stop the countdown to the next lens, as the conditions say. */
  function clock() {
    const should = turning && inView && ready && !full && !engaged;
    if (should === running) return;
    running = should;
    tabsRoot!.toggleAttribute('data-cycling', should);
    clearTimeout(timer);
    bar?.cancel();
    bar = null;
    if (should) countdown();
  }

  function countdown() {
    const fill = tabs.find((t) => t.dataset.lens === current)?.querySelector<HTMLElement>('.tab-progress i');
    bar?.cancel();
    bar = fill?.animate([{ transform: 'scaleX(0)' }, { transform: 'scaleX(1)' }], { duration: DWELL, easing: 'linear', fill: 'forwards' }) ?? null;
    timer = window.setTimeout(next, DWELL);
  }

  function next() {
    // Never swap lenses mid-scroll: the app shares this page's main thread.
    if (performance.now() - lastScroll < 700) {
      timer = window.setTimeout(next, 300);
      return;
    }
    const lens = TURNS[(TURNS.indexOf(current) + 1) % TURNS.length];
    select(lens);
    send({ type: 'lens', lens });
    countdown();
  }

  function stopTurns() {
    turning = false;
    clock();
  }

  // ── what the app says ──

  window.addEventListener('message', (e: MessageEvent<FromApp>) => {
    if (e.source !== iframe.contentWindow || e.data?.source !== 'flux-demo') return;
    const m = e.data;
    if (m.type === 'ready') {
      markReady();
      if (m.color) glow.style.setProperty('--glow', m.color);
      if (m.lens) select(TURNS.includes(m.lens) ? m.lens : null);
    } else if (m.type === 'state') {
      if (m.lens && m.lens !== current) select(TURNS.includes(m.lens) ? m.lens : null);
    } else if (m.type === 'ambient') {
      glow.style.setProperty('--glow', m.color);
    }
  });

  // ── the pointer goes straight in; the wheel waits for a click ──

  function engage(on: boolean) {
    if (on === engaged) return;
    engaged = on;
    frame!.toggleAttribute('data-engaged', on);
    if (on) stopTurns();
  }

  function onFrameLoad() {
    // Without the app's bridge the frame never says it's ready; show it anyway.
    setTimeout(markReady, 9000);
    let w: Window | null = null;
    try {
      w = iframe!.contentWindow;
      w?.addEventListener(
        'wheel',
        (e: WheelEvent) => {
          if (engaged || full) return;
          // A pinch zooms the Wall: that's the app's, and counts as a touch.
          if (e.ctrlKey) return engage(true);
          // Before anything in the app sees it, the wheel scrolls this page instead.
          e.stopImmediatePropagation();
          e.preventDefault();
          const dy = e.deltaY * (e.deltaMode === 1 ? 16 : e.deltaMode === 2 ? window.innerHeight : 1);
          window.scrollBy({ top: dy, behavior: Math.abs(dy) >= 60 ? 'smooth' : 'instant' });
        },
        { capture: true, passive: false },
      );
      w?.addEventListener('pointerdown', () => engage(true), true);
      w?.addEventListener('keydown', () => engage(true), true);
    } catch {
      // A frame from another origin (VITE_DEMO_URL): it keeps the wheel itself.
    }
  }

  document.addEventListener('pointerdown', (e) => {
    if (!full && !frame.contains(e.target as Node)) engage(false);
  });

  if ('IntersectionObserver' in window) {
    new IntersectionObserver(
      (entries) => {
        const r = entries[entries.length - 1].intersectionRatio;
        inView = r >= 0.45;
        if (r < 0.25) engage(false);
        clock();
      },
      { threshold: [0, 0.25, 0.45, 0.7] },
    ).observe(frame);
  }

  // ── full screen: the same iframe, fixed to the window, revealed by a clip ──

  const backdrop = document.createElement('div');
  backdrop.className = 'app-backdrop';
  backdrop.hidden = true;
  document.body.append(backdrop);

  const clipFor = (r: DOMRect) =>
    `inset(${r.top.toFixed(1)}px ${(window.innerWidth - r.right).toFixed(1)}px ${(window.innerHeight - r.bottom).toFixed(1)}px ${r.left.toFixed(1)}px round 22px)`;

  function enterFull() {
    if (full) return;
    load();
    full = true;
    stopTurns();
    const from = frame!.getBoundingClientRect();
    stage!.style.height = `${stage!.offsetHeight}px`;
    frame!.style.transform = 'none';
    frame!.setAttribute('data-full', '');
    document.documentElement.setAttribute('data-app-full', '');
    backdrop.hidden = false;
    if (!reducedMotion()) {
      frame!.style.clipPath = clipFor(from);
      frame!.getBoundingClientRect();
      frame!.style.transition = 'clip-path 0.75s var(--out)';
      frame!.style.clipPath = 'inset(0 0 0 0 round 0px)';
    }
    requestAnimationFrame(() => backdrop.setAttribute('data-on', ''));
    iframe!.focus({ preventScroll: true });
  }

  function exitFull() {
    if (!full) return;
    const finish = () => {
      full = false;
      frame!.removeAttribute('data-full');
      frame!.style.transition = '';
      frame!.style.clipPath = '';
      frame!.style.transform = '';
      document.documentElement.removeAttribute('data-app-full');
      stage!.style.height = '';
      backdrop.hidden = true;
      sceneUpdate(window.scrollY, window.innerHeight);
      clock();
    };
    backdrop.removeAttribute('data-on');
    // Where the frame sits on the page again.
    const r = stage!.getBoundingClientRect();
    const phone = window.innerWidth <= 640;
    const w = phone ? r.width : Math.min(1280, r.width, window.innerHeight * 0.78 * 1.6);
    const h = phone ? Math.min(r.width * (15 / 9), window.innerHeight * 0.72) : w / 1.6;
    const to = new DOMRect(r.left + (r.width - w) / 2, r.top + (r.height - h) / 2, w, h);
    if (reducedMotion()) return finish();
    frame!.style.transition = 'clip-path 0.55s var(--out)';
    frame!.style.clipPath = clipFor(to);
    setTimeout(finish, 560);
  }

  tap.addEventListener('click', enterFull);
  expand.addEventListener('click', enterFull);
  close.addEventListener('click', exitFull);
  backdrop.addEventListener('click', exitFull);
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && full) exitFull();
  });

  // ── scroll: the frame rises flat as it arrives ──

  let top = 0;
  const sceneUpdate = (y: number, vh: number) => {
    if (full) return;
    if (reducedMotion()) {
      frame.style.setProperty('--tilt', '0deg');
      return;
    }
    const p = easeOut(progress(y, top - vh * 1.0, top - vh * 0.22));
    frame.style.setProperty('--tilt', `${(1 - p) * 24}deg`);
    frame.style.setProperty('--lift', `${(1 - p) * 70}px`);
    frame.style.setProperty('--scale', `${0.86 + 0.14 * p}`);
  };
  addScene({
    measure: () => {
      top = pageTop(stage);
    },
    update: sceneUpdate,
  });
}
