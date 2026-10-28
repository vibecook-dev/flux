// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { hideIn, isFlat, lerpQuad, onScreen, projectedQuad, quadArea, quadBox, quadCentre, quadSize, quadTransform, rectQuad, spotIn, srcFor } from './flight';
import type { LensView, Quad } from './types';

/** Where a point of the box lands under a transform made by `quadTransform` (origin 0 0). */
function place(t: string, x: number, y: number): { x: number; y: number } {
  const m3 = /matrix3d\(([^)]+)\)/.exec(t);
  if (m3) {
    const m = m3[1].split(',').map(Number);
    const w = m[3] * x + m[7] * y + m[15];
    return { x: (m[0] * x + m[4] * y + m[12]) / w, y: (m[1] * x + m[5] * y + m[13]) / w };
  }
  const ts = /translate\(([-\d.e]+)px, ([-\d.e]+)px\) scale\(([-\d.e]+), ([-\d.e]+)\)/.exec(t);
  if (!ts) throw new Error(`not a flight transform: ${t}`);
  return { x: +ts[1] + +ts[3] * x, y: +ts[2] + +ts[4] * y };
}

function expectCorners(t: string, w: number, h: number, q: Quad) {
  const got = [place(t, 0, 0), place(t, w, 0), place(t, w, h), place(t, 0, h)];
  got.forEach((p, i) => {
    expect(p.x).toBeCloseTo(q[i].x, 4);
    expect(p.y).toBeCloseTo(q[i].y, 4);
  });
}

