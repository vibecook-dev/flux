/**
 * Posters for the hero's wall, painted on a canvas: the same seven motifs and
 * palettes as the app's demo art (web/src/mock/art.ts), under invented titles,
 * so the page needs no images and shows nobody's real posters.
 */

type RGB = [number, number, number];

// [shadow, light, ink], as in the app's mock art.
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

/** Invented titles, in the demo library's own vocabulary. */
export const TITLES: [string, number][] = [
  ['Burning Canal', 2026],
  ['The Iron Engine', 1987],
  ['Quiet Meridian', 2019],
  ['Velvet Lantern', 1964],
  ['Paper Orchard', 2003],
  ['Wandering Tide', 1978],
  ['Neon Harbor', 2024],
  ['The Broken Archive', 1999],
  ['Hollow Summit', 1959],
  ['Distant Signal', 2011],
  ['Midnight Republic', 1983],
  ['Glass Garden', 2016],
  ['Silent Engine', 1971],
  ['The Last Lantern of Summer', 2008],
  ['Electric Canal', 1992],
  ['Golden Frontier', 1966],
  ['Northern Station', 2021],
  ['Lost Meridian', 1975],
  ['Savage Desert', 1988],
  ['The Amber Bridge', 1997],
  ['Iron Kingdom', 2014],
  ['A Quiet Orbit', 2025],
  ['Crimson Lantern', 1961],
  ['The Glass Republic', 2006],
  ['Distant Harvest', 1980],
  ['Midnight Circuit', 2022],
  ['Velvet Desert', 1993],
  ['Hollow Tide', 2010],
  ['The Wandering Archive', 1969],
  ['Paper Kingdom', 2001],
  ['Electric Summit', 1985],
  ['Neon Orchard', 2018],
];

const hex = (h: string): RGB => [parseInt(h.slice(1, 3), 16), parseInt(h.slice(3, 5), 16), parseInt(h.slice(5, 7), 16)];
const mix = (a: RGB, b: RGB, t: number): RGB => [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t];
const css = ([r, g, b]: RGB, a = 1) => `rgba(${Math.round(r)},${Math.round(g)},${Math.round(b)},${a})`;

function rng(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function hash(s: string): number {
  let h = 0x811c9dc5;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 0x01000193);
  }
  return h >>> 0;
}

export interface Poster {
  title: string;
  year: number;
  /** The poster's average colour, shown until the art is painted. */
  average: RGB;
  paint(ctx: CanvasRenderingContext2D, w: number, h: number): void;
}

export const TITLE_FONT = '"Archivo Variable", "Archivo", "Arial Narrow", sans-serif';

