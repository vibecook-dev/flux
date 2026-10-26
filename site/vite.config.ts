/**
 * The site is one static page. Its live demo is the real web client (../web),
 * built in mock mode into `.demo/` (`pnpm demo`) and served at `<base>demo/`:
 * by the dev and preview servers from `.demo/`, and in a build from
 * `dist/demo/` (`pnpm build` builds it there).
 *
 * SITE_BASE sets where the site lives, e.g. `/flux/` on GitHub Pages; the demo
 * is built for `<SITE_BASE>demo/` to match.
 */
import { createReadStream, existsSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import { extname, join, normalize } from 'node:path';
import type { Connect, Plugin } from 'vite';
import { defineConfig } from 'vite';

const base = process.env.SITE_BASE ?? '/';
/** The site's public address, for link previews (og:image wants an absolute URL). */
const siteUrl = (process.env.SITE_URL ?? base).replace(/\/?$/, '/');

const TYPES: Record<string, string> = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.json': 'application/json',
  '.svg': 'image/svg+xml',
  '.woff2': 'font/woff2',
  '.woff': 'font/woff',
  '.png': 'image/png',
  '.webp': 'image/webp',
  '.wasm': 'application/wasm',
};

/** Serve a built demo from `dir` at <base>demo/, with index.html for the app's own routes. */
function serveDemo(dir: string): Connect.NextHandleFunction {
  const prefix = `${base}demo/`;
  return (req, res, next) => {
    const url = (req.url ?? '').split('?')[0];
    if (!url.startsWith(prefix) && url !== prefix.slice(0, -1)) return next();
    if (!existsSync(join(dir, 'index.html'))) {
      res.statusCode = 503;
      res.setHeader('content-type', 'text/html; charset=utf-8');
      res.end('<p style="font:16px system-ui;color:#ccc;background:#111;padding:2em">The demo isn’t built yet. Run <code>pnpm demo</code> in site/.</p>');
      return;
    }
    const rel = decodeURIComponent(url.slice(prefix.length));
    let file = normalize(join(dir, rel));
    if (!file.startsWith(dir)) return next();
    if (!existsSync(file) || !statSync(file).isFile()) file = join(dir, 'index.html');
    res.setHeader('content-type', TYPES[extname(file)] ?? 'application/octet-stream');
    res.setHeader('cache-control', 'no-cache');
    createReadStream(file).pipe(res);
  };
}

/** Put the site's address into the page's link-preview tags, and its base into 404.html. */
function addresses(): Plugin {
  return {
    name: 'flux-site-addresses',
    transformIndexHtml: (html) => html.replaceAll('__SITE_URL__', siteUrl),
    closeBundle() {
      const page = join(import.meta.dirname, 'dist', '404.html');
      if (existsSync(page)) writeFileSync(page, readFileSync(page, 'utf8').replaceAll('__BASE__', base));
    },
  };
}

function demo(): Plugin {
  return {
    name: 'flux-demo',
    // `pnpm dev` serves the demo `pnpm demo` built; `pnpm preview` the one `pnpm build` put in dist.
    configureServer: (server) => void server.middlewares.use(serveDemo(join(import.meta.dirname, '.demo'))),
    configurePreviewServer: (server) => void server.middlewares.use(serveDemo(join(import.meta.dirname, 'dist', 'demo'))),
  };
}

export default defineConfig({
  base,
  plugins: [demo(), addresses()],
  build: {
    target: 'es2022',
    assetsInlineLimit: 2048,
    reportCompressedSize: false,
  },
});
