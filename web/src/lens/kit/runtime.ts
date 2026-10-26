/**
 * The lens kit: a lens as data. Its layout code runs in a Worker, its look is
 * CSS scoped to the lens, and its items come from an HTML template that is
 * sanitised before it touches the page.
 * flux's own lenses are full modules; kit lenses are what anyone, or an agent,
 * can write in an afternoon and share as a JSON file.
 *
 * Built for big libraries: only the titles within `window` of the position get
 * DOM, and only those are sent to the worker (each keeps its global index `i`).
 *
 * A lens can come from anyone, and a blob: worker shares the page's origin, so
 * left alone its code could call flux's own API as you (flux trusts the
 * network, and the worker can send X-Flux-Request). "No DOM, no network" is
 * enforced in layers:
 * - the worker script starts with a lockdown (LOCKED below): before the lens's
 *   code runs, fetch, XMLHttpRequest, WebSocket, EventSource, importScripts,
 *   nested workers, storage, navigator and location are gone from its scope;
 *   `validateSpec` refuses layout code that uses `import`;
 * - flux's Content-Security-Policy (crates/flux-server/src/web.rs) applies to
 *   the worker too, since a blob: worker inherits it: no other origins
 *   (`connect-src 'self'`), no eval. The dev server sends no CSP, so there the
 *   lockdown is all there is;
 * - the page takes only the frame it asked for from the worker, capped to the
 *   window and with every number checked (`readFrame`), so a lens that posts
 *   its own messages can't hang or break the page.
 *
 * The CSP also shapes the rest: styles come from constructed stylesheets (no
 * inline <style> or style attributes), and the worker runs from a blob: URL
 * (`worker-src 'self' blob:`).
 */
import type { Id, ItemSummary } from '../../api/types';
import { artElement, artLabel, resizeArt } from '../art';
import { clamp, el } from '../dom';
import { paletteOf } from '../palette';
import { isNew, isWatchedItem, progressFraction } from '../sections';
import type { Dir, LensContext, LensModule, LensView } from '../types';
import styles from './kit.module.css';

export interface KitSpec {
  id: string;
  name: string;
  blurb: string;
  /** Which scroll axis moves through the titles (arrow keys always step). */
  input: 'x' | 'y' | 'none';
  /** Pixels of drag or scroll per title. */
  step: number;
  /** Ask for a frame every tick (layout uses view.t) instead of only when something moved. */
  animated: boolean;
  /** Titles kept either side of the position (DOM and worker). */
  window: number;
  /** The look; every selector starts with `.lens`. */
  css: string;
  /** HTML for one title; `{{title}}` etc., and `<div class="art"></div>` for the poster. */
  item: string;
  /** Static HTML behind the titles. */
  scene: string;
  /** Source of `function layout(items, view)`. */
  layout: string;
}

export type SpecResult = { ok: true; spec: KitSpec } | { ok: false; error: string };

const LIMITS = { name: 28, blurb: 160, css: 14000, item: 4000, scene: 4000, layout: 16000 };

function fnv(s: string): number {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return h >>> 0;
}

export function slug(s: string): string {
  return s
    .toLowerCase()
    .normalize('NFKD')
    .replace(/\p{M}/gu, '')
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-|-$/g, '')
    .slice(0, 24);
}

/**
 * Check a lens and fill in defaults. Errors say what to change, in plain words.
 * `id`: keep this id (flux's examples); otherwise one is derived from the name and code.
 */
export function validateSpec(raw: unknown, { id }: { id?: string } = {}): SpecResult {
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) return { ok: false, error: 'A lens is a JSON object with a name, css, item and layout.' };
  const r = raw as Record<string, unknown>;
  const str = (v: unknown, max: number) => (typeof v === 'string' ? v : '').slice(0, max);
  const layout = str(r.layout, LIMITS.layout + 1);
  if (!layout.trim()) return { ok: false, error: 'This lens has no layout. It needs "layout": "function layout(items, view) { … }".' };
  if (layout.length > LIMITS.layout) return { ok: false, error: `The layout code is longer than ${LIMITS.layout.toLocaleString('en')} characters. Ask for a simpler lens.` };
  if (!/function\s+layout\s*\(|(?:const|let|var)\s+layout\s*=/.test(layout)) return { ok: false, error: 'The layout code must define a function called layout(items, view).' };
  // import() is syntax, so the worker's lockdown can't take it away (and it would fetch).
  if (/\bimport\b/.test(layout)) return { ok: false, error: 'The layout code can’t load other code (import). Everything it uses must be in the lens.' };
  const css = str(r.css, LIMITS.css + 1);
  if (css.length > LIMITS.css) return { ok: false, error: `The CSS is longer than ${LIMITS.css.toLocaleString('en')} characters.` };
  const name = str(r.name, LIMITS.name).trim() || 'My lens';
  const input = r.input === 'y' || r.input === 'none' ? r.input : 'x';
  const step = clamp(Number(r.step) || 160, 30, 600);
  const windowSize = clamp(Math.round(Number(r.window) || 40), 8, 150);
  const spec: KitSpec = {
    id: id ?? `my-${slug(name) || 'lens'}-${(fnv(name + layout) % 99991).toString(36)}`,
    name,
    blurb: str(r.blurb, LIMITS.blurb).trim() || `A lens called ${name}.`,
    input,
    step,
    animated: r.animated === true,
    window: windowSize,
    css,
    item: str(r.item, LIMITS.item) || '<div class="art"></div>',
    scene: str(r.scene, LIMITS.scene),
    layout,
  };
  return { ok: true, spec };
}

