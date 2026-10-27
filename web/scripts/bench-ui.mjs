#!/usr/bin/env node
// What every interaction of the shell costs, frame by frame, in real Chrome
// against the mock build (headless Chrome gets a real Metal WebGPU adapter, so
// the GPU lenses run as they do for people).
//
//   pnpm build && pnpm preview --port 4199 &
//   BASE=http://localhost:4199 pnpm bench:ui [filter] [--runs N]
//
// The filter keeps the scenarios whose key contains it: load, lenses, wall,
// flow, gallery, island (search and spaces), item, sheets, lens-gallery, theme.
//
// For each interaction it records the animation frames (p50/p95/max and frames
// longer than 20 ms: headless Chrome runs at 60 Hz), a Chrome trace of the same
// span (busy time on the renderer's main and compositor threads and in the GPU
// process, per frame; style, layout and paint time), long animation frames and
// React commits. A 120 Hz display allows 8.3 ms a frame: judge the per-frame
// thread times against that, not the 60 Hz frame times. Writes $RESULTS
// (default $FLUX_WORK/bench/results/flux-web-ui.json) and prints a table.
//
// Buttons are clicked with dispatchEvent, not Playwright's click(): that waits
// for the element to hold still, reading its position every frame, and those
// forced layouts would be counted as the app's.
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname } from 'node:path';
import { chromium } from 'playwright-core';
import { resultFile } from './work.mjs';

const BASE = (process.env.BASE ?? 'http://localhost:4199').replace(/\/$/, '');
const OUT = process.env.RESULTS ?? resultFile('flux-web-ui.json');
const args = process.argv.slice(2);
const runsAt = args.indexOf('--runs');
const RUNS = runsAt >= 0 ? Number(args[runsAt + 1]) || 1 : 1;
const filter = args.find((a, i) => !a.startsWith('--') && (runsAt < 0 || i !== runsAt + 1)) ?? '';
const Q = '?mock_tailnet=running&mock_demo=1&mock_latency=0';

const CATEGORIES = [
  'toplevel',
  'cc',
  'devtools.timeline',
  'disabled-by-default-devtools.timeline',
  'disabled-by-default-devtools.timeline.frame',
  'blink.user_timing',
  'v8.execute',
];

const pct = (xs, p) => {
  if (!xs.length) return null;
  const s = [...xs].sort((a, b) => a - b);
  return Math.round(s[Math.min(s.length - 1, Math.floor((p / 100) * s.length))] * 10) / 10;
};
const round = (v) => Math.round(v * 10) / 10;

