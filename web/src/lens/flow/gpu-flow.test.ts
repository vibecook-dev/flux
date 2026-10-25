import { describe, expect, it } from 'vitest';
import { measure, pose, type Metrics } from './core';
import { coverBox, coverQuad, coverUnder, flowCamera, glowUV, groupAlphas, reflectedOffset, reflectionAt, reflectionPose, type Placed } from './gpu-flow';

const m: Metrics = measure(1440, 900, { top: 68, bottom: 84 }, false);

function at(d: number): Placed {
  const p = pose(d, m);
  return { x: p.x, z: p.z, rx: 0, ry: p.ry, ad: Math.abs(d) };
}

type V3 = [number, number, number];
const rotX = ([x, y, z]: V3, a: number): V3 => [x, Math.cos(a) * y - Math.sin(a) * z, Math.sin(a) * y + Math.cos(a) * z];
const rotY = ([x, y, z]: V3, a: number): V3 => [Math.cos(a) * x + Math.sin(a) * z, y, -Math.sin(a) * x + Math.cos(a) * z];

describe('covers on the GPU stage', () => {
  it('projects the poster in front where CSS puts it (perspective on the stage)', () => {
    const box = coverBox(at(0), m);
    const s = m.persp / (m.persp - m.lift);
    const cam = flowCamera(m);
    expect(cam).toEqual({ persp: m.persp, originX: m.W / 2, originY: m.floor - m.ch / 2 });
    expect(box?.w).toBeCloseTo(m.cw * s, 6);
    expect(box?.h).toBeCloseTo(m.ch * s, 6);
    expect(box?.x).toBeCloseTo(m.W / 2 - (m.cw * s) / 2, 6);
    expect(box?.y).toBeCloseTo(m.floor - m.ch / 2 - (m.ch * s) / 2, 6);
  });

  it('finds the front-most cover under the pointer, as they are drawn', () => {
    const covers = Array.from({ length: 13 }, (_, i) => ({ id: i, at: at(i - 6) }));
    const y = m.floor - m.ch / 2;
    expect(coverUnder(covers, m.W / 2, y, m)).toBe(6);
    const b7 = coverBox(covers[7].at, m);
    const b8 = coverBox(covers[8].at, m);
    if (!b7 || !b8) throw new Error('covers behind the camera');
    // Where the first two covers of the right stack overlap, the nearer one is on top.
    expect(b8.x).toBeLessThan(b7.x + b7.w);
    expect(coverUnder(covers, (b8.x + b7.x + b7.w) / 2, y, m)).toBe(7);
    expect(coverUnder(covers, b7.x + b7.w + 1, y, m)).toBe(8);
    expect(coverUnder(covers, m.W / 2, 4, m)).toBeNull();
  });

  it('turns the stacks to face the centre', () => {
    const q = coverQuad(at(1), m);
    expect(q.ry).toBeCloseTo((-70 * Math.PI) / 180);
    expect(q.y).toBe(m.floor - m.ch / 2);
  });
});

describe('reflections', () => {
  it('mirror an upright cover about the floor, just below its bottom edge', () => {
    const q = { x: 720, y: 300, z: -20, h: 480, rx: 0, ry: 0.8 };
    const r = reflectionPose(q, 2);
    expect(r).toMatchObject({ x: 720, y: 300, z: -20, rx: -0, ry: 0.8 });
    expect(r.line).toBe(300 + 241);
  });

  it('land where CSS reflects a tilted cover: in its own plane (-webkit-box-reflect)', () => {
    const q = { x: 700, y: 310, z: 14, h: 480, rx: 0.07, ry: -0.11 };
    const gap = 2;
    const r = reflectionPose(q, gap);
    for (const [lx, ly] of [
      [0, 0],
      [-150, 240],
      [150, -240],
      [37, 101],
    ]) {
      // CSS: the point (lx, ly) is reflected to (lx, h + gap − ly) in the cover's plane.
      const css = rotY(rotX([lx, q.h + gap - ly, 0], q.rx), q.ry);
      // The shader: the quad's own point, transformed, then mirrored about the line.
      const v = rotY(rotX([lx, ly, 0], r.rx), r.ry);
      const shader: V3 = [r.x + v[0], 2 * r.line - (r.y + v[1]), r.z + v[2]];
      expect(shader[0]).toBeCloseTo(q.x + css[0], 9);
      expect(shader[1]).toBeCloseTo(q.y + css[1], 9);
      expect(shader[2]).toBeCloseTo(q.z + css[2], 9);
    }
  });

  it('put marks where CSS reflects them, upside down in the cover’s plane', () => {
    const c: V3 = [700, 310, 14];
    const rx = 0.06;
    const ry = -1.1;
    const h = 480;
    const gap = 2;
    const [ox, oy] = [-80, 205];
    const y = reflectedOffset(oy, h, gap);
    for (const [lx, ly] of [
      [0, 0],
      [-30, 4],
      [12, -6],
    ]) {
      // The quad's point (lx, ly) shows the mark's texel (lx, −ly) (its texture is flipped);
      // CSS reflects that texel to (ox + lx, h + gap − (oy − ly)) in the cover's plane.
      const css = rotY(rotX([ox + lx, h + gap - (oy - ly), 0], rx), ry);
      // The shader: centre c + (ox, y), transform origin (−ox, −y), the cover's rotation.
      const v = rotY(rotX([lx + ox, ly + y, 0], rx), ry);
      expect(c[0] + v[0]).toBeCloseTo(c[0] + css[0], 9);
      expect(c[1] + v[1]).toBeCloseTo(c[1] + css[1], 9);
      expect(c[2] + v[2]).toBeCloseTo(c[2] + css[2], 9);
    }
  });

  it('fade from the edge on the floor, gone two thirds of the way up', () => {
    expect(reflectionAt(0.5)).toBe(0);
    expect(reflectionAt(0.64)).toBe(0);
    expect(reflectionAt(0.82)).toBeCloseTo(0.5);
    expect(reflectionAt(1)).toBe(1);
  });
});

describe('group opacity', () => {
  it('composites poster then haze exactly as CSS fades the whole cover', () => {
    const cases = [
      [1, 0, 0.2, 0.9, 0.5],
      [1, 0.546, 0.8, 0.04, 0.3],
      [0.4, 0.546, 0.7, 0.04, 0.1],
      [0.13, 0.3, 0.2, 0.9, 0.6],
    ];
    for (const [op, haze, P, B, D] of cases) {
      const a = groupAlphas(op, haze);
      const drawn = a.haze * B + (1 - a.haze) * (a.under * P + (1 - a.under) * D);
      const css = op * ((1 - haze) * P + haze * B) + (1 - op) * D;
      expect(drawn).toBeCloseTo(css, 12);
    }
    expect(groupAlphas(1, 0.4)).toEqual({ under: 1, haze: 0.4 });
    expect(groupAlphas(1, 1).under).toBe(0);
  });
});

describe('the pointer’s light', () => {
  it('centres the radial sprite on the pointer, its radius a share of the cover', () => {
    const [u0, v0, u1, v1] = glowUV(0.3, 0.8, 0.7, 0.55, 0.25, 0.5, 0.25, 0.5);
    const u = (f: number) => u0 + (u1 - u0) * f;
    const v = (f: number) => v0 + (v1 - v0) * f;
    expect(u(0.3)).toBeCloseTo(0.25);
    expect(v(0.8)).toBeCloseTo(0.5);
    // 70% of the cover's width to the right of the pointer is the sprite's radius.
    expect(u(0.3 + 0.7)).toBeCloseTo(0.5);
    expect(v(0.8 - 0.55)).toBeCloseTo(0);
  });
});
