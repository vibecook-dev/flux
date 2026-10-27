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

// The library holds 13,000 titles and each needs a placeholder: sampling and
// encoding run for every one as its page arrives, so both avoid allocating
// (the colour is three numbers, not a new array per layer) and the encoder uses
// lookup tables instead of a pow() and two cos() per pixel per component. The
// hashes are the same as the blurhash package's `encode` gives.

/** The colour at (x, y) into out[o..o+2], blending layer by layer and rounding as `mix` does. */
function sampleInto(comp: Comp, x: number, y: number, aspect: number, out: Uint8Array, o: number): void {
  let r = 0;
  let g = 0;
  let b = 0;
  for (const l of comp.layers) {
    let c: RGB | null = null;
    let t = 0;
    switch (l.t) {
      case 'bg':
        r = Math.round(l.top[0] + (l.bottom[0] - l.top[0]) * y);
        g = Math.round(l.top[1] + (l.bottom[1] - l.top[1]) * y);
        b = Math.round(l.top[2] + (l.bottom[2] - l.top[2]) * y);
        break;
      case 'circle': {
        const dx = (x - l.cx) * aspect;
        const dy = y - l.cy;
        if (dx * dx + dy * dy <= l.r * l.r) {
          c = l.c;
          t = l.a ?? 1;
        }
        break;
      }
      case 'ring': {
        const dx = (x - l.cx) * aspect;
        const dy = y - l.cy;
        if (Math.abs(Math.sqrt(dx * dx + dy * dy) - l.r) <= l.w / 2) {
          c = l.c;
          t = l.a ?? 1;
        }
        break;
      }
      case 'rect':
        if (x >= l.x && x <= l.x + l.w && y >= l.y && y <= l.y + l.h) {
          c = l.c;
          t = l.a ?? 1;
        }
        break;
      case 'tri': {
        const p = l.pts;
        const ax = p[0][0];
        const ay = p[0][1];
        const bx = p[1][0];
        const by = p[1][1];
        const cx2 = p[2][0];
        const cy2 = p[2][1];
        const s1 = (bx - ax) * (y - ay) - (by - ay) * (x - ax);
        const s2 = (cx2 - bx) * (y - by) - (cy2 - by) * (x - bx);
        const s3 = (ax - cx2) * (y - cy2) - (ay - cy2) * (x - cx2);
        if ((s1 >= 0 && s2 >= 0 && s3 >= 0) || (s1 <= 0 && s2 <= 0 && s3 <= 0)) {
          c = l.c;
          t = l.a ?? 1;
        }
        break;
      }
      case 'glow': {
        const dx = (x - l.cx) * aspect;
        const dy = y - l.cy;
        const d = Math.sqrt(dx * dx + dy * dy) / l.r;
        if (d < 1) {
          c = l.c;
          t = l.a * (1 - d) * (1 - d);
        }
        break;
      }
    }
    if (c) {
      r = Math.round(r + (c[0] - r) * t);
      g = Math.round(g + (c[1] - g) * t);
      b = Math.round(b + (c[2] - b) * t);
    }
  }
  out[o] = r;
  out[o + 1] = g;
  out[o + 2] = b;
}

function sample(comp: Comp, x: number, y: number, aspect: number): RGB {
  const px = new Uint8Array(3);
  sampleInto(comp, x, y, aspect, px, 0);
  return [px[0], px[1], px[2]];
}

const B83 = '0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz#$%*+,-.:;=?@[]^_{|}~';
const enc83 = (n: number, length: number): string => {
  let s = '';
  for (let i = 1; i <= length; i++) s += B83[Math.floor(Math.floor(n) / 83 ** (length - i)) % 83];
  return s;
};
/** sRGB byte → linear light. */
const LINEAR = Float64Array.from({ length: 256 }, (_, v) => (v / 255 <= 0.04045 ? v / 255 / 12.92 : ((v / 255 + 0.055) / 1.055) ** 2.4));
const toSrgb = (value: number): number => {
  const v = Math.max(0, Math.min(1, value));
  return v <= 0.0031308 ? Math.trunc(v * 12.92 * 255 + 0.5) : Math.trunc((1.055 * v ** (1 / 2.4) - 0.055) * 255 + 0.5);
};
const signPow = (v: number, e: number): number => Math.sign(v) * Math.abs(v) ** e;
/** cos(π·k·i/size) for k < n and i < size, per (n, size). */
const cosines = new Map<string, Float64Array>();
function cosTable(n: number, size: number): Float64Array {
  const key = `${n}x${size}`;
  let t = cosines.get(key);
  if (!t) {
    t = new Float64Array(n * size);
    for (let k = 0; k < n; k++) for (let i = 0; i < size; i++) t[k * size + i] = Math.cos((Math.PI * k * i) / size);
    cosines.set(key, t);
  }
  return t;
}