/** A page with the first-run hint dismissed, a lens chosen, and recorders for frames, long animation frames and React commits. */
async function open(browser, { lens = 'wall', path = '/', theme = 'dark' } = {}) {
  const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 2, colorScheme: theme });
  await ctx.addInitScript(
    ({ lens, theme }) => {
      localStorage.setItem('flux.coach.done', 'true');
      localStorage.setItem('flux.theme', theme);
      localStorage.setItem('flux.lens.choice', JSON.stringify({ all: lens, lib1: lens, lib2: lens, lib3: lens }));
      // React reports every commit to the DevTools hook when there is one.
      window.__commits = 0;
      window.__REACT_DEVTOOLS_GLOBAL_HOOK__ = {
        supportsFiber: true,
        renderers: new Map(),
        inject(renderer) {
          this.renderers.set(this.renderers.size + 1, renderer);
          return this.renderers.size;
        },
        onCommitFiberRoot() {
          window.__commits++;
        },
        onCommitFiberUnmount() {},
        onPostCommitFiberRoot() {},
        onScheduleFiberRoot() {},
        checkDCE() {},
      };
      let rec = null;
      // One frame loop per recording: a loop left from the last one stops on its own.
      const loop = (owner) => {
        const tick = (t) => {
          if (rec !== owner) return;
          owner.frames.push(t);
          requestAnimationFrame(tick);
        };
        requestAnimationFrame(tick);
      };
      window.__benchStart = () => {
        rec = { frames: [], loaf: [], commits0: window.__commits };
        loop(rec);
      };
      window.__benchStop = () => {
        const r = rec;
        rec = null;
        const deltas = r.frames.slice(1).map((t, i) => t - r.frames[i]);
        return { frames: r.frames, deltas, loaf: r.loaf, commits: window.__commits - r.commits0 };
      };
      try {
        new PerformanceObserver((list) => {
          if (!rec) return;
          for (const e of list.getEntries())
            rec.loaf.push({
              ms: Math.round(e.duration),
              blocking: Math.round(e.blockingDuration ?? 0),
              scripts: (e.scripts ?? [])
                .slice(0, 3)
                .map((s) => `${String(s.sourceFunctionName || s.invoker || '').slice(0, 60)}@${String(s.sourceURL || '').split('/').pop()}:${Math.round(s.duration)}`),
            });
        }).observe({ type: 'long-animation-frame' });
      } catch {
        // no LoAF API: frames only
      }
    },
    { lens, theme },
  );
  // EXTRA_CSS: a stylesheet added to every page, to try a change without rebuilding.
  if (process.env.EXTRA_CSS) {
    await ctx.addInitScript((css) => {
      const add = () => {
        const s = document.createElement('style');
        s.textContent = css;
        document.head.append(s);
      };
      if (document.head) add();
      else document.addEventListener('DOMContentLoaded', add);
    }, process.env.EXTRA_CSS);
  }
  const page = await ctx.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(e.message));
  const cdp = await ctx.newCDPSession(page);
  await page.goto(`${BASE}${path}${Q}`, { waitUntil: 'networkidle' });
  await page.waitForTimeout(1200);
  return { ctx, page, cdp, errors };
}

async function startTrace(cdp) {
  await cdp.send('Tracing.start', {
    transferMode: 'ReturnAsStream',
    traceConfig: { includedCategories: CATEGORIES, recordMode: 'recordContinuously' },
  });
}

async function endTrace(cdp) {
  const done = new Promise((resolve) => cdp.once('Tracing.tracingComplete', resolve));
  await cdp.send('Tracing.end');
  const { stream } = await done;
  let text = '';
  for (;;) {
    const chunk = await cdp.send('IO.read', { handle: stream });
    text += chunk.base64Encoded ? Buffer.from(chunk.data, 'base64').toString() : chunk.data;
    if (chunk.eof) break;
  }
  await cdp.send('IO.close', { handle: stream });
  const parsed = JSON.parse(text);
  return Array.isArray(parsed) ? parsed : parsed.traceEvents;
}

/**
 * Busy time per thread between the two marks, and style/layout/paint on the page's
 * main thread. With the page's frame times (and when the start mark was, in page
 * time), also the main thread's and the GPU process's busy time within each frame.
 */
