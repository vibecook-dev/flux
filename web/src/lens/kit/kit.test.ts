// @vitest-environment jsdom
import { runInNewContext } from 'node:vm';
import { describe, expect, it } from 'vitest';
import type { ItemSummary } from '../../api/types';
import { EXAMPLES } from './examples';
import { makePrompt, parseLensReply } from './prompt';
import { readFrame, sanitizeHTML, scopeCSS, slug, validateSpec, workerSource } from './runtime';

describe('sanitizeHTML', () => {
  it('drops scripts, handlers, links, images and inline styles', () => {
    const out = sanitizeHTML(
      '<div class="a" onclick="alert(1)" style="color:red" id="x"><script>alert(2)</script><img src="x" onerror="alert(3)"><a href="javascript:alert(4)">link</a><iframe src="https://example.com"></iframe><b data-k="1">ok</b></div>',
    );
    expect(out).not.toMatch(/script|onclick|onerror|style=|<img|<a|iframe|javascript|id="x"/i);
    expect(out).toContain('<div class="a">');
    expect(out).toContain('<b data-k="1">ok</b>');
  });

  it('keeps simple SVG shapes but not their links or animations', () => {
    const out = sanitizeHTML('<svg viewBox="0 0 10 10"><circle cx="5" cy="5" r="4" fill="red"/><use href="#evil"/><animate attributeName="r"/></svg>');
    expect(out).toContain('<circle cx="5" cy="5" r="4" fill="red">');
    expect(out).not.toMatch(/<use|href|<animate/);
  });

  it('strips url() in kept attributes', () => {
    expect(sanitizeHTML('<div title="url(https://x)">t</div>')).toBe('<div>t</div>');
  });
});

describe('scopeCSS', () => {
  it('scopes .lens selectors to the lens root and everything else under it', () => {
    const css = scopeCSS('.lens .item { width: 10px } .lens.wide{color:red} .card, .lens > .x { opacity: .5 }', 'tonight');
    expect(css).toContain('[data-kit="tonight"] .item{ width: 10px }');
    expect(css).toContain('[data-kit="tonight"].wide{color:red}');
    expect(css).toContain('[data-kit="tonight"] .card,[data-kit="tonight"] > .x{ opacity: .5 }');
  });

  it('drops page-wide rules, imports, fonts and anything that loads', () => {
    const css = scopeCSS('@import url(x.css); :root { --bg: red } body { margin: 0 } @font-face { font-family: X; src: url(x.woff) } .lens .a { background: url(https://x/y.png) }', 'k');
    expect(css).not.toMatch(/@import|:root|body|@font-face|https:/);
    expect(css).toContain('[data-kit="k"] .a{ background: none }');
  });

  it('renames keyframes so they never collide with the page', () => {
    const css = scopeCSS('@keyframes spin { to { transform: rotate(1turn) } } .lens .a { animation: spin 2s infinite }', 'helix');
    expect(css).toContain('@keyframes k-helix-spin');
    expect(css).toContain('animation: k-helix-spin 2s infinite');
  });

  it('scopes rules inside @media', () => {
    const css = scopeCSS('@media (max-width: 600px) { .lens .a { width: 1px } }', 'k');
    expect(css).toBe('@media (max-width: 600px){[data-kit="k"] .a{ width: 1px }}');
  });
});