/** Real blurhash of the composition (what a server computes at ingest). */
function blurhashOf(comp: Comp, w: number, h: number, cx: number, cy: number): string {
  const n = w * h;
  const px = new Uint8Array(n * 3);
  const aspect = w / h;
  for (let j = 0; j < h; j++) for (let i = 0; i < w; i++) sampleInto(comp, (i + 0.5) / w, (j + 0.5) / h, aspect, px, (j * w + i) * 3);
  const lin = new Float64Array(n * 3);
  for (let k = 0; k < n * 3; k++) lin[k] = LINEAR[px[k]];
  const cosX = cosTable(cx, w);
  const cosY = cosTable(cy, h);
  const factors: number[][] = [];
  for (let y = 0; y < cy; y++) {
    for (let x = 0; x < cx; x++) {
      const norm = x === 0 && y === 0 ? 1 : 2;
      let r = 0;
      let g = 0;
      let b = 0;
      for (let j = 0; j < h; j++) {
        const fy = norm * cosY[y * h + j];
        for (let i = 0; i < w; i++) {
          const basis = fy * cosX[x * w + i];
          const o = (j * w + i) * 3;
          r += basis * lin[o];
          g += basis * lin[o + 1];
          b += basis * lin[o + 2];
        }
      }
      factors.push([r / n, g / n, b / n]);
    }
  }
  const [dc, ...ac] = factors;
  let hash = enc83(cx - 1 + (cy - 1) * 9, 1);
  let max = 1;
  if (ac.length) {
    const actual = Math.max(...ac.map((f) => Math.max(...f)));
    const quantised = Math.floor(Math.max(0, Math.min(82, Math.floor(actual * 166 - 0.5))));
    max = (quantised + 1) / 166;
    hash += enc83(quantised, 1);
  } else hash += enc83(0, 1);
  hash += enc83((toSrgb(dc[0]) << 16) + (toSrgb(dc[1]) << 8) + toSrgb(dc[2]), 4);
  const q = (v: number) => Math.floor(Math.max(0, Math.min(18, Math.floor(signPow(v / max, 0.5) * 9 + 9.5))));
  for (const f of ac) hash += enc83(q(f[0]) * 19 * 19 + q(f[1]) * 19 + q(f[2]), 2);
  return hash;
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

/** Native sizes: what an `?w=`-less request gets, and the space art is laid out in. */
export const ART_SIZE = { poster: [400, 600], wide: [1280, 720] } as const;

/** A line of type on a poster. */
interface Line {
  s: string;
  x: number;
  y: number;
  size: number;
  weight: number;
  fill: string;
  opacity: number;
  spacing: number;
}

/**
 * What a poster shows, worked out once for both renderers: a composition filling
 * the poster, or (frame posters) a 16:9 frame letterboxed at y 120..345, then
 * the type in drawing order.
 */
function posterPlan(spec: ArtSpec): { comp: Comp; frame: boolean; lines: Line[] } {
  const H = ART_SIZE.poster[1];
  const style = spec.source === 'generated' ? 'generated' : spec.source === 'frame' ? 'frame' : 'provider';
  if (style === 'frame') {
    // A letterboxed frame from the file, title typeset underneath (no AI).
    const comp = composition(spec.seed, 'wide', 'provider');
    const words = wrap(spec.title.toUpperCase(), 16, 3);
    const longest = Math.max(...words.map((l) => l.length));
    const size = Math.min(words.length > 2 ? 38 : 46, Math.floor(344 / (0.56 * longest)));
    const lines: Line[] = words.map((s, i) =>
      // First baseline sits a cap-height below the year line (y 372), so they never touch.
      ({ s, x: 28, y: 386 + size * 0.8 + i * (size + 2), size, weight: 700, fill: '#efeae2', opacity: 1, spacing: 1 }),
    );
    lines.push({ s: `${spec.year ?? ''}`, x: 28, y: 372, size: 15, weight: 400, fill: '#9c9aa3', opacity: 1, spacing: 3 });
    return { comp, frame: true, lines };
  }
  const comp = composition(spec.seed, 'poster', style);
  const words = wrap(spec.title.toUpperCase(), spec.title.length > 22 ? 14 : 11, 3);
  // Condensed bold caps run ~0.56 em per glyph; shrink so the longest line fits 340 px.
  const longest = Math.max(...words.map((l) => l.length));
  const size = Math.min(words.length >= 3 ? 46 : words.length === 2 ? 56 : spec.title.length > 8 ? 60 : 72, Math.floor(340 / (0.56 * longest)));
  const top = H - 36 - (words.length - 1) * (size * 0.95);
  const lines: Line[] = [];
  if (spec.year) lines.push({ s: String(spec.year), x: 32, y: top - size - 4, size: 16, weight: 600, fill: css(comp.ink), opacity: 0.8, spacing: 4 });
  for (const [i, s] of words.entries()) lines.push({ s, x: 30, y: top + i * size * 0.95, size, weight: 800, fill: css(comp.ink), opacity: 1, spacing: 0.5 });
  return { comp, frame: false, lines };
}

const lineSvg = (l: Line) =>
  `<text x="${l.x}" y="${l.y}" font-family="${FONT}" font-weight="${l.weight}" font-size="${l.size}" fill="${l.fill}"${l.opacity < 1 ? ` fill-opacity="${l.opacity}"` : ''} letter-spacing="${l.spacing}">${esc(l.s)}</text>`;

/** 2:3 poster SVG. */
export function posterSvg(spec: ArtSpec): string {
  const [W, H] = ART_SIZE.poster;
  const id = `a${hash32(spec.seed).toString(36)}`;
  const { comp, frame, lines } = posterPlan(spec);
  const text = lines.map(lineSvg).join('');
  if (frame) {
    return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${W} ${H}" width="${W}" height="${H}"><rect width="${W}" height="${H}" fill="#101012"/><svg y="120" width="${W}" height="225" viewBox="0 0 ${W} 225">${layersSvg(comp, W, 225, id)}</svg>${text}</svg>`;
  }
  const shade = `<defs><linearGradient id="${id}s" x1="0" y1="0" x2="0" y2="1"><stop offset="0.55" stop-color="#000" stop-opacity="0"/><stop offset="1" stop-color="#000" stop-opacity="0.55"/></linearGradient></defs><rect width="${W}" height="${H}" fill="url(#${id}s)"/>`;
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${W} ${H}" width="${W}" height="${H}">${layersSvg(comp, W, H, id)}${shade}${text}</svg>`;
}

const wideComp = (spec: Pick<ArtSpec, 'seed' | 'source'>) => composition(spec.seed, 'wide', spec.source === 'generated' ? 'generated' : 'provider');

/** 16:9 backdrop or episode still SVG (textless, like provider backdrops). */
export function wideSvg(spec: Pick<ArtSpec, 'seed' | 'source'>): string {
  const [W, H] = ART_SIZE.wide;
  const id = `w${hash32(spec.seed).toString(36)}`;
  // A soft vignette makes the flat shapes read as a lit frame.
  const vignette = `<defs><radialGradient id="${id}v" cx="0.5" cy="0.45" r="0.75"><stop offset="0.55" stop-color="#000" stop-opacity="0"/><stop offset="1" stop-color="#000" stop-opacity="0.55"/></radialGradient></defs><rect width="${W}" height="${H}" fill="url(#${id}v)"/>`;
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${W} ${H}" width="${W}" height="${H}" preserveAspectRatio="xMidYMid slice">${layersSvg(wideComp(spec), W, H, id)}${vignette}</svg>`;
}

// ───────────── canvas rendering ─────────────
// The same pictures drawn on a canvas, so a worker can encode them as JPEG
// (art-raster.ts): a browser decodes a JPEG off the main thread, as it does a
// real server's art, where an SVG must be rasterised on the main thread.

type Ctx2D = OffscreenCanvasRenderingContext2D | CanvasRenderingContext2D;

const rgba = ([r, g, b]: RGB, a: number) => `rgba(${r},${g},${b},${a})`;

function drawLayers(ctx: Ctx2D, comp: Comp, W: number, H: number): void {
  for (const l of comp.layers) {
    switch (l.t) {
      case 'bg': {
        const g = ctx.createLinearGradient(0, 0, 0, H);
        g.addColorStop(0, css(l.top));
        g.addColorStop(1, css(l.bottom));
        ctx.fillStyle = g;
        ctx.fillRect(0, 0, W, H);
        break;
      }
      case 'circle':
        ctx.fillStyle = rgba(l.c, l.a ?? 1);
        ctx.beginPath();
        ctx.arc(l.cx * W, l.cy * H, l.r * H, 0, Math.PI * 2);
        ctx.fill();
        break;
      case 'ring':
        ctx.strokeStyle = rgba(l.c, l.a ?? 1);
        ctx.lineWidth = l.w * H;
        ctx.beginPath();
        ctx.arc(l.cx * W, l.cy * H, l.r * H, 0, Math.PI * 2);
        ctx.stroke();
        break;
      case 'rect':
        ctx.fillStyle = rgba(l.c, l.a ?? 1);
        ctx.fillRect(l.x * W, l.y * H, l.w * W, l.h * H);
        break;
      case 'tri':
        ctx.fillStyle = rgba(l.c, l.a ?? 1);
        ctx.beginPath();
        for (const [x, y] of l.pts) ctx.lineTo(x * W, y * H);
        ctx.closePath();
        ctx.fill();
        break;
      case 'glow': {
        const x = l.cx * W;
        const y = l.cy * H;
        const r = l.r * H;
        const g = ctx.createRadialGradient(x, y, 0, x, y, r);
        g.addColorStop(0, rgba(l.c, l.a));
        g.addColorStop(1, rgba(l.c, 0));
        ctx.fillStyle = g;
        ctx.beginPath();
        ctx.arc(x, y, r, 0, Math.PI * 2);
        ctx.fill();
        break;
      }
    }
  }
}

/** A dark ramp over `0..1` of a unit box, transparent until `from` (the poster shade and backdrop vignette). */
function darken(g: CanvasGradient, from: number): CanvasGradient {
  g.addColorStop(from, 'rgba(0,0,0,0)');
  g.addColorStop(1, 'rgba(0,0,0,0.55)');
  return g;
}

/** What one art request draws. */
export interface ArtDraw {
  kind: 'poster' | 'wide';
  spec: ArtSpec;
}

/** Draw `art` in its native size's space (`ART_SIZE`); scale the context first for another size. */
export function drawArt(ctx: Ctx2D, art: ArtDraw): void {
  const [W, H] = ART_SIZE[art.kind];
  if (art.kind === 'wide') {
    drawLayers(ctx, wideComp(art.spec), W, H);
    // The vignette's circle is in the box's units, so it is an ellipse on a 16:9 frame.
    ctx.save();
    ctx.scale(W, H);
    ctx.fillStyle = darken(ctx.createRadialGradient(0.5, 0.45, 0, 0.5, 0.45, 0.75), 0.55);
    ctx.fillRect(0, 0, 1, 1);
    ctx.restore();
    return;
  }
  const { comp, frame, lines } = posterPlan(art.spec);
  if (frame) {
    ctx.fillStyle = '#101012';
    ctx.fillRect(0, 0, W, H);
    ctx.save();
    ctx.translate(0, 120);
    ctx.beginPath();
    ctx.rect(0, 0, W, 225);
    ctx.clip();
    drawLayers(ctx, comp, W, 225);
    ctx.restore();
  } else {
    drawLayers(ctx, comp, W, H);
    ctx.fillStyle = darken(ctx.createLinearGradient(0, 0, 0, H), 0.55);
    ctx.fillRect(0, 0, W, H);
  }
  for (const l of lines) {
    ctx.font = `${l.weight} ${l.size}px ${FONT}`;
    ctx.letterSpacing = `${l.spacing}px`;
    ctx.globalAlpha = l.opacity;
    ctx.fillStyle = l.fill;
    ctx.fillText(l.s, l.x, l.y);
  }
  ctx.globalAlpha = 1;
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