function digest(events, frames = [], markAt = null) {
  const names = new Map();
  const procs = new Map();
  for (const e of events) {
    if (e.ph !== 'M') continue;
    if (e.name === 'thread_name') names.set(`${e.pid}:${e.tid}`, e.args.name);
    if (e.name === 'process_name') procs.set(e.pid, e.args.name);
  }
  const start = events.find((e) => e.name === 'bench:start')?.ts;
  const end = events.find((e) => e.name === 'bench:end')?.ts;
  const inside = (e) => start !== undefined && end !== undefined && e.ts >= start && e.ts + (e.dur ?? 0) <= end + 1;
  // Each thread's tasks as merged intervals: one task is often traced twice, a
  // ThreadControllerImpl::RunTask inside a RunTask, and summing both counted it twice.
  const tasks = new Map();
  for (const e of events) {
    if (e.ph !== 'X' || !e.dur || (e.name !== 'ThreadControllerImpl::RunTask' && e.name !== 'RunTask')) continue;
    const k = `${e.pid}:${e.tid}`;
    if (!tasks.has(k)) tasks.set(k, []);
    tasks.get(k).push([e.ts, e.ts + e.dur]);
  }
  for (const [k, list] of tasks) {
    list.sort((a, b) => a[0] - b[0]);
    const merged = [];
    for (const [a, b] of list) {
      const last = merged[merged.length - 1];
      if (last && a <= last[1]) last[1] = Math.max(last[1], b);
      else merged.push([a, b]);
    }
    tasks.set(k, merged);
  }
  const busy = new Map();
  for (const [k, list] of tasks) {
    let ms = 0;
    for (const [a, b] of list) if (inside({ ts: a, dur: b - a })) ms += (b - a) / 1000;
    if (ms > 0) busy.set(k, ms);
  }
  // The page's renderer: the one whose main thread was busiest.
  let main = null;
  for (const [k, ms] of busy) if (names.get(k) === 'CrRendererMain' && (!main || ms > busy.get(main))) main = k;
  // A commit that waits for the compositor's next frame is the main thread idling inside a
  // task (frame pacing), not work: take those waits out of its busy time.
  const waits = events.filter((e) => e.ph === 'X' && e.dur && inside(e) && `${e.pid}:${e.tid}` === main && e.name === 'LayerTreeHost::WaitForCommitCompletion');
  if (main) busy.set(main, busy.get(main) - waits.reduce((a, e) => a + e.dur / 1000, 0));
  const pid = main ? Number(main.split(':')[0]) : null;
  const sum = (pred) => [...busy].filter(([k]) => pred(k)).reduce((a, [, ms]) => a + ms, 0);
  const compositor = sum((k) => Number(k.split(':')[0]) === pid && names.get(k) === 'Compositor');
  const gpu = sum((k) => /GPU/i.test(procs.get(Number(k.split(':')[0])) ?? '') && /CrGpuMain|VizCompositorThread/.test(names.get(k) ?? ''));
  const stage = { style: 0, layout: 0, paint: 0 };
  for (const e of events) {
    if (e.ph !== 'X' || !e.dur || !inside(e) || `${e.pid}:${e.tid}` !== main) continue;
    if (e.name === 'UpdateLayoutTree') stage.style += e.dur / 1000;
    else if (e.name === 'Layout') stage.layout += e.dur / 1000;
    else if (e.name === 'Paint' || e.name === 'PrePaint') stage.paint += e.dur / 1000;
  }
  // Per frame: map page time to trace time through the start mark, then bucket each task's overlap.
  const perFrame = { main: [], gpu: [] };
  if (frames.length > 1 && markAt !== null && start !== undefined) {
    const toTrace = (ms) => start + (ms - markAt) * 1000;
    const bounds = frames.map(toTrace);
    const bucket = (pred) => {
      const out = new Array(bounds.length - 1).fill(0);
      for (const [k, list] of tasks) {
        if (!pred(k)) continue;
        for (const [a, b] of list) {
          // Binary search the first frame the task can overlap.
          let lo = 0;
          let hi = bounds.length - 1;
          while (lo < hi) {
            const mid = (lo + hi) >> 1;
            if (bounds[mid + 1] <= a) lo = mid + 1;
            else hi = mid;
          }
          for (let i = lo; i < out.length && bounds[i] < b; i++) out[i] += Math.max(0, Math.min(b, bounds[i + 1]) - Math.max(a, bounds[i])) / 1000;
        }
      }
      return out;
    };
    perFrame.main = bucket((k) => k === main);
    for (const w of waits) {
      for (let i = 0; i < perFrame.main.length; i++) {
        const o = Math.max(0, Math.min(w.ts + w.dur, bounds[i + 1]) - Math.max(w.ts, bounds[i]));
        if (o > 0) perFrame.main[i] = Math.max(0, perFrame.main[i] - o / 1000);
      }
    }
    perFrame.gpu = bucket((k) => /GPU/i.test(procs.get(Number(k.split(':')[0])) ?? '') && /CrGpuMain|VizCompositorThread/.test(names.get(k) ?? ''));
  }
  return { main: main ? busy.get(main) : 0, compositor, gpu, ...stage, perFrame, spanMs: start !== undefined && end !== undefined ? (end - start) / 1000 : null };
}