export function poster(title: string, year: number, index: number): Poster {
  const r = rng(hash(title) ^ (index * 2654435761));
  const [shadow, light, ink] = PALETTES[(hash(title) + index * 7) % PALETTES.length].map(hex) as [RGB, RGB, RGB];
  const deep = mix(shadow, [0, 0, 0], 0.45);
  const motif = index % 7;
  const average = mix(mix(shadow, light, motif === 3 ? 0.55 : 0.25), deep, 0.25);

  const paint = (ctx: CanvasRenderingContext2D, w: number, h: number) => {
    const X = (v: number) => v * w;
    const Y = (v: number) => v * h;
    const grad = (top: RGB, bottom: RGB) => {
      const g = ctx.createLinearGradient(0, 0, 0, h);
      g.addColorStop(0, css(top));
      g.addColorStop(1, css(bottom));
      ctx.fillStyle = g;
      ctx.fillRect(0, 0, w, h);
    };
    const glow = (cx: number, cy: number, rad: number, c: RGB, a: number) => {
      const g = ctx.createRadialGradient(X(cx), Y(cy), 0, X(cx), Y(cy), rad * w);
      g.addColorStop(0, css(c, a));
      g.addColorStop(1, css(c, 0));
      ctx.fillStyle = g;
      ctx.fillRect(0, 0, w, h);
    };
    const circle = (cx: number, cy: number, rad: number, c: RGB, a = 1) => {
      ctx.beginPath();
      ctx.arc(X(cx), Y(cy), rad * w, 0, Math.PI * 2);
      ctx.fillStyle = css(c, a);
      ctx.fill();
    };
    const ring = (cx: number, cy: number, rad: number, lw: number, c: RGB, a = 1) => {
      ctx.beginPath();
      ctx.arc(X(cx), Y(cy), rad * w, 0, Math.PI * 2);
      ctx.strokeStyle = css(c, a);
      ctx.lineWidth = lw * w;
      ctx.stroke();
    };
    const rect = (x: number, y: number, rw: number, rh: number, c: RGB, a = 1) => {
      ctx.fillStyle = css(c, a);
      ctx.fillRect(X(x), Y(y), X(rw), Y(rh));
    };
    const tri = (pts: [number, number][], c: RGB, a = 1) => {
      ctx.beginPath();
      pts.forEach(([x, y], i) => (i ? ctx.lineTo(X(x), Y(y)) : ctx.moveTo(X(x), Y(y))));
      ctx.closePath();
      ctx.fillStyle = css(c, a);
      ctx.fill();
    };

    switch (motif) {
      case 0: {
        // Dusk: a sun sinking into a horizon.
        const horizon = 0.56;
        grad(mix(shadow, light, 0.22), shadow);
        glow(0.5, horizon - 0.04, 0.55, light, 0.55);
        circle(0.5, horizon - 0.02, 0.22, mix(light, ink, 0.25));
        rect(0, horizon, 1, 1 - horizon, deep);
        rect(0, horizon, 1, 0.004, ink, 0.6);
        break;
      }
      case 1: {
        // Ridges under a low sun.
        grad(mix(light, ink, 0.35), mix(shadow, light, 0.3));
        glow(0.7, 0.3, 0.5, ink, 0.5);
        circle(0.7, 0.28, 0.08, ink, 0.95);
        const peaks = [0.38 + r() * 0.1, 0.46 + r() * 0.1, 0.56 + r() * 0.08];
        peaks.forEach((py, i) => {
          const px = 0.15 + r() * 0.7;
          tri([[px - 0.9, 1.05], [px, py], [px + 0.9, 1.05]], mix(mix(shadow, light, 0.35), deep, (i + 1) / 3.2));
        });
        break;
      }
      case 2: {
        // Concentric rings from a corner.
        grad(shadow, deep);
        const cx = r() > 0.5 ? 0 : 1;
        for (let i = 6; i >= 1; i--) ring(cx, 0.78, i * 0.15, 0.035, i % 2 ? light : ink, 0.85);
        break;
      }
      case 3: {
        // Diagonal split with a small moon.
        grad(light, mix(light, shadow, 0.3));
        tri([[0, 1], [1, 0.22], [1, 1]], shadow);
        circle(0.28, 0.22, 0.07, ink);
        break;
      }
      case 4: {
        // Monolith in haze.
        grad(mix(shadow, light, 0.35), shadow);
        glow(0.5, 0.4, 0.55, light, 0.5);
        rect(0.41, 0.12, 0.18, 0.46, deep);
        rect(0, 0.58, 1, 0.42, mix(shadow, deep, 0.6));
        break;
      }
      case 5: {
        // Eclipse.
        grad(deep, shadow);
        glow(0.5, 0.36, 0.48, light, 0.8);
        circle(0.5, 0.36, 0.2, deep);
        ring(0.5, 0.36, 0.205, 0.007, ink, 0.85);
        break;
      }
      default: {
        // Projector beam.
        const fromLeft = r() > 0.5;
        const sx = fromLeft ? 0.04 : 0.96;
        const far = fromLeft ? 1.05 : -0.05;
        grad(deep, mix(shadow, deep, 0.5));
        tri([[sx, 0.28], [far, 0.0], [far, 0.7]], light, 0.28);
        tri([[sx, 0.28], [far, 0.14], [far, 0.46]], mix(light, ink, 0.4), 0.3);
        glow(sx, 0.28, 0.18, ink, 0.9);
        circle(sx, 0.28, 0.018, ink);
      }
    }

    // A dark foot for the type, as a printed one-sheet would have.
    const foot = ctx.createLinearGradient(0, h * 0.58, 0, h);
    foot.addColorStop(0, css(deep, 0));
    foot.addColorStop(1, css(deep, 0.78));
    ctx.fillStyle = foot;
    ctx.fillRect(0, h * 0.58, w, h * 0.42);

    // Type: the year spaced out, the title condensed and heavy.
    const pad = w * 0.085;
    const maxW = w - pad * 2;
    const words = title.toUpperCase().split(' ');
    let size = w * 0.17;
    let lines: string[] = [];
    const setFont = (s: number) => {
      ctx.font = `800 condensed ${s}px ${TITLE_FONT}`;
    };
    for (let attempt = 0; attempt < 8; attempt++) {
      setFont(size);
      lines = [];
      let line = '';
      for (const word of words) {
        const next = line ? `${line} ${word}` : word;
        if (ctx.measureText(next).width <= maxW || !line) line = next;
        else {
          lines.push(line);
          line = word;
        }
      }
      if (line) lines.push(line);
      const widest = Math.max(...lines.map((l) => ctx.measureText(l).width));
      if (lines.length <= 3 && widest <= maxW) break;
      size *= 0.88;
    }
    const lh = size * 0.94;
    const bottom = h - pad * 1.05;
    ctx.fillStyle = css(ink);
    ctx.textBaseline = 'alphabetic';
    lines.forEach((l, i) => ctx.fillText(l, pad, bottom - (lines.length - 1 - i) * lh));
    const ys = Math.max(9, w * 0.042);
    ctx.font = `600 ${ys}px ${TITLE_FONT}`;
    ctx.fillStyle = css(ink, 0.82);
    const spaced = String(year).split('').join(String.fromCharCode(8202));
    ctx.fillText(spaced, pad, bottom - lines.length * lh - ys * 0.15);
  };

  return { title, year, average, paint };
}

export const POSTERS: Poster[] = TITLES.map(([t, y], i) => poster(t, y, i));
