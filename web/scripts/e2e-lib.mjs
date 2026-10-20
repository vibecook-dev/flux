// Shared helpers for the end-to-end product test (scripts/e2e-*.mjs): a real
// release flux-server on the real corpus, driven by the system Google Chrome
// through playwright-core.
//
// House rules baked in (shared machine, see docs/prototype/e2e.md):
// - every process we start runs at `nice -n 19`;
// - temp files, browser profiles and data dirs live under $E2E_WORK
//   (default $FLUX_WORK/tmp/e2e);
// - heavy steps wait while $FLUX_WORK/QUIET exists;
// - tailnet mode is always off.
import { execFileSync, spawn } from 'node:child_process';
import { createWriteStream, existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { loadavg } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { fluxWork, testMedia } from './work.mjs';

const here = dirname(fileURLToPath(import.meta.url));
export const WEB_DIR = resolve(here, '..');
// Paths come from the environment (see CONTRIBUTING.md); nothing defaults to
// a particular machine. FLUX_WORK is required unless every E2E_* path is set.
export const WORK = process.env.E2E_WORK ?? join(fluxWork('the end-to-end run'), 'tmp', 'e2e');
export const MEDIA = process.env.E2E_MEDIA ?? testMedia();
export const RESULTS = process.env.E2E_RESULTS ?? join(fluxWork('bench results'), 'bench', 'results');
export const SHOTS = process.env.E2E_SHOTS ?? join(fluxWork('screenshots'), 'bench', 'e2e-shots');
// FLUX_SERVER, else $CARGO_TARGET_DIR/release/flux-server, else the repo's target/.
export const SERVER_BIN =
  process.env.FLUX_SERVER ?? join(process.env.CARGO_TARGET_DIR ?? resolve(WEB_DIR, '..', 'target'), 'release', 'flux-server');
// A file that pauses heavy steps while it exists (a shared machine's "be quiet" flag).
export const QUIET = process.env.FLUX_WORK ? join(process.env.FLUX_WORK, 'QUIET') : null;

// Chrome, Playwright and the server keep their temp files under WORK.
process.env.TMPDIR = join(WORK, 'tmp');
mkdirSync(process.env.TMPDIR, { recursive: true });
mkdirSync(SHOTS, { recursive: true });
mkdirSync(RESULTS, { recursive: true });

export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
export const load1 = () => Math.round(loadavg()[0] * 100) / 100;

/** Waits (polling once a minute) while the QUIET flag exists. Returns ms waited. */
export async function quietGate(label = 'next step') {
  const t0 = Date.now();
  let logged = false;
  while (QUIET && existsSync(QUIET)) {
    if (!logged) console.log(`[e2e] QUIET is set; waiting before ${label}…`);
    logged = true;
    await sleep(60_000);
  }
  const waited = Date.now() - t0;
  if (waited > 1000) console.log(`[e2e] QUIET cleared after ${Math.round(waited / 1000)} s`);
  return waited;
}

export function pct(xs, p) {
  const v = xs.filter((x) => typeof x === 'number' && Number.isFinite(x));
  if (!v.length) return null;
  const s = [...v].sort((a, b) => a - b);
  return Math.round(s[Math.min(s.length - 1, Math.ceil((p / 100) * s.length) - 1)] * 10) / 10;
}

export const summary = (xs) => ({ n: xs.length, p50: pct(xs, 50), p95: pct(xs, 95), max: pct(xs, 100) });

/** RSS of a process in MB (ps), or null when it is gone. */
export function rssMb(pid) {
  try {
    const kb = Number(execFileSync('ps', ['-o', 'rss=', '-p', String(pid)]).toString().trim());
    return Number.isFinite(kb) ? Math.round(kb / 102.4) / 10 : null;
  } catch {
    return null;
  }
}

/** Samples a process's RSS every `everyMs` (ps); `.stop()` → {peak, samples}. */
export function sampleRss(pid, everyMs = 500) {
  const samples = [];
  const t0 = Date.now();
  const timer = setInterval(() => {
    const mb = rssMb(pid);
    if (mb !== null) samples.push([Date.now() - t0, mb]);
  }, everyMs);
  return {
    stop() {
      clearInterval(timer);
      return { peak_mb: samples.reduce((m, [, v]) => Math.max(m, v), 0), samples: samples.length, every_ms: everyMs };
    },
  };
}

/**
 * Starts the release flux-server: `nice -n 19`, tailnet off, the built web UI,
 * wrapped in `/usr/bin/time -l` so the exit report carries the true peak RSS.
 * Resolves once `GET /api/v1/server` answers 200; `readyMs` is spawn → that 200.
 */
const running = new Set();
// Never leave a server behind when a run dies.
for (const sig of ['exit', 'SIGINT', 'SIGTERM', 'uncaughtException']) {
  process.on(sig, (e) => {
    for (const pid of running) {
      try {
        process.kill(pid, 'SIGTERM');
      } catch {
        /* gone */
      }
    }
    if (sig === 'uncaughtException') {
      console.error(e);
      process.exit(1);
    }
    if (sig === 'SIGINT' || sig === 'SIGTERM') process.exit(130);
  });
}

export async function startServer({ dataDir, port, libraries = [], args = [], log = null, env = {} }) {
  mkdirSync(dataDir, { recursive: true });
  const busy = await fetch(`http://127.0.0.1:${port}/api/v1/server`).then(
    () => true,
    () => false,
  );
  if (busy) throw new Error(`port ${port} is already serving; stop that server first`);
  const logPath = log ?? join(WORK, `server-${port}.log`);
  const out = createWriteStream(logPath, { flags: 'a' });
  const argv = [
    '-l',
    'nice',
    '-n',
    '19',
    SERVER_BIN,
    '--data-dir',
    dataDir,
    '--port',
    String(port),
    '--bind',
    '127.0.0.1',
    '--tailnet',
    'off',
    '--web-dir',
    join(WEB_DIR, 'dist'),
    ...libraries.flatMap((l) => ['--library', l]),
    ...args,
  ];
  const t0 = performance.now();
  const proc = spawn('/usr/bin/time', argv, {
    env: { ...process.env, FLUX_LOG: process.env.FLUX_LOG ?? 'info', ...env },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  let stderr = '';
  proc.stdout.pipe(out);
  proc.stderr.on('data', (b) => {
    out.write(b);
    stderr += b.toString();
    if (stderr.length > 1 << 20) stderr = stderr.slice(-(1 << 19));
  });
  const exited = new Promise((r) => proc.on('exit', (code, sig) => r({ code, sig })));
  const base = `http://127.0.0.1:${port}`;
  let readyMs = null;
  for (let i = 0; i < 30_000; i++) {
    try {
      const r = await fetch(`${base}/api/v1/server`);
      if (r.ok) {
        readyMs = performance.now() - t0;
        break;
      }
    } catch {
      /* not listening yet */
    }
    if (proc.exitCode !== null) throw new Error(`flux-server exited early (log: ${logPath})`);
    await sleep(2);
  }
  if (readyMs === null) throw new Error('flux-server did not become ready');
  // The server is the child of `nice`, which is the child of `time`.
  const serverPid = () => {
    try {
      return Number(execFileSync('pgrep', ['-n', '-f', `${SERVER_BIN} --data-dir ${dataDir}`]).toString().trim()) || null;
    } catch {
      return null;
    }
  };
  const pid = serverPid();
  if (pid) running.add(pid);
  return {
    proc,
    pid,
    base,
    readyMs,
    logPath,
    rss: () => (pid ? rssMb(pid) : null),
    /**
     * SIGTERM; resolves with `/usr/bin/time -l`'s peak RSS (MB) and the shutdown
     * time. That figure is getrusage(RUSAGE_CHILDREN): the largest of the server
     * and every ffmpeg it ran, so pair it with `sampleRss` for the server alone.
     */
    async stop() {
      const t = performance.now();
      if (pid) {
        try {
          process.kill(pid, 'SIGTERM');
        } catch {
          /* gone */
        }
      }
      const res = await Promise.race([exited, sleep(15_000).then(() => null)]);
      if (res === null) {
        try {
          if (pid) process.kill(pid, 'SIGKILL');
        } catch {
          /* gone */
        }
        await exited;
      }
      const shutdownMs = performance.now() - t;
      running.delete(pid);
      const m = stderr.match(/(\d+)\s+maximum resident set size/);
      const peakMb = m ? Math.round(Number(m[1]) / 1024 / 102.4) / 10 : null;
      out.end();
      return { peakMb, shutdownMs: Math.round(shutdownMs) };
    },
  };
}

/** Fetch JSON from the API (throws on non-2xx with the server's text). */
export async function api(base, path, init) {
  // flux refuses unsafe /api/ calls without X-Flux-Request (its CSRF guard); the web client sends it on every call.
  const headers = { 'x-flux-request': '1', ...(init?.body ? { 'content-type': 'application/json' } : {}), ...(init?.headers ?? {}) };
  const r = await fetch(base + path, { ...init, headers });
  if (!r.ok) throw new Error(`${init?.method ?? 'GET'} ${path} → ${r.status} ${await r.text()}`);
  return r.status === 204 ? null : r.json();
}

/** Waits until no library is scanning (and every library has finished at least once). */
export async function waitScansIdle(base, { timeoutMs = 600_000, ids = null } = {}) {
  const t0 = Date.now();
  for (;;) {
    const libs = await api(base, '/api/v1/libraries');
    const relevant = ids ? libs.filter((l) => ids.includes(l.id)) : libs;
    if (relevant.length && relevant.every((l) => l.scan.state !== 'scanning' && l.scan.finished_at !== null)) return libs;
    if (Date.now() - t0 > timeoutMs) throw new Error('scans did not finish');
    await sleep(100);
  }
}

/** System Chrome via playwright-core, with its profile on the SSD. */
export async function launchChrome(extraArgs = []) {
  const { chromium } = await import('playwright-core');
  return chromium.launch({
    channel: 'chrome',
    headless: process.env.HEADFUL ? false : true,
    args: ['--autoplay-policy=no-user-gesture-required', '--mute-audio', `--disk-cache-dir=${join(WORK, 'tmp', 'chrome-cache')}`, ...extraArgs],
  });
}

/** Records console errors, page errors, failed requests and HTTP ≥ 400 on a page. */
export function watchPage(page, sink) {
  page.on('console', (m) => {
    if (m.type() === 'error') sink.push({ kind: 'console', text: m.text().slice(0, 300), url: page.url() });
  });
  page.on('pageerror', (e) => sink.push({ kind: 'pageerror', text: String(e).slice(0, 300), url: page.url() }));
  page.on('requestfailed', (r) => {
    const f = r.failure()?.errorText ?? '';
    // Aborted media/segment requests are normal (seeks, hls.js cancellations, unmounts).
    if (/ERR_ABORTED|NS_BINDING_ABORTED/.test(f)) return;
    sink.push({ kind: 'requestfailed', text: `${f} ${r.url()}`, url: page.url() });
  });
  page.on('response', (r) => {
    if (r.status() >= 400) sink.push({ kind: 'http', status: r.status(), text: `${r.status()} ${r.request().method()} ${r.url()}`, url: page.url() });
  });
}

/**
 * A device without an AV1 decoder (e.g. an M1 Mac): MSE, canPlayType and
 * mediaCapabilities all say no to AV1, so the server must transcode it.
 */
export const NO_AV1_INIT = `(() => {
  const isAv1 = (t) => /av01|av1/i.test(String(t));
  if (window.MediaSource) {
    const orig = MediaSource.isTypeSupported.bind(MediaSource);
    MediaSource.isTypeSupported = (t) => (isAv1(t) ? false : orig(t));
  }
  const cpt = HTMLMediaElement.prototype.canPlayType;
  HTMLMediaElement.prototype.canPlayType = function (t) { return isAv1(t) ? '' : cpt.call(this, t); };
  if (navigator.mediaCapabilities) {
    const di = navigator.mediaCapabilities.decodingInfo.bind(navigator.mediaCapabilities);
    navigator.mediaCapabilities.decodingInfo = (cfg) =>
      cfg?.video && isAv1(cfg.video.contentType)
        ? Promise.resolve({ supported: false, smooth: false, powerEfficient: false })
        : di(cfg);
  }
})();`;

export function writeResult(name, obj) {
  const path = join(RESULTS, `flux-e2e-${name}.json`);
  writeFileSync(path, JSON.stringify(obj, null, 2));
  console.log(`[e2e] wrote ${path}`);
  return path;
}

export function readJson(path, fallback = null) {
  try {
    return JSON.parse(readFileSync(path, 'utf8'));
  } catch {
    return fallback;
  }
}

/** Machine facts recorded with every result (the machine is shared). */
export function machine() {
  let chrome = null;
  try {
    chrome = execFileSync('/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', ['--version']).toString().trim();
  } catch {
    /* not installed there */
  }
  let commit = null;
  try {
    commit = execFileSync('git', ['-C', WEB_DIR, 'rev-parse', '--short', 'HEAD']).toString().trim();
  } catch {
    /* not a checkout */
  }
  return {
    host: 'M1 Max, 64 GB, macOS (SHARED with another project; numbers are indicative)',
    chrome,
    commit,
    server_bin: SERVER_BIN,
    node: process.version,
  };
}
