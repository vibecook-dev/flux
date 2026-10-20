/**
 * Vite dev/preview middleware for mock playback bytes:
 *   /mock-media/index.json, /mock-media/hls/*  → $FLUX_MOCK_MEDIA (scripts/make-mock-media.sh;
 *                                                default $FLUX_WORK/web-mock-media)
 *   /mock-media/features/*                     → $FLUX_FEATURES (the feature clips;
 *                                                default $FLUX_TEST_MEDIA/features)
 *
 * With neither set, /mock-media/* answers 404 and the mock UI still works
 * without playback bytes (see CONTRIBUTING.md).
 *
 * Supports single HTTP Range requests (progressive playback and seeking) and
 * marks segments immutable, like flux-server will (research/03 §10).
 */
import { createReadStream, existsSync, statSync } from 'node:fs';
import type { IncomingMessage, ServerResponse } from 'node:http';
import { extname, join, normalize } from 'node:path';
import type { Connect, Plugin } from 'vite';

const env = process.env;
const MEDIA = env.FLUX_MOCK_MEDIA ?? (env.FLUX_WORK ? join(env.FLUX_WORK, 'web-mock-media') : null);
const FEATURES =
  env.FLUX_FEATURES ??
  (env.FLUX_TEST_MEDIA ? join(env.FLUX_TEST_MEDIA, 'features') : env.FLUX_WORK ? join(env.FLUX_WORK, 'media', 'features') : null);
let warned = false;

const TYPES: Record<string, string> = {
  '.m3u8': 'application/vnd.apple.mpegurl',
  '.m4s': 'video/iso.segment',
  '.mp4': 'video/mp4',
  '.mkv': 'video/x-matroska',
  '.webm': 'video/webm',
  '.json': 'application/json',
  '.vtt': 'text/vtt',
};

function resolve(urlPath: string): string | null {
  const rel = decodeURIComponent(urlPath.replace(/^\/mock-media\/?/, ''));
  const isFeature = rel.startsWith('features/');
  const root = isFeature ? FEATURES : MEDIA;
  const sub = isFeature ? rel.slice('features/'.length) : rel;
  if (!root) {
    if (!warned) console.warn('[mock-media] FLUX_WORK / FLUX_TEST_MEDIA are not set; /mock-media/* returns 404 (see CONTRIBUTING.md)');
    warned = true;
    return null;
  }
  const full = normalize(join(root, sub));
  if (!full.startsWith(normalize(root))) return null; // no path traversal
  return existsSync(full) && statSync(full).isFile() ? full : null;
}

function serve(req: IncomingMessage, res: ServerResponse, next: Connect.NextFunction) {
  const url = (req.url ?? '').split('?')[0];
  if (!url.startsWith('/mock-media/')) return next();
  const file = resolve(url);
  if (!file) {
    res.statusCode = 404;
    res.end('not found');
    return;
  }
  const size = statSync(file).size;
  const ext = extname(file);
  res.setHeader('content-type', TYPES[ext] ?? 'application/octet-stream');
  res.setHeader('accept-ranges', 'bytes');
  res.setHeader('cache-control', ext === '.m3u8' || ext === '.json' ? 'no-cache' : 'public, max-age=31536000, immutable');
  const range = /^bytes=(\d*)-(\d*)$/.exec(req.headers.range ?? '');
  if (range) {
    let start = range[1] === '' ? size - Number(range[2]) : Number(range[1]);
    let end = range[1] !== '' && range[2] !== '' ? Number(range[2]) : size - 1;
    start = Math.max(0, start);
    end = Math.min(size - 1, end);
    if (start > end) {
      res.statusCode = 416;
      res.setHeader('content-range', `bytes */${size}`);
      res.end();
      return;
    }
    res.statusCode = 206;
    res.setHeader('content-range', `bytes ${start}-${end}/${size}`);
    res.setHeader('content-length', String(end - start + 1));
    if (req.method === 'HEAD') return res.end();
    createReadStream(file, { start, end }).pipe(res);
    return;
  }
  res.setHeader('content-length', String(size));
  if (req.method === 'HEAD') return res.end();
  createReadStream(file).pipe(res);
}

export function mockMediaPlugin(): Plugin {
  return {
    name: 'flux-mock-media',
    configureServer(server) {
      server.middlewares.use(serve);
    },
    configurePreviewServer(server) {
      server.middlewares.use(serve);
    },
  };
}