async function measure(session, name, act) {
  const { page, cdp } = session;
  await page.evaluate(() => window.__benchStart());
  await startTrace(cdp);
  const markAt = await page.evaluate(() => performance.mark('bench:start').startTime);
  await act(page);
  await page.evaluate(() => performance.mark('bench:end'));
  const events = await endTrace(cdp);
  // TRACES=dir keeps each trace, to open in DevTools' Performance panel.
  if (process.env.TRACES) {
    mkdirSync(process.env.TRACES, { recursive: true });
    writeFileSync(`${process.env.TRACES}/${name.replace(/[^\w]+/g, '-')}.json`, JSON.stringify({ traceEvents: events }));
  }
  const r = await page.evaluate(() => window.__benchStop());
  const t = digest(events, r.frames, markAt);
  const n = Math.max(1, r.deltas.length);
  return {
    name,
    frames: r.deltas.length,
    p50: pct(r.deltas, 50),
    p95: pct(r.deltas, 95),
    max: r.deltas.length ? round(Math.max(...r.deltas)) : null,
    dropped: r.deltas.filter((d) => d > 20).length,
    mainPerFrame: round(t.main / n),
    mainP95: pct(t.perFrame.main, 95),
    mainMax: t.perFrame.main.length ? round(Math.max(...t.perFrame.main)) : null,
    over8: t.perFrame.main.filter((ms) => ms > 8.3).length,
    gpuP95: pct(t.perFrame.gpu, 95),
    compPerFrame: round(t.compositor / n),
    gpuPerFrame: round(t.gpu / n),
    style: round(t.style),
    layout: round(t.layout),
    paint: round(t.paint),
    loaf: r.loaf.length,
    worstLoaf: r.loaf.length ? Math.max(...r.loaf.map((l) => l.ms)) : 0,
    loafScripts: r.loaf.sort((a, b) => b.ms - a.ms).slice(0, 2),
    commits: r.commits,
  };
}

const wheel = async (page, dy, steps, gap = 16, dx = 0) => {
  for (let i = 0; i < steps; i++) {
    await page.mouse.wheel(dx, dy);
    await page.waitForTimeout(gap);
  }
};
const lensButton = (id) => `button[role="tab"][data-lens="${id}"]`;
const posterSpot = async (page) =>
  page.evaluate(() => {
    const a = [...document.querySelectorAll('#main a[href*="/item/"]')].find((x) => {
      const r = x.getBoundingClientRect();
      return r.width > 40 && r.top > 120 && r.bottom < innerHeight - 120;
    });
    if (!a) return null;
    const r = a.getBoundingClientRect();
    return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
  });

