/** The room renderer's maths: the camera, projection, depth precision, gradients. */
import { describe, expect, it } from 'vitest';
import { depthMargin, eyeSpace, packGradients, projectRoom, roomCorners, viewDistance } from './room3d';

const cam = { persp: 800, yaw: 0, eyeX: 0, eyeY: 0, eyeZ: 0, centerX: 600, centerY: 400, near: 8 };

describe('the room camera', () => {
  it('sees straight ahead at the centre, smaller with distance, as CSS perspective does', () => {
    const p = projectRoom(0, 0, -800, cam);
    expect(p).not.toBeNull();
    expect(p?.x).toBeCloseTo(600);
    expect(p?.y).toBeCloseTo(400);
    expect(p?.dist).toBeCloseTo(800);
    expect(p?.scale).toBeCloseTo(1);
    // 100 px right at twice the distance: 50 px right on screen.
    const q = projectRoom(100, 0, -1600, cam);
    expect(q?.x).toBeCloseTo(650);
    expect(q?.scale).toBeCloseTo(0.5);
  });

  it('walks: the eye translates the world before the turn (CSS translate3d(-x, -y, d))', () => {
    // Standing at d = 1000 (eyeZ = d) looking down the corridor, a point 800 further is 800 away.
    const walked = { ...cam, eyeZ: 1000, eyeX: -50 };
    expect(viewDistance(50, 0, -1800, walked)).toBeCloseTo(800);
    expect(projectRoom(50, 0, -1800, walked)?.x).toBeCloseTo(600);
  });

  it('turns: CSS rotateY(yaw), so a positive yaw looks toward the right wall', () => {
    const turned = { ...cam, yaw: Math.PI / 2 };
    // Straight ahead is now level with the eye: in the eye plane, not drawable.
    expect(projectRoom(0, 0, -800, turned)).toBeNull();
    // A point to the right is now ahead.
    const r = projectRoom(800, 0, 0, turned);
    expect(r?.dist).toBeCloseTo(800);
    expect(r?.x).toBeCloseTo(600);
    const [ex, , ez] = eyeSpace(800, 0, 0, turned);
    expect(ex).toBeCloseTo(0);
    expect(ez).toBeCloseTo(0);
  });

  it('places quad corners as the shader does, and clips at the near plane', () => {
    const c = roomCorners({ x: 0, y: 0, z: -800, w: 200, h: 100 }, cam);
    expect(c.map((p) => [Math.round(p?.x ?? NaN), Math.round(p?.y ?? NaN)])).toEqual([
      [500, 350],
      [700, 350],
      [500, 450],
      [700, 450],
    ]);
    // A wall running past the eye: its near corners are behind the near plane.
    const wall = roomCorners({ x: -500, y: 0, z: 0, w: 2000, h: 700, ry: Math.PI / 2 }, cam);
    expect(wall.filter((p) => p === null)).toHaveLength(2);
  });
});

describe('depth precision', () => {
  it('reversed-Z float32 keeps surfaces a fraction of a pixel apart ordered, near and far', () => {
    for (const dist of [10, 500, 6000, 7600, 20000]) {
      expect(depthMargin(dist, 0.75, 8)).toBeGreaterThan(64);
      expect(depthMargin(dist, 0.05, 8)).toBeGreaterThan(4);
    }
  });

  it('would not with a conventional 24-bit buffer far away (why reversed-Z)', () => {
    // Conventional depth: (1/near − 1/dist) / (1/near − 1/far) in 2^24 steps. At the end of
    // the corridor two surfaces 0.75 px apart are under two steps apart: interpolation error
    // alone flips them (z-fighting). Reversed-Z float32 puts hundreds of steps between them.
    const near = 8;
    const far = 20000;
    const steps = ((1 / (7600 - 0.75) - 1 / 7600) / (1 / near - 1 / far)) * 2 ** 24;
    expect(steps).toBeLessThan(2);
    expect(depthMargin(7600, 0.75, near)).toBeGreaterThan(400);
  });
});

describe('gradients', () => {
  it('pack a header and premultiplied stops per gradient', () => {
    const { data, offsets } = packGradients([
      [
        [0, [1, 0, 0, 0.5]],
        [1, [0, 0, 1, 1]],
      ],
      [[0, [1, 1, 1, 0.2]]],
    ]);
    expect(offsets).toEqual([0, 5]);
    expect(Array.from(data.slice(0, 4))).toEqual([2, 0, 0, 0]);
    expect(data[4]).toBe(0);
    expect(Array.from(data.slice(8, 12))).toEqual([0.5, 0, 0, 0.5]);
    expect(data[12]).toBe(1);
    expect(Array.from(data.slice(16, 20))).toEqual([0, 0, 1, 1]);
    expect(data[20]).toBe(1);
    expect(Array.from(data.slice(28, 32)).map((v) => +v.toFixed(3))).toEqual([0.2, 0.2, 0.2, 0.2]);
  });
});