describe('quadTransform', () => {
  it('maps the ghost onto an upright rectangle with a translate and a scale', () => {
    const q = rectQuad({ left: 244, top: 146, width: 177.6, height: 266.4 });
    const t = quadTransform(360, 540, q);
    expect(t).toMatch(/^translate\(/);
    expectCorners(t, 360, 540, q);
  });

  it('maps the ghost exactly onto a poster seen in perspective (a painting on the Gallery wall)', () => {
    const q: Quad = [
      { x: 874.7, y: 296 },
      { x: 1022.6, y: 260.9 },
      { x: 1022.6, y: 598.5 },
      { x: 874.7, y: 571 },
    ];
    const t = quadTransform(200, 300, q);
    expect(t).toMatch(/^matrix3d\(/);
    expectCorners(t, 200, 300, q);
    // In perspective the poster's middle is where the quad's diagonals cross (nearer the
    // far, smaller edge than the corners' average).
    const mid = place(t, 100, 150);
    const [a, b, c, d] = q;
    const den = (a.x - c.x) * (b.y - d.y) - (a.y - c.y) * (b.x - d.x);
    const u = ((a.x - b.x) * (b.y - d.y) - (a.y - b.y) * (b.x - d.x)) / den;
    expect(mid.x).toBeCloseTo(a.x + u * (c.x - a.x), 3);
    expect(mid.y).toBeCloseTo(a.y + u * (c.y - a.y), 3);
    expect(mid.x).toBeLessThan(quadCentre(q).x);
  });

  it('maps onto a parallelogram (no perspective) at its corners too', () => {
    const q: Quad = [
      { x: 0, y: 10 },
      { x: 100, y: 0 },
      { x: 100, y: 150 },
      { x: 0, y: 160 },
    ];
    expectCorners(quadTransform(100, 150, q), 100, 150, q);
  });
});

describe('quads', () => {
  it('puts the GPU lenses’ corners (top-left, top-right, bottom-left, bottom-right) in order round the edge, in client px', () => {
    const q = projectedQuad(
      [
        { x: 0, y: 0 },
        { x: 10, y: 0 },
        { x: 0, y: 15 },
        { x: 10, y: 15 },
      ],
      100,
      50,
    );
    expect(q).toEqual([
      { x: 100, y: 50 },
      { x: 110, y: 50 },
      { x: 110, y: 65 },
      { x: 100, y: 65 },
    ]);
  });

  it('has no quad for a face seen from behind, edge on, or partly behind the camera', () => {
    const mirrored = [
      { x: 10, y: 0 },
      { x: 0, y: 0 },
      { x: 10, y: 15 },
      { x: 0, y: 15 },
    ];
    expect(projectedQuad(mirrored)).toBeNull();
    expect(
      projectedQuad([
        { x: 0, y: 0 },
        { x: 0, y: 0 },
        { x: 0, y: 15 },
        { x: 0, y: 15 },
      ]),
    ).toBeNull();
    expect(projectedQuad([{ x: 0, y: 0 }, null, { x: 0, y: 15 }, { x: 10, y: 15 }])).toBeNull();
  });

  it('measures quads', () => {
    const r = rectQuad({ left: 10, top: 20, width: 30, height: 45 });
    expect(isFlat(r)).toBe(true);
    expect(quadArea(r)).toBeCloseTo(30 * 45);
    expect(quadBox(r)).toEqual({ x: 10, y: 20, w: 30, h: 45 });
    expect(quadCentre(r)).toEqual({ x: 25, y: 42.5 });
    expect(quadSize(r)).toEqual({ w: 30, h: 45 });
    const t: Quad = [
      { x: 0, y: 0 },
      { x: 40, y: 10 },
      { x: 40, y: 50 },
      { x: 0, y: 60 },
    ];
    expect(isFlat(t)).toBe(false);
    // Its near side is 60 tall; its top edge ~41 long.
    expect(quadSize(t).h).toBeCloseTo(60);
    expect(quadSize(t).w).toBeCloseTo(Math.hypot(40, 10));
    expect(lerpQuad(r, t, 0)).toEqual(r);
    expect(lerpQuad(r, t, 1)).toEqual(t);
    expect(lerpQuad(r, t, 0.5)[0]).toEqual({ x: 5, y: 10 });
  });

  it('knows when any of a quad is in the window', () => {
    expect(onScreen(rectQuad({ left: -50, top: 10, width: 60, height: 90 }), 1000, 800)).toBe(true);
    expect(onScreen(rectQuad({ left: -70, top: 10, width: 60, height: 90 }), 1000, 800)).toBe(false);
    expect(onScreen(rectQuad({ left: 10, top: 801, width: 60, height: 90 }), 1000, 800)).toBe(false);
  });
});

describe('srcFor', () => {
  afterEach(() => vi.unstubAllGlobals());
  const art = { url: '/api/v1/art/p-1', blurhash: null, source: 'provider' } as never;

  it('picks the srcset candidate the browser shows at that width on this screen', () => {
    vi.stubGlobal('devicePixelRatio', 1);
    expect(srcFor(art, 360)).toBe('/api/v1/art/p-1?w=480');
    vi.stubGlobal('devicePixelRatio', 1.25);
    expect(srcFor(art, 360)).toBe('/api/v1/art/p-1?w=480');
    vi.stubGlobal('devicePixelRatio', 2);
    expect(srcFor(art, 360)).toBe('/api/v1/art/p-1?w=960');
    vi.stubGlobal('devicePixelRatio', 3);
    expect(srcFor(art, 360)).toBe('/api/v1/art/p-1?w=960');
    expect(srcFor(null, 360)).toBeNull();
  });
});

describe('hiding a title while its poster flies', () => {
  const rect = (l: number, t: number, w: number, h: number) => ({ left: l, top: t, width: w, height: h, right: l + w, bottom: t + h, x: l, y: t, toJSON: () => ({}) }) as DOMRect;

  it('asks a lens that hides titles itself, and shows it again once', () => {
    const hide = vi.fn();
    const view = { hide, rectOf: () => null, update: () => undefined, move: () => false, destroy: () => undefined } as unknown as LensView;
    const undo = hideIn(view, 7);
    expect(hide).toHaveBeenLastCalledWith(7);
    undo();
    undo();
    expect(hide).toHaveBeenCalledTimes(2);
    expect(hide).toHaveBeenLastCalledWith(null);
  });

  it('keeps a title hidden until every flight hiding it has landed (two can overlap)', () => {
    const hide = vi.fn();
    const view = { hide, rectOf: () => null } as unknown as LensView;
    const a = hideIn(view, 7);
    const b = hideIn(view, 7);
    a();
    expect(hide).not.toHaveBeenCalledWith(null);
    b();
    expect(hide).toHaveBeenLastCalledWith(null);
  });

  it('shows one title while another is still flying, keeping that one hidden', () => {
    const hide = vi.fn();
    const view = { hide, rectOf: () => null } as unknown as LensView;
    const seven = hideIn(view, 7);
    const eight = hideIn(view, 8);
    expect(hide).toHaveBeenLastCalledWith(8);
    eight();
    expect(hide).toHaveBeenLastCalledWith(7);
    seven();
    expect(hide).toHaveBeenLastCalledWith(null);
  });

  it('shows a DOM lens’s element only when the last flight hiding it lands, never back to hidden', () => {
    const el = document.createElement('div');
    const view = { artOf: () => el, rectOf: () => null } as unknown as LensView;
    const a = hideIn(view, 7);
    const b = hideIn(view, 7);
    a();
    expect(el.style.visibility).toBe('hidden');
    b();
    expect(el.style.visibility).toBe('');
  });

  it('hides a DOM lens’s poster element and puts it back', () => {
    const el = document.createElement('div');
    const view = { artOf: () => el, rectOf: () => null, update: () => undefined, move: () => false, destroy: () => undefined } as unknown as LensView;
    const undo = hideIn(view, 7);
    expect(el.style.visibility).toBe('hidden');
    undo();
    expect(el.style.visibility).toBe('');
  });

  it('finds a title only where it is in the window, from the lens’s own spot or its rect', () => {
    const own = { quad: rectQuad({ left: 10, top: 10, width: 20, height: 30 }), radius: 4, src: 'a.jpg' };
    const withSpot = { spotOf: () => own, rectOf: () => null } as unknown as LensView;
    expect(spotIn(withSpot, 1)).toBe(own);
    const byRect = { rectOf: () => rect(100, 100, 60, 90) } as unknown as LensView;
    expect(spotIn(byRect, 1)?.quad).toEqual(rectQuad(rect(100, 100, 60, 90)));
    const away = { rectOf: () => rect(-500, 100, 60, 90) } as unknown as LensView;
    expect(spotIn(away, 1)).toBeNull();
    const broken = {
      rectOf: () => {
        throw new Error('gone');
      },
    } as unknown as LensView;
    expect(spotIn(broken, 1)).toBeNull();
    expect(spotIn(null, 1)).toBeNull();
  });
});
