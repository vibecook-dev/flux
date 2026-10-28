import { describe, expect, it } from 'vitest';
import { type BundleEntry, stampBundle } from './pwa-plugin.ts';

const HTML = '<!doctype html>\n<html>\n  <head>\n    <title>flux</title>\n  </head>\n  <body></body>\n</html>\n';

function bundle(extra: Record<string, BundleEntry> = {}, worker = true): Record<string, BundleEntry> {
  const b: Record<string, BundleEntry> = {
    'index.html': { type: 'asset', fileName: 'index.html', source: HTML },
    'assets/index-BNz0hwrt.js': { type: 'chunk', fileName: 'assets/index-BNz0hwrt.js', code: 'x', imports: [], dynamicImports: [], exports: [] },
    'assets/index-D5nIgSa0.css': { type: 'asset', fileName: 'assets/index-D5nIgSa0.css', source: 'a{}' },
    'assets/dm-mono-latin-400-normal-4GdczIuU.woff2': { type: 'asset', fileName: 'assets/dm-mono-latin-400-normal-4GdczIuU.woff2', source: new Uint8Array([1]) },
    'assets/dm-mono-latin-400-normal--0xN8mdc.woff': { type: 'asset', fileName: 'assets/dm-mono-latin-400-normal--0xN8mdc.woff', source: new Uint8Array([1]) },
    ...extra,
  };
  if (worker) b['sw.js'] = { type: 'chunk', fileName: 'sw.js', code: 'self.addEventListener("fetch",()=>{});', imports: [], dynamicImports: [], exports: [] };
  return b;
}

describe('stampBundle', () => {
  it('stamps index.html and the worker with the same build id', () => {
    const b = bundle();
    const { build, precache } = stampBundle(b, true);
    expect(build).toMatch(/^[0-9a-f]{16}$/);
    expect(b['index.html'].source).toContain(`<meta name="flux-build" content="${build}" />`);
    const code = b['sw.js'].code ?? '';
    expect(code.startsWith(`self.__FLUX_SW__ = ${JSON.stringify({ build, precache })};\n`)).toBe(true);
    expect(code).toContain('self.addEventListener("fetch"');
  });

  it('precaches the client’s files but the .woff fallbacks', () => {
    const { precache } = stampBundle(bundle(), true);
    expect(precache).toEqual(['assets/dm-mono-latin-400-normal-4GdczIuU.woff2', 'assets/index-BNz0hwrt.js', 'assets/index-D5nIgSa0.css']);
  });

  it('changes the id when any file or the page changes, and only then', () => {
    const a = stampBundle(bundle(), true).build;
    expect(stampBundle(bundle(), true).build).toBe(a);
    const renamed = bundle();
    delete renamed['assets/index-BNz0hwrt.js'];
    renamed['assets/index-Zz9yYx8w.js'] = { type: 'chunk', fileName: 'assets/index-Zz9yYx8w.js', code: 'y', imports: [], dynamicImports: [], exports: [] };
    expect(stampBundle(renamed, true).build).not.toBe(a);
    const page = bundle();
    page['index.html'].source = HTML.replace('<title>flux</title>', '<title>flux!</title>');
    expect(stampBundle(page, true).build).not.toBe(a);
    // The worker's own code doesn't count: it carries the id.
    const sw = bundle();
    sw['sw.js'].code = 'other';
    expect(stampBundle(sw, true).build).toBe(a);
  });

  it('builds a mock bundle without a worker, but still stamps the page', () => {
    const b = bundle({}, false);
    const { build } = stampBundle(b, false);
    expect(b['index.html'].source).toContain(build);
    expect(() => stampBundle(bundle(), false)).toThrow(/mock build has a sw\.js/);
  });

  it('refuses a worker that isn’t a classic script on its own', () => {
    const b = bundle();
    b['sw.js'].imports = ['assets/core-abc.js'];
    expect(() => stampBundle(b, true)).toThrow(/must stand alone.*assets\/core-abc\.js/);
    const e = bundle();
    e['sw.js'].exports = ['x'];
    expect(() => stampBundle(e, true)).toThrow(/must stand alone/);
    const missing = bundle();
    delete missing['sw.js'];
    expect(() => stampBundle(missing, true)).toThrow(/sw\.js was not built/);
  });

  it('needs an index.html with a head', () => {
    const b = bundle();
    delete b['index.html'];
    expect(() => stampBundle(b, true)).toThrow(/no index\.html/);
    const headless = bundle();
    headless['index.html'].source = '<p>hi</p>';
    expect(() => stampBundle(headless, true)).toThrow(/no <\/head>/);
  });
});