// Each scenario opens its own page (so "first switch" really is the first) and returns rows.
const SCENARIOS = {
  async load(browser) {
    const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 2, colorScheme: 'dark' });
    await ctx.addInitScript(() => {
      localStorage.setItem('flux.coach.done', 'true');
      localStorage.setItem('flux.theme', 'dark');
      window.__loaf = [];
      try {
        new PerformanceObserver((l) => l.getEntries().forEach((e) => window.__loaf.push(Math.round(e.duration)))).observe({ type: 'long-animation-frame', buffered: true });
      } catch {
        // none
      }
    });
    const page = await ctx.newPage();
    const t0 = Date.now();
    await page.goto(`${BASE}/${Q}`, { waitUntil: 'domcontentloaded' });
    await page.waitForSelector('#main a[href*="/item/"]', { state: 'attached', timeout: 30_000 });
    const firstPoster = Date.now() - t0;
    await page.waitForTimeout(1500);
    const loaf = await page.evaluate(() => window.__loaf);
    await ctx.close();
    return [{ name: 'load: first titles', firstPosterMs: firstPoster, loaf: loaf.length, worstLoaf: loaf.length ? Math.max(...loaf) : 0, loafs: loaf.sort((a, b) => b - a).slice(0, 6) }];
  },

  async lenses(browser) {
    const s = await open(browser);
    const rows = [];
    for (const round of ['first', 'again']) {
      for (const id of ['flow', 'shelf', 'gallery', 'wall']) {
        rows.push(
          await measure(s, `lens switch → ${id} (${round})`, async (page) => {
            await page.dispatchEvent(lensButton(id), 'click');
            await page.waitForTimeout(1400);
          }),
        );
      }
    }
    await s.ctx.close();
    return rows;
  },

  async wall(browser) {
    const s = await open(browser, { path: '/library/3' });
    await s.page.mouse.move(720, 450);
    const rows = [
      await measure(s, 'wall: zoom out and in (6 steps)', async (page) => {
        for (const k of ['-', '-', '+', '+', '+', '-']) {
          await page.keyboard.press(k);
          await page.waitForTimeout(450);
        }
      }),
      await measure(s, 'wall: fast scroll (10k)', async (page) => {
        await wheel(page, 260, 40);
        await page.waitForTimeout(700);
        await wheel(page, -260, 40);
        await page.waitForTimeout(700);
      }),
    ];
    await s.ctx.close();
    return rows;
  },

  async flow(browser) {
    const s = await open(browser, { lens: 'flow' });
    await s.page.mouse.move(720, 450);
    const rows = [
      await measure(s, 'flow: wheel flick', async (page) => {
        await wheel(page, 0, 30, 16, 120);
        await page.waitForTimeout(900);
        await wheel(page, 0, 30, 16, -120);
        await page.waitForTimeout(900);
      }),
      await measure(s, 'flow: arrow keys', async (page) => {
        for (let i = 0; i < 12; i++) {
          await page.keyboard.press('ArrowRight');
          await page.waitForTimeout(90);
        }
        await page.waitForTimeout(800);
      }),
    ];
    await s.ctx.close();
    return rows;
  },

  async gallery(browser) {
    const s = await open(browser, { lens: 'gallery' });
    await s.page.mouse.move(720, 450);
    const rows = [
      await measure(s, 'gallery: walk the corridor', async (page) => {
        await wheel(page, 120, 50, 20);
        await page.waitForTimeout(1200);
      }),
    ];
    await s.ctx.close();
    return rows;
  },

  async island(browser) {
    const s = await open(browser);
    const rows = [
      await measure(s, 'island: open, type a search, close', async (page) => {
        await page.keyboard.press('/');
        await page.waitForTimeout(500);
        await page.keyboard.type('amber', { delay: 90 });
        await page.waitForTimeout(800);
        await page.keyboard.press('Escape');
        await page.waitForTimeout(400);
        await page.keyboard.press('Escape');
        await page.waitForTimeout(700);
      }),
      await measure(s, 'spaces: All → Movies → TV → All', async (page) => {
        for (const label of ['Movies', 'TV Shows', 'All']) {
          await page.keyboard.press('/');
          await page.waitForTimeout(350);
          await page.locator('button[aria-pressed]', { hasText: label }).first().dispatchEvent('click');
          await page.waitForTimeout(900);
        }
      }),
    ];
    await s.ctx.close();
    return rows;
  },

  async item(browser) {
    const s = await open(browser);
    const rows = [];
    for (const round of ['first', 'again']) {
      rows.push(
        await measure(s, `item: open and close (${round})`, async (page) => {
          const at = await posterSpot(page);
          if (!at) throw new Error('no poster on screen');
          await page.mouse.click(at.x, at.y);
          await page.waitForTimeout(1300);
          await page.keyboard.press('Escape');
          await page.waitForTimeout(1100);
        }),
      );
    }
    rows.push(
      await measure(s, 'item: scroll its page', async (page) => {
        const at = await posterSpot(page);
        await page.mouse.click(at.x, at.y);
        await page.waitForTimeout(1200);
        await page.mouse.move(720, 500);
        await wheel(page, 160, 20, 30);
        await wheel(page, -160, 20, 30);
        await page.waitForTimeout(500);
      }),
    );
    rows.push(
      await measure(s, 'item: edit details open and close', async (page) => {
        const edit = page.getByRole('button', { name: /Edit details/ });
        await edit.scrollIntoViewIfNeeded();
        await edit.dispatchEvent('click');
        await page.waitForTimeout(700);
        await page.getByRole('button', { name: 'Cancel' }).dispatchEvent('click');
        await page.waitForTimeout(700);
        await page.keyboard.press('Escape');
        await page.waitForTimeout(900);
      }),
    );
    await s.ctx.close();
    return rows;
  },

  async sheets(browser) {
    const s = await open(browser);
    const rows = [];
    for (const [label, path] of [
      ['Agents & changes', '/changes'],
      ['Libraries & scanning', '/admin'],
      ['Remote access', '/connect'],
    ]) {
      rows.push(
        await measure(s, `sheet: ${label} open, scroll, close`, async (page) => {
          await page.locator('[aria-haspopup="menu"]').first().dispatchEvent('click');
          await page.waitForTimeout(400);
          await page.locator(`a[href$="${path}"][role="menuitem"]`).dispatchEvent('click');
          await page.waitForTimeout(900);
          await page.mouse.move(720, 500);
          await wheel(page, 180, 12, 30);
          await wheel(page, -180, 12, 30);
          await page.keyboard.press('Escape');
          await page.waitForTimeout(900);
        }),
      );
    }
    await s.ctx.close();
    return rows;
  },

  async 'lens-gallery'(browser) {
    const s = await open(browser);
    const rows = [];
    for (const round of ['first', 'again'])
      rows.push(
        await measure(s, `lens gallery: open, browse, close (${round})`, async (page) => {
          await page.keyboard.press('\\');
          await page.waitForTimeout(1200);
          await page.mouse.move(720, 500);
          await wheel(page, 160, 10, 30);
          await page.keyboard.press('Escape');
          await page.waitForTimeout(900);
        }),
      );
    await s.ctx.close();
    return rows;
  },

  async theme(browser) {
    const s = await open(browser);
    const rows = [
      await measure(s, 'theme: dark → light → dark', async (page) => {
        for (const label of ['Light', 'Dark']) {
          await page.locator('[aria-haspopup="menu"]').first().dispatchEvent('click');
          await page.waitForTimeout(350);
          await page.getByRole('radio', { name: label }).dispatchEvent('click');
          await page.waitForTimeout(900);
          await page.keyboard.press('Escape');
          await page.waitForTimeout(300);
        }
      }),
    ];
    await s.ctx.close();
    return rows;
  },
};

