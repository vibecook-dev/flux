#!/usr/bin/env node
// Z-fighting check on the real WebGPU path (src/lens/gpu/room3d.ts, the Gallery's renderer):
//
//   pnpm dev &                      # the check imports the renderer from the dev server
//   BASE=http://localhost:5173 pnpm check:zfight
//
// Draws a red wall and a green decal `gap` px in front of it, from 300 to 20,000 px away
// (the Gallery's corridor ends at 7,600) and from head-on to 87°, and compares the decal's
// pixels with the decal drawn alone. The Gallery hangs everything at least 0.75 px in front
// of its wall: there the wall must win no pixel. A decal on the wall's own plane (gap 0) is
// the control: seen at an angle it z-fights, which shows the check can tell.
import { chromium } from 'playwright-core';

const BASE = process.env.BASE ?? 'http://localhost:5173';
const browser = await chromium.launch({ channel: 'chrome', headless: true });
const page = await browser.newPage({ viewport: { width: 900, height: 700 } });
await page.goto(`${BASE}/?mock_latency=0`, { waitUntil: 'networkidle' });
const rows = await page.evaluate(async () => {
  const { gpu } = await import('/src/lens/gpu/device.ts');
  const R = await import('/src/lens/gpu/room3d.ts');
  const S = await import('/src/lens/gpu/scene3d.ts');
  const P = await import('/src/lens/gpu/posters.ts');
  const T = await import('/src/lens/gpu/text.ts');
  const g = await gpu();
  if (!g || !(await R.prepareRoom3D(g))) return null;
  const posters = P.posterCache(g);
  await posters.prepare();
  const canvas = document.createElement('canvas');
  canvas.style.cssText = 'position:fixed;left:0;top:0;width:800px;height:600px;z-index:99';
  document.body.append(canvas);
  const r = new R.Room3DRenderer(g, canvas);
  r.resize(800, 600);
  const tex = { posters: posters.views(), glyphs: T.textAtlas(g).view, key: 'zfight' };
  const cam = { width: 800, height: 600, centerX: 400, centerY: 300, persp: 800, yaw: 0, eyeX: 0, eyeY: 0, eyeZ: 0, near: 8, dpr: 1, time: 0 };
  const read = document.createElement('canvas');
  read.width = 800;
  read.height = 600;
  const rx = read.getContext('2d', { willReadFrequently: true });
  const greens = (wall, gap, angle, dist) => {
    const ry = (angle * Math.PI) / 180;
    const b = new S.Quad3DBatch(4);
    const size = dist * 0.6;
    if (wall) {
      const o = b.add(0, 0, -dist, size * 4, size * 4, R.KINDR.solid);
      b.rotate(o, 0, ry);
      b.color(o, [1, 0, 0, 1]);
      b.flags(o, R.FLAGR.noaa);
    }
    // `gap` along the wall's normal.
    const o = b.add(Math.sin(ry) * gap, 0, -dist + Math.cos(ry) * gap, size, size, R.KINDR.solid);
    b.rotate(o, 0, ry);
    b.color(o, [0, 1, 0, 1]);
    b.flags(o, R.FLAGR.noaa);
    r.draw(b, { reflections: 0, architecture: wall ? 1 : 0, decals: 1 }, cam, tex);
    rx.clearRect(0, 0, 800, 600);
    rx.drawImage(canvas, 0, 0, 800, 600);
    const px = rx.getImageData(0, 0, 800, 600).data;
    let n = 0;
    for (let i = 0; i < px.length; i += 4) if (px[i + 1] > 200 && px[i] < 50) n++;
    return n;
  };
  const out = [];
  for (const angle of [0, 45, 60, 80, 87]) {
    for (const dist of [300, 2000, 7600, 20000]) {
      // Each against the decal alone at the same gap (a gap also moves it a little on screen).
      const lost = (gap) => Math.max(0, greens(false, gap, angle, dist) - greens(true, gap, angle, dist));
      out.push({ angle, dist, alone: greens(false, 0.75, angle, dist), gallery: lost(0.75), thin: lost(0.05), coplanar: lost(0) });
    }
  }
  canvas.remove();
  return out;
});
await browser.close();
if (!rows) {
  console.error('No WebGPU here: nothing to check.');
  process.exit(2);
}
let failed = 0;
console.log('angle   distance   decal pixels   lost at 0.75 px   at 0.05 px   coplanar (control)');
for (const r of rows) {
  if (r.gallery > 0) failed++;
  console.log(`${String(r.angle).padStart(4)}°  ${String(r.dist).padStart(8)}   ${String(r.alone).padStart(12)}   ${String(r.gallery).padStart(15)}   ${String(r.thin).padStart(10)}   ${String(r.coplanar).padStart(18)}`);
}
console.log(failed ? `\n${failed} cases where the wall shows through a decal 0.75 px in front of it: z-fighting.` : '\nNo z-fighting at the Gallery\'s gap, at any distance or angle checked.');
process.exit(failed ? 1 : 0);