const BAD_TAGS = new Set([
  'SCRIPT', 'IFRAME', 'OBJECT', 'EMBED', 'LINK', 'META', 'STYLE', 'BASE', 'FORM', 'INPUT', 'BUTTON', 'TEXTAREA', 'SELECT', 'OPTION',
  'VIDEO', 'AUDIO', 'SOURCE', 'TRACK', 'IMG', 'PICTURE', 'IMAGE', 'USE', 'FOREIGNOBJECT', 'TEMPLATE', 'NOSCRIPT', 'FRAME', 'FRAMESET',
  'PORTAL', 'A', 'AREA', 'MAP', 'CANVAS', 'DIALOG', 'SLOT', 'ANIMATE', 'SET', 'ANIMATEMOTION', 'ANIMATETRANSFORM', 'HANDLER', 'SVG:SCRIPT',
]);
const KEEP_ATTR = /^(class|role|title|aria-[\w-]+|data-[\w-]+|viewbox|d|cx|cy|r|rx|ry|x|y|x1|y1|x2|y2|width|height|points|fill|stroke|stroke-width|stroke-linecap|stroke-linejoin|opacity|fill-opacity|stroke-opacity|transform|preserveaspectratio)$/i;

/**
 * Keep markup inert: no scripts, handlers, links, loads or inline styles (flux's
 * CSP forbids those anyway; style belongs in the lens CSS). Only plain markup,
 * classes and simple SVG shapes survive.
 */
export function sanitizeHTML(html: string): string {
  const t = document.createElement('template');
  t.innerHTML = String(html ?? '');
  const walk = (node: Element | DocumentFragment) => {
    for (const c of [...node.children]) {
      if (BAD_TAGS.has(c.tagName.toUpperCase())) {
        c.remove();
        continue;
      }
      for (const a of [...c.attributes]) {
        const v = a.value.toLowerCase().replace(/\s+/g, '');
        if (!KEEP_ATTR.test(a.name) || v.includes('javascript:') || v.includes('url(')) c.removeAttribute(a.name);
      }
      walk(c);
    }
  };
  walk(t.content);
  return t.innerHTML;
}