describe('validateSpec', () => {
  it('says what is missing, in plain words', () => {
    expect(validateSpec(null)).toEqual({ ok: false, error: expect.stringMatching(/JSON object/) as string });
    expect(validateSpec({ name: 'X' })).toEqual({ ok: false, error: expect.stringMatching(/no layout/) as string });
    expect(validateSpec({ layout: 'return 1' })).toEqual({ ok: false, error: expect.stringMatching(/function called layout/) as string });
  });

  it('fills defaults and keeps numbers in range', () => {
    const r = validateSpec({ name: '  ', layout: 'const layout = (items) => items.map(() => ({}))', step: 9999, window: 2, input: 'z' });
    expect(r.ok).toBe(true);
    if (!r.ok) return;
    expect(r.spec).toMatchObject({ name: 'My lens', input: 'x', step: 600, window: 8, animated: false, item: '<div class="art"></div>', css: '', scene: '' });
    expect(r.spec.blurb).toBe('A lens called My lens.');
  });

  it('derives a stable id from the name and code, or keeps the one given', () => {
    const spec = { name: 'Departure board!', layout: 'function layout(items) { return items }' };
    const a = validateSpec(spec);
    const b = validateSpec(spec);
    expect(a.ok && b.ok && a.spec.id === b.spec.id).toBe(true);
    expect(a.ok && a.spec.id.startsWith('my-departure-board-')).toBe(true);
    const kept = validateSpec(spec, { id: 'tonight' });
    expect(kept.ok && kept.spec.id).toBe('tonight');
  });

  it('refuses layout code that imports (import() would fetch)', () => {
    expect(validateSpec({ layout: 'import("/api/v1/libraries"); function layout(items) { return items }' })).toEqual({ ok: false, error: expect.stringMatching(/import/) as string });
    expect(validateSpec({ layout: 'function layout(items) { return items } // important' }).ok).toBe(true);
  });

  it('accepts every example', () => {
    for (const [id, spec] of Object.entries(EXAMPLES)) expect(validateSpec(spec, { id }).ok).toBe(true);
  });

  it('slugs names for ids', () => {
    expect(slug('Amélie’s Café — Night')).toBe('amelie-s-cafe-night');
  });
});

type Message = Record<string, unknown>;
interface Frame {
  type: string;
  first: number;
  out: Message[];
}

/** The worker's real script, run in a sandbox the way a Worker runs it: `self` is the global scope, with any `globals` on it. */
function startWorker(layout: string, globals: Record<string, unknown> = {}) {
  const posted: Message[] = [];
  const scope: Record<string, unknown> & { onmessage?: (e: { data: unknown }) => void } = { ...globals, postMessage: (m: Message) => posted.push(m) };
  scope.self = scope;
  runInNewContext(workerSource(layout), scope);
  return {
    posted,
    run(data: unknown): Message {
      scope.onmessage?.({ data });
      return posted[posted.length - 1];
    },
  };
}

describe('example layouts, run by the worker script', () => {
  const window = Array.from({ length: 41 }, (_, k) => ({
    i: 980 + k,
    id: 980 + k,
    title: `Title ${k}`,
    year: k % 7 === 0 ? null : 1990 + (k % 5),
    decade: 1990,
    kind: 'movie',
    seasons: 0,
    badges: [],
    progress: 0,
    watched: false,
    isNew: false,
    available: true,
    hue: 120,
    sat: 0.4,
    lum: 0.2,
  }));
  const view = { w: 1440, h: 900, t: 0, pos: 1000.4, n: 13000, first: 980, focus: 1000, safe: { top: 68, bottom: 84 }, phone: false, mini: false, theme: 'dark', reducedMotion: false };

  for (const [id, spec] of Object.entries(EXAMPLES)) {
    it(`${id} returns one transform per title in the window`, () => {
      const worker = startWorker(spec.layout);
      expect(worker.posted).toEqual([{ type: 'ready' }]);
      const frame = worker.run({ seq: 1, view, items: window }) as unknown as Frame;
      expect(frame.type).toBe('frame');
      expect(frame.first).toBe(980);
      expect(frame.out).toHaveLength(window.length);
      // The focused title is drawn.
      expect(frame.out[20].o).toBeGreaterThan(0.5);
      for (const o of frame.out) for (const v of Object.values(o)) if (typeof v === 'number') expect(Number.isFinite(v)).toBe(true);
    });
  }

  it('cleans what a layout returns', () => {
    const worker = startWorker(`function layout() { return [{ x: NaN, s: Infinity, zIndex: 'top', cls: 'a'.repeat(99) }, null, 7]; }`);
    const frame = worker.run({ seq: 2, view, items: window.slice(0, 3) }) as unknown as Frame;
    expect(frame.out).toEqual([{ x: 0, y: 0, z: 0, rx: 0, ry: 0, rz: 0, s: 1, o: 1, zIndex: null, cls: 'a'.repeat(60) }, { o: 0 }, { o: 0 }]);
  });

  it('locks the worker down first, before the lens code', () => {
    const src = workerSource('/* the lens */ function layout(items) { return items; }');
    const lens = src.indexOf('/* the lens */');
    expect(src.trimStart().startsWith('for (const k of [')).toBe(true);
    expect(src.indexOf('Object.defineProperty(self, k')).toBeLessThan(lens);
    for (const k of ['fetch', 'XMLHttpRequest', 'WebSocket', 'EventSource', 'importScripts']) expect(src.slice(0, lens)).toContain(`"${k}"`);
  });

  it('leaves lens code no network, storage or address, and still runs it', () => {
    const net = () => 'sent';
    const worker = startWorker(
      `const seen = [typeof fetch, typeof self.fetch, typeof globalThis.XMLHttpRequest, typeof importScripts, typeof WebSocket, typeof EventSource, typeof indexedDB, typeof navigator, typeof location];
       let threw = 'nothing';
       try { fetch('/api/v1/libraries', { method: 'POST', headers: { 'x-flux-request': '1' } }); } catch (e) { threw = e.name; }
       function layout() { return [...seen, threw].map((cls) => ({ cls })); }`,
      { fetch: net, XMLHttpRequest: net, importScripts: net, WebSocket: net, EventSource: net, indexedDB: {}, navigator: {}, location: {} },
    );
    const frame = worker.run({ seq: 1, view, items: window }) as unknown as Frame;
    expect(frame.out.map((o) => o.cls)).toEqual([...Array<string>(9).fill('undefined'), 'TypeError']);
  });

  it('reports a layout that throws, returns the wrong thing or is missing', () => {
    expect(startWorker('function layout() { throw new Error("boom") }').run({ seq: 1, view })).toEqual({ type: 'error', message: 'boom' });
    expect(startWorker('const layout = () => 1;').run({ seq: 1, view })).toEqual({ type: 'error', message: 'layout() must return an array, one object per title.' });
    expect(() => startWorker('const notLayout = 1;')).toThrow('The code does not define layout(items, view).');
  });
});

