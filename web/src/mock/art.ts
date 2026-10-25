/**
 * Mock artwork. The test library has no real posters (and a prototype must not
 * ship TMDB images), so the mock server renders stand-ins: abstract duotone
 * compositions with typeset titles, as SVG. Each composition is a list of
 * simple layers so the same description can be (a) rendered to SVG and
 * (b) point-sampled to compute a real blurhash for the placeholder.
 *
 * Styles per `ArtSource`, so the UI's labelling can be checked:
 * - provider: poster-like composition with title (stands in for TMDB art)
 * - frame: a letterboxed "frame from the file" with a typeset title (no AI)
 * - generated: softer painterly gradients (stands in for opt-in genAI art)
 */
import { encode } from 'blurhash';

type RGB = [number, number, number];

type Layer =
  | { t: 'bg'; top: RGB; bottom: RGB }
  | { t: 'circle'; cx: number; cy: number; r: number; c: RGB; a?: number }
  | { t: 'ring'; cx: number; cy: number; r: number; w: number; c: RGB; a?: number }
  | { t: 'rect'; x: number; y: number; w: number; h: number; c: RGB; a?: number }
  | { t: 'tri'; pts: [number, number][]; c: RGB; a?: number }
  | { t: 'glow'; cx: number; cy: number; r: number; c: RGB; a: number };

/** A composition in a unit box (0..1 on both axes). */
interface Comp {
  layers: Layer[];
  ink: RGB;
}

const hex = (h: string): RGB => [parseInt(h.slice(1, 3), 16), parseInt(h.slice(3, 5), 16), parseInt(h.slice(5, 7), 16)];
const css = ([r, g, b]: RGB) => `rgb(${r},${g},${b})`;
const mix = (a: RGB, b: RGB, t: number): RGB => [
  Math.round(a[0] + (b[0] - a[0]) * t),
  Math.round(a[1] + (b[1] - a[1]) * t),
  Math.round(a[2] + (b[2] - a[2]) * t),
];

// Curated cinematic palettes: [shadow, light, ink].
const PALETTES: [string, string, string][] = [
  ['#0f2a3d', '#e9a03b', '#f6ead2'],
  ['#1b1035', '#e0457b', '#f8dfe7'],
  ['#0d3b33', '#d9c26a', '#f1ecd8'],
  ['#3a0d12', '#f06a3f', '#fbe3d2'],
  ['#14213d', '#5fa8d3', '#e8f3fa'],
  ['#1e1e1e', '#c8b8a6', '#f3eee7'],
  ['#2b2d42', '#ef233c', '#edf2f4'],
  ['#1d3557', '#a8dadc', '#f1faee'],
  ['#3d1e6d', '#ffb86b', '#ffeedd'],
  ['#0b3c49', '#ff7f50', '#ffe8dc'],
  ['#263238', '#9ccc65', '#eef5e4'],
  ['#250902', '#ad2831', '#ffdcb0'],
  ['#002a32', '#00b894', '#f6e58d'],
  ['#22223b', '#c9ada7', '#f2e9e4'],
  ['#003049', '#f77f00', '#fcd581'],
  ['#1a1423', '#b75d69', '#f0d8cf'],
  ['#2d2a32', '#ddd92a', '#eae8ff'],
  ['#0a0908', '#c6ac8f', '#eae0d5'],
];

/** FNV-1a 32-bit. */
export function hash32(s: string): number {
  let h = 0x811c9dc5;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 0x01000193);
  }
  return h >>> 0;
}