/** Scope CSS to one lens root, keep its keyframes private, and drop anything that could load or leak. */
export function scopeCSS(css: string, id: string): string {
  const root = `[data-kit="${id}"]`;
  let src = String(css ?? '')
    .replace(/\/\*[\s\S]*?\*\//g, '')
    .replace(/@import[^;]*;?/gi, '')
    .replace(/@font-face\s*\{[^}]*\}/gi, '')
    .replace(/@namespace[^;]*;?/gi, '')
    .replace(/url\s*\([^)]*\)/gi, 'none')
    .replace(/image-set\s*\(/gi, 'none(')
    .replace(/expression\s*\(/gi, '(')
    .replace(/-moz-binding|behavior\s*:/gi, '');
  const names = [...src.matchAll(/@keyframes\s+([\w-]+)/g)].map((m) => m[1]);
  for (const n of new Set(names)) src = src.replace(new RegExp(`(?<![\\w-])${n.replace(/[-]/g, '\\-')}(?![\\w-])`, 'g'), `k-${id}-${n}`);
  const out: string[] = [];
  const rules = (text: string, sink: string[]) => {
    let i = 0;
    while (i < text.length) {
      const open = text.indexOf('{', i);
      if (open < 0) break;
      const head = text.slice(i, open).trim().replace(/^[;\s]+/, '');
      let depth = 1;
      let j = open + 1;
      while (j < text.length && depth) {
        if (text[j] === '{') depth++;
        else if (text[j] === '}') depth--;
        j++;
      }
      const body = text.slice(open + 1, j - 1);
      if (/^@(media|supports|container)\b/i.test(head)) {
        const inner: string[] = [];
        rules(body, inner);
        if (inner.length) sink.push(`${head}{${inner.join('')}}`);
      } else if (/^@keyframes\b/i.test(head)) {
        sink.push(`${head}{${body}}`);
      } else if (!head.startsWith('@') && head) {
        const sel = head
          .split(',')
          .map((s) => s.trim())
          .filter(Boolean)
          .map((s) => {
            if (s === '.lens' || s.startsWith('.lens ') || s.startsWith('.lens.') || s.startsWith('.lens:') || s.startsWith('.lens>') || s.startsWith('.lens[')) return root + s.slice(5);
            if (/^(:root|html|body)\b/.test(s)) return null;
            return `${root} ${s}`;
          })
          .filter((s): s is string => s !== null);
        if (sel.length) sink.push(`${sel.join(',')}{${body.replace(/[{}]/g, '')}}`);
      }
      i = j;
    }
  };
  rules(src, out);
  return out.join('\n');
}

/** What the worker sees of a title (no URLs, no account data). */
interface WorkerItem {
  i: number;
  id: Id;
  title: string;
  year: number | null;
  decade: number | null;
  kind: string;
  seasons: number;
  badges: string[];
  progress: number;
  watched: boolean;
  isNew: boolean;
  available: boolean;
  hue: number;
  sat: number;
  lum: number;
}

/** One title's place in a frame, as the page applies it. */
export interface Out {
  x: number;
  y: number;
  z: number;
  rx: number;
  ry: number;
  rz: number;
  s: number;
  o: number;
  zIndex: number | null;
  cls: string;
}

/**
 * What the lockdown takes from the worker's scope before the lens's code runs:
 * everything that reaches the network, storage, other code or the page's
 * address. postMessage and onmessage stay (the loop needs them).
 */
const LOCKED = [
  'fetch', 'XMLHttpRequest', 'WebSocket', 'WebSocketStream', 'WebTransport', 'EventSource', 'importScripts',
  'Request', 'Response', 'Headers', 'indexedDB', 'caches', 'cookieStore', 'BroadcastChannel', 'Worker', 'SharedWorker',
  'navigator', 'location', 'Notification', 'RTCPeerConnection', 'RTCDataChannel', 'fetchLater',
  'requestFileSystem', 'webkitRequestFileSystem', 'requestFileSystemSync', 'webkitRequestFileSystemSync',
];

// On a worker's [Global] scope these are own properties of self, so redefining them as undefined (and not
// configurable) removes them for good; anything an engine keeps on a prototype is deleted there first.
const WORKER_PRELUDE = `
for (const k of ${JSON.stringify(LOCKED)}) {
  for (let o = Object.getPrototypeOf(self); o; o = Object.getPrototypeOf(o)) { try { delete o[k]; } catch (e) {} }
  try { Object.defineProperty(self, k, { value: undefined, writable: false, configurable: false }); } catch (e) {}
}
let ITEMS = [];
const clamp = (v, a, b) => Math.max(a, Math.min(b, v));
const lerp = (a, b, t) => a + (b - a) * t;
const rand = (i, salt = 0) => { const x = Math.sin((i + 1) * 12.9898 + salt * 78.233) * 43758.5453; return x - Math.floor(x); };
const __layout = (function () {
`;
const WORKER_LOOP = `
;return typeof layout === 'function' ? layout : null;
})();
const __num = (v, d) => (typeof v === 'number' && isFinite(v) ? v : d);
if (!__layout) throw new Error('The code does not define layout(items, view).');
self.onmessage = (e) => {
  const m = e.data;
  if (m.items) ITEMS = m.items;
  let out;
  try { out = __layout(ITEMS, m.view); }
  catch (err) { self.postMessage({ type: 'error', message: String((err && err.message) || err) }); return; }
  if (!Array.isArray(out)) { self.postMessage({ type: 'error', message: 'layout() must return an array, one object per title.' }); return; }
  self.postMessage({ type: 'frame', seq: m.seq, first: m.view.first, out: out.map((o) => o && typeof o === 'object' ? {
    x: __num(o.x, 0), y: __num(o.y, 0), z: __num(o.z, 0), rx: __num(o.rx, 0), ry: __num(o.ry, 0), rz: __num(o.rz, 0),
    s: __num(o.s, 1), o: __num(o.o, 1), zIndex: typeof o.zIndex === 'number' && isFinite(o.zIndex) ? o.zIndex : null,
    cls: typeof o.cls === 'string' ? o.cls.slice(0, 60) : '' } : { o: 0 }) });
};
self.postMessage({ type: 'ready' });
`;

/** The worker's whole script: the lockdown and helpers, the lens's layout code, then the loop that runs it and cleans what it returns. */
export function workerSource(layout: string): string {
  return `${WORKER_PRELUDE}\n${layout}\n${WORKER_LOOP}`;
}

const finite = (v: unknown, d: number): number => (typeof v === 'number' && Number.isFinite(v) ? v : d);
const isList = (v: unknown): v is readonly unknown[] => Array.isArray(v);

function cleanOut(v: unknown): Out {
  if (!v || typeof v !== 'object') return { x: 0, y: 0, z: 0, rx: 0, ry: 0, rz: 0, s: 1, o: 0, zIndex: null, cls: '' };
  const o = v as Partial<Record<keyof Out, unknown>>;
  return {
    x: finite(o.x, 0),
    y: finite(o.y, 0),
    z: finite(o.z, 0),
    rx: finite(o.rx, 0),
    ry: finite(o.ry, 0),
    rz: finite(o.rz, 0),
    s: finite(o.s, 1),
    o: finite(o.o, 1),
    zIndex: typeof o.zIndex === 'number' && Number.isFinite(o.zIndex) ? o.zIndex : null,
    cls: typeof o.cls === 'string' ? o.cls.slice(0, 60) : '',
  };
}

/**
 * A frame from the worker, checked again on the page: lens code can post its
 * own messages, so take only the frame asked for (by `seq`), at most `max`
 * entries, each with finite numbers. Anything else is null.
 */
export function readFrame(m: unknown, seq: number, max: number): { first: number; out: Out[] } | null {
  if (!m || typeof m !== 'object') return null;
  const f = m as { type?: unknown; seq?: unknown; first?: unknown; out?: unknown };
  if (f.type !== 'frame' || f.seq !== seq || !isList(f.out)) return null;
  return { first: typeof f.first === 'number' && Number.isSafeInteger(f.first) ? f.first : 0, out: f.out.slice(0, max).map(cleanOut) };
}

const FIELDS = new Set(['title', 'year', 'years', 'meta', 'kind', 'decade', 'index', 'number', 'seasons', 'badges', 'show', 'director', 'runtime', 'genre', 'country', 'logline']);

function fieldsOf(it: ItemSummary, i: number): Record<string, string> {
  const seasons = it.kind === 'show' && it.child_count ? `${it.child_count} season${it.child_count === 1 ? '' : 's'}` : '';
  const kind = it.kind === 'movie' ? 'Film' : it.kind === 'show' ? 'Show' : it.kind === 'episode' ? 'Episode' : it.kind === 'season' ? 'Season' : 'Extra';
  return {
    title: it.title,
    year: it.year ? String(it.year) : '',
    years: it.year ? String(it.year) : '',
    meta: [it.year ? String(it.year) : '', seasons, ...it.badges.slice(0, 2)].filter(Boolean).join(' · '),
    kind,
    decade: it.year ? `${Math.floor(it.year / 10) * 10}s` : '',
    index: String(i + 1),
    number: String(i + 1).padStart(2, '0'),
    seasons,
    badges: it.badges.join(' · '),
    show: it.parent_title ?? '',
  };
}

function workerItem(it: ItemSummary, i: number): WorkerItem {
  const p = paletteOf(it);
  return {
    i,
    id: it.id,
    title: it.title,
    year: it.year,
    decade: it.year ? Math.floor(it.year / 10) * 10 : null,
    kind: it.kind,
    seasons: it.kind === 'show' ? (it.child_count ?? 0) : 0,
    badges: it.badges,
    progress: progressFraction(it) ?? 0,
    watched: isWatchedItem(it),
    isNew: isNew(it),
    available: it.available,
    hue: Math.round(p.hue),
    sat: p.sat,
    lum: p.lum,
  };
}

const failures = new WeakMap<LensView, { msg: string | null; subs: Set<(msg: string) => void> }>();

/** Why a kit lens stopped (or null while it runs). */
export function kitFailure(view: LensView): string | null {
  return failures.get(view)?.msg ?? null;
}

/** Be told when a kit lens stops (its preview can then offer a fix). */
export function onKitFailure(view: LensView, cb: (msg: string) => void): () => void {
  const f = failures.get(view);
  if (!f) return () => undefined;
  if (f.msg) cb(f.msg);
  f.subs.add(cb);
  return () => f.subs.delete(cb);
}

class KitView implements LensView {
  private readonly root: HTMLElement;
  private readonly ctx: LensContext;
  private readonly spec: KitSpec;
  private readonly mini: boolean;
  private readonly scene: HTMLDivElement;
  private readonly stage: HTMLDivElement;
  private sheet: CSSStyleSheet | null = null;
  private worker: Worker | null = null;
  private ready = false;
  private failed: string | null = null;
  private items: readonly ItemSummary[];
  private pos = 0;
  private target = 0;
  private els = new Map<number, HTMLElement>();
  private first = 0;
  private last = -1;
  private windowKey = '';
  private sentKey = '';
  private seq = 0;
  private pending = false;
  private dirty = true;
  private paused = false;
  private raf = 0;
  private lastFocus = -1;
  private readonly t0 = performance.now();
  private watchdog: ReturnType<typeof setTimeout> | null = null;
  private frameDog: ReturnType<typeof setTimeout> | null = null;
  private settleT: ReturnType<typeof setTimeout> | null = null;
  private readonly ro: ResizeObserver;
  private readonly cleanup: (() => void)[] = [];
  private drag: { x: number; y: number; start: number; moved: boolean; id: number } | null = null;

  constructor(root: HTMLElement, ctx: LensContext, spec: KitSpec) {
    this.root = root;
    this.ctx = ctx;
    this.spec = spec;
    this.mini = ctx.env.mini;
    this.items = ctx.items;
    failures.set(this, { msg: null, subs: new Set() });
    root.classList.add(styles.kit, 'lens');
    root.dataset.kit = spec.id;
    this.adoptCSS(scopeCSS(spec.css, spec.id));
    this.scene = el('div', `${styles.scene} kit-scene`, { 'aria-hidden': 'true' });
    this.scene.innerHTML = sanitizeHTML(spec.scene);
    this.stage = el('div', `${styles.stage} kit-stage`, { role: 'list', 'aria-label': spec.name });
    root.append(this.scene, this.stage);
    const fi = ctx.focus !== null ? this.items.findIndex((i) => i.id === ctx.focus) : -1;
    this.pos = this.target = fi >= 0 ? fi : 0;
    this.startWorker();
    this.rewindow(true);
    if (!this.mini) this.bind();
    this.ro = new ResizeObserver(() => this.kick());
    this.ro.observe(root);
    this.kick();
  }

  // ── style and worker ──

  private adoptCSS(css: string): void {
    if (!css) return;
    try {
      const sheet = new CSSStyleSheet();
      sheet.replaceSync(css);
      document.adoptedStyleSheets = [...document.adoptedStyleSheets, sheet];
      this.sheet = sheet;
    } catch {
      // No constructed stylesheets here (old browser): the lens runs unstyled rather than breaking the page's CSP.
    }
  }

  private startWorker(): void {
    const url = URL.createObjectURL(new Blob([workerSource(this.spec.layout)], { type: 'text/javascript' }));
    try {
      this.worker = new Worker(url);
    } catch (err) {
      this.fail(err instanceof Error ? err.message : 'This browser could not start the lens.');
      return;
    } finally {
      setTimeout(() => URL.revokeObjectURL(url), 5000);
    }
    this.worker.onmessage = (e: MessageEvent<unknown>) => this.onMessage(e.data);
    this.worker.onerror = (e) => {
      e.preventDefault();
      this.fail(e.message || 'The layout code has an error.');
    };
    this.watchdog = setTimeout(() => {
      if (!this.ready) this.fail('The layout code did not start.');
    }, 4000);
  }

  // Lens code can post messages too, so nothing from the worker is trusted as is.
  private onMessage(m: unknown): void {
    const msg = m && typeof m === 'object' ? (m as { type?: unknown; message?: unknown }) : {};
    if (msg.type === 'ready') {
      if (this.ready) return;
      this.ready = true;
      if (this.watchdog) clearTimeout(this.watchdog);
      this.kick();
    } else if (msg.type === 'error') {
      this.fail(typeof msg.message === 'string' && msg.message ? msg.message : 'The layout code stopped.');
    } else if (msg.type === 'frame' && this.pending) {
      // The window holds at most 2 × window + 1 titles.
      const frame = readFrame(m, this.seq, this.spec.window * 2 + 1);
      if (!frame) return;
      this.pending = false;
      if (this.frameDog) clearTimeout(this.frameDog);
      this.apply(frame.first, frame.out);
      if (this.dirty || this.spec.animated) this.kick();
    }
  }

  private fail(msg: string): void {
    if (this.failed) return;
    this.failed = msg;
    this.worker?.terminate();
    this.worker = null;
    cancelAnimationFrame(this.raf);
    const card = el('div', styles.fail, { role: 'alert' }, el('b', null, null, `${this.spec.name} stopped`), el('span', null, null, msg.slice(0, 200)));
    this.root.append(card);
    const f = failures.get(this);
    if (f) {
      f.msg = msg;
      for (const s of f.subs) s(msg);
    }
  }

  // ── the window of titles that get DOM ──

  private rewindow(force = false): void {
    const n = this.items.length;
    const w = this.spec.window;
    const c = clamp(Math.round(this.pos), 0, Math.max(0, n - 1));
    // Move the window only when the position nears an edge that isn't the end of the list (cheap while scrolling).
    if (!force && n && this.last >= this.first && this.last < n && (this.first === 0 || c - this.first >= w / 2) && (this.last === n - 1 || this.last - c >= w / 2)) return;
    const first = n ? Math.max(0, c - w) : 0;
    const last = n ? Math.min(n - 1, c + w) : -1;
    for (const [i, node] of this.els) {
      if (i < first || i > last || node.dataset.id !== String(this.items[i]?.id)) {
        node.remove();
        this.els.delete(i);
      }
    }
    for (let i = first; i <= last; i++) if (!this.els.has(i)) this.els.set(i, this.build(this.items[i], i));
    this.first = first;
    this.last = last;
    this.windowKey = `${first}:${last}:${last >= 0 ? this.items[first].id : ''}:${last >= 0 ? this.items[last].id : ''}`;
    this.dirty = true;
  }

  private build(it: ItemSummary, i: number): HTMLElement {
    const f = fieldsOf(it, i);
    const html = this.spec.item.replace(/\{\{\s*(\w+)\s*\}\}/g, (_m, k: string) => (FIELDS.has(k) ? escapeHTML(f[k] ?? '') : ''));
    const node = el('div', `${styles.item} item`, { role: 'listitem', tabindex: '-1', 'data-id': it.id, 'aria-label': artLabel(it) });
    node.innerHTML = sanitizeHTML(html);
    for (const ph of node.querySelectorAll('.art')) {
      const extra = [...ph.classList].filter((c) => c !== 'art').join(' ');
      ph.replaceWith(artElement(it, { width: 220, eager: Math.abs(i - this.pos) < 3, className: extra }));
    }
    const p = paletteOf(it);
    node.style.setProperty('--c1', p.c1);
    node.style.setProperty('--c2', p.c2);
    node.style.setProperty('--c3', p.c3);
    node.style.setProperty('--i', String(i));
    node.style.setProperty('--n', String(this.items.length));
    node.style.setProperty('--p', String(progressFraction(it) ?? 0));
    node.style.setProperty('--hue', String(Math.round(p.hue)));
    node.style.visibility = 'hidden';
    this.stage.append(node);
    return node;
  }

  // ── frames ──

  private view() {
    const r = this.root.getBoundingClientRect();
    const safe = this.ctx.safe;
    return {
      w: r.width,
      h: r.height,
      t: (performance.now() - this.t0) / 1000,
      pos: this.pos,
      n: this.items.length,
      first: this.first,
      focus: this.focusIndex(),
      safe: { top: safe.top, bottom: safe.bottom },
      phone: r.width < 640,
      mini: this.mini,
      theme: this.ctx.env.theme,
      reducedMotion: this.ctx.env.reducedMotion,
    };
  }

  private focusIndex(): number {
    return clamp(Math.round(this.pos), 0, Math.max(0, this.items.length - 1));
  }

  private kick(): void {
    this.dirty = true;
    if (!this.raf && !this.failed && !this.paused) this.raf = requestAnimationFrame(() => this.loop());
  }

  private loop(): void {
    this.raf = 0;
    if (this.failed || this.paused) return;
    const rm = this.ctx.env.reducedMotion;
    const d = this.target - this.pos;
    if (Math.abs(d) > 0.0005) {
      this.pos = rm ? this.target : this.pos + d * 0.16;
      this.dirty = true;
    } else this.pos = this.target;
    this.rewindow();
    if ((this.dirty || this.spec.animated) && this.ready && !this.pending && this.worker) {
      this.pending = true;
      this.dirty = false;
      const items = this.windowKey !== this.sentKey ? this.items.slice(this.first, this.last + 1).map((it, k) => workerItem(it, this.first + k)) : undefined;
      this.sentKey = this.windowKey;
      this.worker.postMessage({ seq: ++this.seq, view: this.view(), items });
      this.frameDog = setTimeout(() => {
        if (this.pending) this.fail('The layout code took too long.');
      }, 2500);
    }
    const f = this.focusIndex();
    if (f !== this.lastFocus && this.items[f]) {
      this.els.get(this.lastFocus)?.classList.remove('is-focus');
      this.lastFocus = f;
      this.els.get(f)?.classList.add('is-focus');
      if (!this.mini) {
        this.ctx.setFocus(this.items[f].id);
        this.ctx.intent(this.items[f]);
      }
    }
    const moving = Math.abs(this.target - this.pos) > 0.0005;
    const busy = moving || this.dirty || (this.spec.animated && !this.mini && !rm);
    if (busy) this.raf = requestAnimationFrame(() => this.loop());
  }

  private apply(first: number, out: Out[]): void {
    for (let k = 0; k < out.length; k++) {
      const i = first + k;
      const node = this.els.get(i);
      if (!node) continue;
      const o = out[k];
      const op = clamp(o.o, 0, 1);
      if (op <= 0.001) {
        node.style.visibility = 'hidden';
        continue;
      }
      const wasHidden = node.style.visibility === 'hidden';
      node.style.visibility = '';
      node.style.opacity = op >= 0.999 ? '' : op.toFixed(3);
      node.style.transform = `translate(-50%, -50%) translate3d(${o.x.toFixed(1)}px, ${o.y.toFixed(1)}px, ${o.z.toFixed(1)}px) rotateX(${o.rx.toFixed(2)}deg) rotateY(${o.ry.toFixed(2)}deg) rotateZ(${o.rz.toFixed(2)}deg) scale(${o.s.toFixed(3)})`;
      node.style.zIndex = String(Math.round(o.zIndex ?? 1000 + o.z));
      const cls = o.cls.replace(/[^\w\s-]/g, '');
      if (node.dataset.cls !== cls) {
        for (const c of (node.dataset.cls ?? '').split(/\s+/)) if (c) node.classList.remove(c);
        for (const c of cls.split(/\s+/)) if (c) node.classList.add(c);
        node.dataset.cls = cls;
      }
      // Ask for an image sized to how the lens actually draws it.
      if (wasHidden) {
        const art = node.querySelector<HTMLElement>('.art');
        const w = art?.offsetWidth ?? 0;
        if (art && w > 0 && Math.abs(w - Number(art.dataset.w ?? 0)) > 40) resizeArt(art, w);
      }
    }
  }

  // ── input ──

  private bind(): void {
    const axis = this.spec.input;
    const step = this.spec.step;
    const on = <K extends keyof HTMLElementEventMap>(type: K, fn: (e: HTMLElementEventMap[K]) => void, opts?: AddEventListenerOptions) => {
      this.root.addEventListener(type, fn as EventListener, opts);
      this.cleanup.push(() => this.root.removeEventListener(type, fn as EventListener, opts));
    };
    const settle = () => {
      if (this.settleT) clearTimeout(this.settleT);
      this.settleT = setTimeout(() => {
        this.target = clamp(Math.round(this.target), 0, Math.max(0, this.items.length - 1));
        this.kick();
      }, 140);
    };
    on(
      'wheel',
      (e) => {
        if (axis === 'none' || e.ctrlKey) return;
        e.preventDefault();
        const dlt = axis === 'y' ? e.deltaY || e.deltaX : e.deltaX || e.deltaY;
        const px = e.deltaMode === 1 ? dlt * 40 : dlt;
        this.target = clamp(this.target + px / step, 0, Math.max(0, this.items.length - 1));
        this.kick();
        settle();
      },
      { passive: false },
    );
    on('pointerdown', (e) => {
      if (axis === 'none' || e.button !== 0) return;
      this.drag = { x: e.clientX, y: e.clientY, start: this.target, moved: false, id: e.pointerId };
    });
    on('pointermove', (e) => {
      const d = this.drag;
      if (!d || e.pointerId !== d.id) return;
      const dd = axis === 'y' ? e.clientY - d.y : e.clientX - d.x;
      if (Math.abs(dd) > 6 && !d.moved) {
        d.moved = true;
        this.root.setPointerCapture(e.pointerId);
      }
      if (d.moved) {
        this.target = clamp(d.start - dd / step, 0, Math.max(0, this.items.length - 1));
        this.kick();
      }
    });
    const end = () => {
      if (this.drag?.moved) settle();
      setTimeout(() => (this.drag = null), 0);
    };
    on('pointerup', end);
    on('pointercancel', end);
    on('click', (e) => {
      if (this.drag?.moved) return;
      const node = (e.target as HTMLElement).closest<HTMLElement>(`.${styles.item}`);
      if (!node) return;
      const i = [...this.els].find(([, n]) => n === node)?.[0];
      if (i === undefined) return;
      if (i === this.focusIndex()) this.ctx.open(this.items[i].id, node.querySelector('.art') ?? node);
      else {
        this.target = i;
        this.kick();
      }
    });
  }

  move(dir: Dir): boolean {
    if (!this.items.length) return false;
    const d = dir === 'left' || dir === 'up' ? -1 : 1;
    this.target = clamp(Math.round(this.target) + d, 0, this.items.length - 1);
    this.kick();
    return true;
  }

  key(e: KeyboardEvent): boolean {
    if (e.key === 'Enter') {
      const i = this.focusIndex();
      const it = this.items[i];
      if (it) this.ctx.open(it.id, this.els.get(i)?.querySelector('.art') ?? null);
      return true;
    }
    if (e.key === 'Home' || e.key === 'End') {
      this.target = e.key === 'Home' ? 0 : Math.max(0, this.items.length - 1);
      this.kick();
      return true;
    }
    return false;
  }

  update(items: readonly ItemSummary[]): void {
    const keep = this.items[this.focusIndex()]?.id;
    this.items = items;
    const i = keep !== undefined ? items.findIndex((x) => x.id === keep) : -1;
    if (i >= 0) {
      // The same title stays in front; titles arriving later don't move you.
      const shift = i - Math.round(this.pos);
      this.pos += shift;
      this.target += shift;
    } else {
      this.pos = this.target = clamp(Math.round(this.pos), 0, Math.max(0, items.length - 1));
    }
    for (const node of this.els.values()) {
      node.style.setProperty('--n', String(items.length));
      node.classList.remove('is-focus');
    }
    this.lastFocus = -1;
    this.rewindow(true);
    this.kick();
  }

  rectOf(id: Id): DOMRect | null {
    for (const [i, node] of this.els) {
      if (this.items[i]?.id === id) return (node.querySelector('.art') ?? node).getBoundingClientRect();
    }
    return null;
  }

  artOf(id: Id): HTMLElement | null {
    for (const [i, node] of this.els) {
      if (this.items[i]?.id === id) return node.querySelector<HTMLElement>('.art');
    }
    return null;
  }

  reveal(id: Id): void {
    const i = this.items.findIndex((x) => x.id === id);
    if (i < 0) return;
    this.pos = this.target = i;
    this.rewindow(true);
    this.kick();
  }

  pause(paused: boolean): void {
    this.paused = paused;
    if (!paused) this.kick();
  }

  destroy(): void {
    if (this.sheet) document.adoptedStyleSheets = document.adoptedStyleSheets.filter((s) => s !== this.sheet);
    cancelAnimationFrame(this.raf);
    for (const t of [this.watchdog, this.frameDog, this.settleT]) if (t) clearTimeout(t);
    this.worker?.terminate();
    this.ro.disconnect();
    for (const c of this.cleanup) c();
    this.root.removeAttribute('data-kit');
  }
}

function escapeHTML(s: string): string {
  return s.replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c] ?? c);
}

/** A failure card in place of a lens whose spec doesn't check out. */
function brokenModule(error: string): LensModule {
  return {
    mount(root) {
      root.classList.add(styles.kit);
      const card = el('div', styles.fail, { role: 'alert' }, el('b', null, null, 'This lens can’t run'), el('span', null, null, error));
      root.append(card);
      const view: LensView = { update: () => undefined, move: () => false, rectOf: () => null, destroy: () => card.remove() };
      failures.set(view, { msg: error, subs: new Set() });
      return view;
    },
  };
}

/** A lens module from a kit spec (validated here; `id` keeps an example's own id). */
export function kitModule(raw: unknown, id?: string): LensModule {
  const r = validateSpec(raw, { id: id ?? (raw && typeof raw === 'object' && typeof (raw as { id?: unknown }).id === 'string' ? (raw as { id: string }).id : undefined) });
  if (!r.ok) return brokenModule(r.error);
  const spec = r.spec;
  return {
    mount(root, ctx) {
      return new KitView(root, ctx, spec);
    },
  };
}