describe('readFrame', () => {
  const asked = (out: unknown, seq = 3) => readFrame({ type: 'frame', seq, first: 5, out }, 3, 4);

  it('takes only the frame the page asked for', () => {
    expect(asked([{}], 2)).toBeNull();
    expect(readFrame({ type: 'error', seq: 3, out: [] }, 3, 4)).toBeNull();
    expect(readFrame(null, 3, 4)).toBeNull();
    expect(asked({ length: 1e12 })).toBeNull();
  });

  it('caps it to the window and checks every value', () => {
    expect(asked(Array.from({ length: 1000 }, () => ({})))?.out).toHaveLength(4);
    expect(asked([{ x: 4, y: 'far', s: Infinity, o: 0.5, zIndex: NaN, cls: { evil: 1 } }, null])).toEqual({
      first: 5,
      out: [
        { x: 4, y: 0, z: 0, rx: 0, ry: 0, rz: 0, s: 1, o: 0.5, zIndex: null, cls: '' },
        { x: 0, y: 0, z: 0, rx: 0, ry: 0, rz: 0, s: 1, o: 0, zIndex: null, cls: '' },
      ],
    });
  });
});

describe('prompt', () => {
  const items = [
    { id: 1, kind: 'movie', title: 'Metropolis', year: 1927 },
    { id: 2, kind: 'show', title: 'Severance', year: 2022 },
  ] as unknown as ItemSummary[];

  it('carries the format, an example, the library and the wish', () => {
    const p = makePrompt('An airport departure board', items);
    expect(p).toContain('function layout(items, view)');
    expect(p).toContain('items is only the window of titles near the position');
    expect(p).toContain('"name": "Tonight"');
    expect(p).toContain('Metropolis (1927, film); Severance (2022, show)');
    expect(p).toContain('"An airport departure board"');
    expect(p).toContain('no style attributes');
  });

  it('reads a lens out of bare JSON, a fenced block or prose around JSON', () => {
    const lens = { name: 'X', layout: 'function layout(i){return i}' };
    expect(parseLensReply(JSON.stringify(lens))).toEqual(lens);
    expect(parseLensReply('Here you go:\n```json\n' + JSON.stringify(lens) + '\n```\nEnjoy!')).toEqual(lens);
    expect(parseLensReply('Sure! ' + JSON.stringify(lens) + ' Let me know.')).toEqual(lens);
    expect(() => parseLensReply('no lens here')).toThrow(/no JSON object/);
    expect(() => parseLensReply('   ')).toThrow(/Paste the lens/);
  });
});