/** Deterministic PRNG (mulberry32). */
export function rng(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function composition(seed: string, kind: 'poster' | 'wide', style: 'provider' | 'frame' | 'generated'): Comp {
  const r = rng(hash32(seed));
  const [s0, s1, s2] = PALETTES[Math.floor(r() * PALETTES.length)].map(hex) as [RGB, RGB, RGB];
  const shadow = s0;
  const light = s1;
  const ink = s2;
  const deep = mix(shadow, [0, 0, 0], 0.45);
  const layers: Layer[] = [];

  if (style === 'generated') {
    layers.push({ t: 'bg', top: mix(shadow, light, 0.25), bottom: deep });
    for (let i = 0; i < 4; i++) {
      layers.push({ t: 'glow', cx: r(), cy: 0.2 + r() * 0.6, r: 0.35 + r() * 0.35, c: i % 2 ? light : mix(light, ink, 0.5), a: 0.55 });
    }
    return { layers, ink };
  }

  const wide = kind === 'wide';
  // Wide art (backdrops, stills) only uses the atmospheric compositions.
  const options = wide ? [0, 1, 3, 4, 5, 6] : [0, 1, 2, 3, 4, 5, 6];
  const pick = options[Math.floor(r() * options.length)];
  switch (pick) {
    case 0: {
      // Dusk: a sun sinking into a horizon, with its halo.
      const horizon = wide ? 0.64 : 0.6;
      const sx = wide ? 0.3 + r() * 0.4 : 0.5;
      layers.push({ t: 'bg', top: mix(shadow, light, 0.22), bottom: shadow });
      layers.push({ t: 'glow', cx: sx, cy: horizon - 0.04, r: wide ? 0.55 : 0.5, c: light, a: 0.5 });
      layers.push({ t: 'circle', cx: sx, cy: horizon - 0.02, r: wide ? 0.16 : 0.22, c: mix(light, ink, 0.25) });
      layers.push({ t: 'rect', x: 0, y: horizon, w: 1, h: 1 - horizon, c: deep });
      layers.push({ t: 'rect', x: 0, y: horizon, w: 1, h: 0.005, c: ink, a: 0.6 });
      break;
    }
    case 1: {
      // Ridges: layered mountains under a low sun.
      layers.push({ t: 'bg', top: mix(light, ink, 0.35), bottom: mix(shadow, light, 0.3) });
      layers.push({ t: 'glow', cx: 0.7, cy: 0.35, r: 0.5, c: ink, a: 0.5 });
      layers.push({ t: 'circle', cx: 0.7, cy: 0.33, r: wide ? 0.06 : 0.08, c: ink, a: 0.95 });
      const peaks = [0.42 + r() * 0.1, 0.5 + r() * 0.1, 0.62 + r() * 0.08];
      peaks.forEach((py, i) => {
        const px = 0.15 + r() * 0.7;
        layers.push({ t: 'tri', pts: [[px - 0.9, 1.05], [px, py], [px + 0.9, 1.05]], c: mix(mix(shadow, light, 0.35), deep, (i + 1) / 3.2) });
      });
      break;
    }
    case 2: {
      // Concentric rings from a corner (posters only).
      layers.push({ t: 'bg', top: shadow, bottom: deep });
      const cx = r() > 0.5 ? 0 : 1;
      for (let i = 5; i >= 1; i--) {
        layers.push({ t: 'ring', cx, cy: 0.85, r: i * 0.15, w: 0.035, c: i % 2 ? light : ink, a: 0.85 });
      }
      break;
    }
    case 3: {
      // Diagonal split with a small moon.
      layers.push({ t: 'bg', top: light, bottom: mix(light, shadow, 0.3) });
      layers.push({ t: 'tri', pts: [[0, 1], [1, wide ? 0.15 : 0.25], [1, 1]], c: shadow });
      layers.push({ t: 'circle', cx: wide ? 0.22 : 0.28, cy: 0.24, r: wide ? 0.09 : 0.07, c: ink });
      break;
    }
    case 4: {
      // Monolith in haze.
      layers.push({ t: 'bg', top: mix(shadow, light, 0.35), bottom: shadow });
      layers.push({ t: 'glow', cx: 0.5, cy: 0.45, r: 0.55, c: light, a: 0.5 });
      layers.push({ t: 'rect', x: wide ? 0.47 : 0.41, y: wide ? 0.2 : 0.16, w: wide ? 0.06 : 0.18, h: wide ? 0.62 : 0.5, c: deep });
      layers.push({ t: 'rect', x: 0, y: wide ? 0.82 : 0.66, w: 1, h: wide ? 0.18 : 0.34, c: mix(shadow, deep, 0.6) });
      break;
    }
    case 5: {
      // Eclipse.
      layers.push({ t: 'bg', top: deep, bottom: shadow });
      layers.push({ t: 'glow', cx: wide ? 0.62 : 0.5, cy: 0.4, r: 0.45, c: light, a: 0.75 });
      layers.push({ t: 'circle', cx: wide ? 0.62 : 0.5, cy: 0.4, r: wide ? 0.17 : 0.2, c: deep });
      layers.push({ t: 'ring', cx: wide ? 0.62 : 0.5, cy: 0.4, r: wide ? 0.175 : 0.205, w: 0.007, c: ink, a: 0.85 });
      break;
    }
    default: {
      // Projector beam: light from a small source widening across the dark.
      const fromLeft = r() > 0.5;
      const sx = fromLeft ? 0.04 : 0.96;
      layers.push({ t: 'bg', top: deep, bottom: mix(shadow, deep, 0.5) });
      layers.push({ t: 'tri', pts: [[sx, 0.3], [fromLeft ? 1.05 : -0.05, 0.02], [fromLeft ? 1.05 : -0.05, 0.75]], c: light, a: 0.28 });
      layers.push({ t: 'tri', pts: [[sx, 0.3], [fromLeft ? 1.05 : -0.05, 0.16], [fromLeft ? 1.05 : -0.05, 0.5]], c: mix(light, ink, 0.4), a: 0.3 });
      layers.push({ t: 'glow', cx: sx, cy: 0.3, r: 0.18, c: ink, a: 0.9 });
      layers.push({ t: 'circle', cx: sx, cy: 0.3, r: 0.018, c: ink });
    }
  }
  return { layers, ink };
}

// ───────────── sampling (for blurhash) ─────────────

function sample(comp: Comp, x: number, y: number, aspect: number): RGB {
  let c: RGB = [0, 0, 0];
  for (const l of comp.layers) {
    switch (l.t) {
      case 'bg':
        c = mix(l.top, l.bottom, y);
        break;
      case 'circle': {
        const dx = (x - l.cx) * aspect;
        const dy = y - l.cy;
        if (dx * dx + dy * dy <= l.r * l.r) c = mix(c, l.c, l.a ?? 1);
        break;
      }
      case 'ring': {
        const d = Math.hypot((x - l.cx) * aspect, y - l.cy);
        if (Math.abs(d - l.r) <= l.w / 2) c = mix(c, l.c, l.a ?? 1);
        break;
      }
      case 'rect':
        if (x >= l.x && x <= l.x + l.w && y >= l.y && y <= l.y + l.h) c = mix(c, l.c, l.a ?? 1);
        break;
      case 'tri': {
        const [[ax, ay], [bx, by], [cx2, cy2]] = l.pts;
        const s1 = (bx - ax) * (y - ay) - (by - ay) * (x - ax);
        const s2 = (cx2 - bx) * (y - by) - (cy2 - by) * (x - bx);
        const s3 = (ax - cx2) * (y - cy2) - (ay - cy2) * (x - cx2);
        if ((s1 >= 0 && s2 >= 0 && s3 >= 0) || (s1 <= 0 && s2 <= 0 && s3 <= 0)) c = mix(c, l.c, l.a ?? 1);
        break;
      }
      case 'glow': {
        const d = Math.hypot((x - l.cx) * aspect, y - l.cy) / l.r;
        if (d < 1) c = mix(c, l.c, l.a * (1 - d) * (1 - d));
        break;
      }
    }
  }
  return c;
}

/** Real blurhash of the composition (what a server computes at ingest). */
function blurhashOf(comp: Comp, w: number, h: number, cx: number, cy: number): string {
  const px = new Uint8ClampedArray(w * h * 4);
  const aspect = w / h;
  for (let j = 0; j < h; j++) {
    for (let i = 0; i < w; i++) {
      const [r, g, b] = sample(comp, (i + 0.5) / w, (j + 0.5) / h, aspect);
      const o = (j * w + i) * 4;
      px[o] = r;
      px[o + 1] = g;
      px[o + 2] = b;
      px[o + 3] = 255;
    }
  }
  return encode(px, w, h, cx, cy);
}

// ───────────── SVG rendering ─────────────

function esc(s: string): string {
  return s.replace(/[&<>"']/g, (ch) => `&#${ch.charCodeAt(0)};`);
}

function layersSvg(comp: Comp, W: number, H: number, id: string): string {
  const out: string[] = [];
  const defs: string[] = [];
  let g = 0;
  for (const l of comp.layers) {
    switch (l.t) {
      case 'bg':
        defs.push(`<linearGradient id="${id}b" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="${css(l.top)}"/><stop offset="1" stop-color="${css(l.bottom)}"/></linearGradient>`);
        out.push(`<rect width="${W}" height="${H}" fill="url(#${id}b)"/>`);
        break;
      case 'circle':
        out.push(`<circle cx="${l.cx * W}" cy="${l.cy * H}" r="${l.r * H}" fill="${css(l.c)}" fill-opacity="${l.a ?? 1}"/>`);
        break;
      case 'ring':
        out.push(`<circle cx="${l.cx * W}" cy="${l.cy * H}" r="${l.r * H}" fill="none" stroke="${css(l.c)}" stroke-opacity="${l.a ?? 1}" stroke-width="${l.w * H}"/>`);
        break;
      case 'rect':
        out.push(`<rect x="${l.x * W}" y="${l.y * H}" width="${l.w * W}" height="${l.h * H}" fill="${css(l.c)}" fill-opacity="${l.a ?? 1}"/>`);
        break;
      case 'tri':
        out.push(`<polygon points="${l.pts.map(([x, y]) => `${x * W},${y * H}`).join(' ')}" fill="${css(l.c)}" fill-opacity="${l.a ?? 1}"/>`);
        break;
      case 'glow': {
        const gid = `${id}g${g++}`;
        defs.push(`<radialGradient id="${gid}"><stop offset="0" stop-color="${css(l.c)}" stop-opacity="${l.a}"/><stop offset="1" stop-color="${css(l.c)}" stop-opacity="0"/></radialGradient>`);
        out.push(`<circle cx="${l.cx * W}" cy="${l.cy * H}" r="${l.r * H}" fill="url(#${gid})"/>`);
        break;
      }
    }
  }
  return `<defs>${defs.join('')}</defs>${out.join('')}`;
}

/** Greedy word wrap by character budget. */
function wrap(text: string, perLine: number, maxLines: number): string[] {
  const words = text.split(/\s+/);
  const lines: string[] = [];
  let cur = '';
  for (const w of words) {
    if (!cur) cur = w;
    else if ((cur + ' ' + w).length <= perLine) cur += ' ' + w;
    else {
      lines.push(cur);
      cur = w;
    }
  }
  if (cur) lines.push(cur);
  if (lines.length > maxLines) {
    const kept = lines.slice(0, maxLines);
    kept[maxLines - 1] = `${kept[maxLines - 1].replace(/[,:;.]$/, '')}…`;
    return kept;
  }
  return lines;
}

// SVG-as-image cannot load web fonts, so use the best condensed system faces.
const FONT = `'Avenir Next Condensed','Helvetica Neue','Arial Narrow',Arial,sans-serif`;

export interface ArtSpec {
  seed: string;
  title: string;
  year: number | null;
  source: 'provider' | 'local' | 'frame' | 'generated' | 'custom';
}

/** 2:3 poster SVG. */
export function posterSvg(spec: ArtSpec): string {
  const W = 400;
  const H = 600;
  const style = spec.source === 'generated' ? 'generated' : spec.source === 'frame' ? 'frame' : 'provider';
  const id = `a${hash32(spec.seed).toString(36)}`;
  if (style === 'frame') {
    // A letterboxed frame from the file, title typeset underneath (no AI).
    const comp = composition(spec.seed, 'wide', 'provider');
    const frame = layersSvg(comp, W, 225, id);
    const lines = wrap(spec.title.toUpperCase(), 16, 3);
    const longest = Math.max(...lines.map((l) => l.length));
    const size = Math.min(lines.length > 2 ? 38 : 46, Math.floor(344 / (0.56 * longest)));
    const text = lines
      // First baseline sits a cap-height below the year line (y 372), so they never touch.
      .map((ln, i) => `<text x="28" y="${386 + size * 0.8 + i * (size + 2)}" font-family="${FONT}" font-weight="700" font-size="${size}" fill="#efeae2" letter-spacing="1">${esc(ln)}</text>`)
      .join('');
    return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${W} ${H}" width="${W}" height="${H}"><rect width="${W}" height="${H}" fill="#101012"/><svg y="120" width="${W}" height="225" viewBox="0 0 ${W} 225">${frame}</svg>${text}<text x="28" y="372" font-family="${FONT}" font-size="15" fill="#9c9aa3" letter-spacing="3">${spec.year ?? ''}</text></svg>`;
  }
  const comp = composition(spec.seed, 'poster', style);
  const lines = wrap(spec.title.toUpperCase(), spec.title.length > 22 ? 14 : 11, 3);
  // Condensed bold caps run ~0.56 em per glyph; shrink so the longest line fits 340 px.
  const longest = Math.max(...lines.map((l) => l.length));
  const size = Math.min(lines.length >= 3 ? 46 : lines.length === 2 ? 56 : spec.title.length > 8 ? 60 : 72, Math.floor(340 / (0.56 * longest)));
  const top = H - 36 - (lines.length - 1) * (size * 0.95);
  const text = lines
    .map((ln, i) => `<text x="30" y="${top + i * size * 0.95}" font-family="${FONT}" font-weight="800" font-size="${size}" fill="${css(comp.ink)}" letter-spacing="0.5">${esc(ln)}</text>`)
    .join('');
  const year = spec.year
    ? `<text x="32" y="${top - size - 4}" font-family="${FONT}" font-size="16" font-weight="600" fill="${css(comp.ink)}" fill-opacity="0.8" letter-spacing="4">${spec.year}</text>`
    : '';
  const shade = `<defs><linearGradient id="${id}s" x1="0" y1="0" x2="0" y2="1"><stop offset="0.55" stop-color="#000" stop-opacity="0"/><stop offset="1" stop-color="#000" stop-opacity="0.55"/></linearGradient></defs><rect width="${W}" height="${H}" fill="url(#${id}s)"/>`;
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${W} ${H}" width="${W}" height="${H}">${layersSvg(comp, W, H, id)}${shade}${year}${text}</svg>`;
}

/** 16:9 backdrop or episode still SVG (textless, like provider backdrops). */
export function wideSvg(spec: Pick<ArtSpec, 'seed' | 'source'>): string {
  const W = 1280;
  const H = 720;
  const style = spec.source === 'generated' ? 'generated' : 'provider';
  const comp = composition(spec.seed, 'wide', style);
  const id = `w${hash32(spec.seed).toString(36)}`;
  // A soft vignette makes the flat shapes read as a lit frame.
  const vignette = `<defs><radialGradient id="${id}v" cx="0.5" cy="0.45" r="0.75"><stop offset="0.55" stop-color="#000" stop-opacity="0"/><stop offset="1" stop-color="#000" stop-opacity="0.55"/></radialGradient></defs><rect width="${W}" height="${H}" fill="url(#${id}v)"/>`;
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${W} ${H}" width="${W}" height="${H}" preserveAspectRatio="xMidYMid slice">${layersSvg(comp, W, H, id)}${vignette}</svg>`;
}

const hashCache = new Map<string, string>();

/** Blurhash for a poster (2:3) or wide (16:9) art of `seed`. Memoised. */
export function artBlurhash(seed: string, kind: 'poster' | 'wide', source: ArtSpec['source']): string {
  const key = `${kind}|${source}|${seed}`;
  let h = hashCache.get(key);
  if (!h) {
    const style = source === 'generated' ? 'generated' : 'provider';
    if (kind === 'poster' && source === 'frame') {
      // The dark mat with the frame's middle colour where the frame sits (y 120..345 of 600).
      const frameMid = sample(composition(seed, 'wide', 'provider'), 0.5, 0.5, 16 / 9);
      const mat: RGB = [16, 16, 18];
      const comp: Comp = {
        layers: [
          { t: 'bg', top: mat, bottom: mat },
          { t: 'rect', x: 0, y: 0.2, w: 1, h: 0.375, c: frameMid },
        ],
        ink: [239, 234, 226],
      };
      h = blurhashOf(comp, 8, 12, 3, 4);
    } else {
      h = kind === 'poster' ? blurhashOf(composition(seed, 'poster', style), 8, 12, 3, 4) : blurhashOf(composition(seed, 'wide', style), 16, 9, 4, 3);
    }
    hashCache.set(key, h);
  }
  return h;
}