const browser = await chromium.launch({ channel: 'chrome', headless: true, args: ['--enable-unsafe-webgpu'] });
const all = [];
for (let run = 0; run < RUNS; run++) {
  for (const [name, scenario] of Object.entries(SCENARIOS)) {
    if (filter && !name.includes(filter)) continue;
    try {
      for (const row of await scenario(browser)) all.push({ run, scenario: name, ...row });
    } catch (e) {
      all.push({ run, scenario: name, name: `${name}: FAILED`, error: String(e.message).split('\n')[0] });
    }
  }
}
await browser.close();

mkdirSync(dirname(OUT), { recursive: true });
writeFileSync(OUT, JSON.stringify({ base: BASE, at: new Date().toISOString(), rows: all }, null, 2));
const cols = ['frames', 'max', 'dropped', 'mainPerFrame', 'mainP95', 'mainMax', 'over8', 'compPerFrame', 'gpuPerFrame', 'gpuP95', 'style', 'layout', 'paint', 'worstLoaf', 'commits'];
console.log(['interaction'.padEnd(46), ...cols.map((c) => c.padStart(7))].join(' '));
for (const r of all) {
  if (r.error) {
    console.log(`${r.name.padEnd(46)} ${r.error}`);
    continue;
  }
  if (r.firstPosterMs !== undefined) {
    console.log(`${r.name.padEnd(46)} first titles ${r.firstPosterMs} ms; long frames ${r.loaf} (worst ${r.worstLoaf} ms: ${r.loafs.join(', ')})`);
    continue;
  }
  console.log([r.name.slice(0, 46).padEnd(46), ...cols.map((c) => String(r[c] ?? '').padStart(7))].join(' '));
}
console.log(`\n${OUT}`);
