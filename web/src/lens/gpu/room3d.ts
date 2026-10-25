/**
 * Rooms you walk through (the Gallery): the perspective quads of scene3d.ts, with
 * the same CSS placement maths and the same batch format, seen by a camera that
 * moves and drawn with a depth buffer.
 *
 * - **Walking costs no geometry.** Quads live in world space; the camera (CSS
 *   `translateZ(P) rotateY(yaw) translate3d(-x, -y, d)` under `perspective: P`) is
 *   a uniform, so a step is one 64-byte write. Everything is one instanced quad
 *   (four vertices) per element, in three draws.
 * - **Depth without z-fighting.** Reversed-Z into a float32 depth buffer with an
 *   infinite far plane: depth = near / distance, so precision is relative and
 *   two surfaces a fraction of a pixel apart stay ordered at any distance in
 *   view (`depthMargin` checks the offsets the Gallery uses). Nothing is hung on
 *   the plane of what holds it: decals (light pools, frames, labels) sit a set
 *   distance in front of their wall, and no two surfaces share a plane where they
 *   overlap, unless they are the same colour.
 * - **Clipped at the eye.** Walls and floor run behind the camera; the near plane
 *   clips them in clip space (scene3d.ts clamps `w`, fine only for things in front).
 * - **Three passes, one buffer:** floor reflections (painter's order, under the
 *   floor), opaque architecture (depth write), then decals back to front (depth
 *   test, no write), so antialiased edges and fades blend over what is behind them.
 */
import { BUFFER, TEXTURE, type Gpu } from './device';
import { QUAD3D_FLOATS, type Quad3DBatch } from './scene3d';

export const KINDR = { solid: 0, poster: 1, text: 2, ring: 3, shadow: 4, linear: 7, radial: 8, floor: 9 } as const;
export const FLAGR = { gray: 1, hairline: 2, gradient: 8, mirror: 16, noaa: 64, inset: 128, cut: 256 } as const;

/** Floats in the camera uniform. */
const CAM_FLOATS = 16;

export const ROOM3D_WGSL = /* wgsl */ `
struct Cam {
  view: vec2f,     // canvas, CSS px
  center: vec2f,   // perspective origin, CSS px
  eye: vec4f,      // added to world points before the turn: (-x, -y, d)
  persp: f32,
  yaw: f32,        // radians, CSS rotateY
  near: f32,       // view distance of the near plane, CSS px
  time: f32,
  dpr: f32,
  _a: f32,
  _b: f32,
  _c: f32,
};

struct Q {
  pos: vec4f,     // centre x, y, z (world)
  size: vec4f,    // width, height, corner radius, kind
  rot: vec4f,     // rotateX, rotateY, rotateZ (radians; X applied first, as CSS lists them right to left)
  origin: vec4f,  // transform origin, relative to the centre
  color: vec4f,   // straight-alpha colour, or a tint (gradients)
  uv: vec4f,      // texture rect; a second colour (poster placeholders); gradient geometry
  tex: vec4f,     // poster code or gradient offset, texture width or ring width / blur, ready-at (< 0: none), opacity
  fx: vec4f,      // flags, mirror line y, mirror strength, brightness (0 = as is)
};

@group(0) @binding(0) var<uniform> cam: Cam;
@group(0) @binding(1) var<storage, read> quads: array<Q>;
@group(0) @binding(2) var samp: sampler;
@group(0) @binding(3) var tex0: texture_2d_array<f32>;
@group(0) @binding(4) var tex1: texture_2d_array<f32>;
@group(0) @binding(5) var tex2: texture_2d_array<f32>;
@group(0) @binding(6) var tex3: texture_2d_array<f32>;
@group(0) @binding(7) var glyphs: texture_2d<f32>;
// Gradients: at each offset a header (stop count), then per stop (position) and (premultiplied colour).
@group(0) @binding(8) var<storage, read> grads: array<vec4f>;

const K_SOLID: u32 = 0u;
const K_POSTER: u32 = 1u;
const K_TEXT: u32 = 2u;
const K_RING: u32 = 3u;
const K_SHADOW: u32 = 4u;
const K_LINEAR: u32 = 7u;
const K_RADIAL: u32 = 8u;
const K_FLOOR: u32 = 9u;

const F_GRAY: u32 = 1u;
const F_HAIRLINE: u32 = 2u;
const F_GRADIENT: u32 = 8u;
const F_MIRROR: u32 = 16u;
const F_NOAA: u32 = 64u;
const F_INSET: u32 = 128u;
const F_CUT: u32 = 256u;

struct VOut {
  @builtin(position) pos: vec4f,
  @location(0) local: vec2f,
  @location(1) @interpolate(flat) half: vec2f,
  @location(2) @interpolate(flat) idx: u32,
};

fn rotX(p: vec3f, a: f32) -> vec3f {
  let c = cos(a);
  let s = sin(a);
  return vec3f(p.x, c * p.y - s * p.z, s * p.y + c * p.z);
}

fn rotY(p: vec3f, a: f32) -> vec3f {
  let c = cos(a);
  let s = sin(a);
  return vec3f(c * p.x + s * p.z, p.y, -s * p.x + c * p.z);
}

fn rotZ(p: vec3f, a: f32) -> vec3f {
  let c = cos(a);
  let s = sin(a);
  return vec3f(c * p.x - s * p.y, s * p.x + c * p.y, p.z);
}

/** A quad-local point in world space (CSS: translate, rotate about the origin). */
fn world(q: Q, local: vec2f, flags: u32) -> vec3f {
  var p = vec3f(local, 0.0) - q.origin.xyz;
  p = rotZ(rotY(rotX(p, q.rot.x), q.rot.y), q.rot.z);
  var v = p + q.origin.xyz + q.pos.xyz;
  if ((flags & F_MIRROR) != 0u) {
    v.y = 2.0 * q.fx.y - v.y;
  }
  return v;
}

/** World to the camera's space: the eye at z = persp, looking down -z. */
fn eyeSpace(v: vec3f) -> vec3f {
  var e = rotY(v + cam.eye.xyz, cam.yaw);
  e.z = e.z + cam.persp;
  return e;
}

@vertex
fn vs(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> VOut {
  let q = quads[ii];
  let kind = u32(q.size.w);
  let flags = u32(q.fx.x);
  let half = q.size.xy * 0.5;
  let corner = vec2f(f32(vi & 1u), f32(vi >> 1u)) * 2.0 - 1.0;
  // A margin so edges can antialias: two device pixels at this corner's distance
  // (a far quad needs a wider margin in world units than a near one).
  var pad = 0.0;
  if (kind != K_TEXT && kind != K_SHADOW && (flags & F_NOAA) == 0u) {
    let e0 = eyeSpace(world(q, corner * half, flags));
    pad = 2.0 * max(1.0, (cam.persp - e0.z) / cam.persp / max(cam.dpr, 0.5));
  }
  let local = corner * (half + vec2f(pad));
  let e = eyeSpace(world(q, local, flags));
  // CSS perspective, unclamped: w = distance / persp is negative behind the eye, and
  // the hardware clips at the near plane. Depth = near / distance (reversed, infinite far).
  let w = (cam.persp - e.z) / cam.persp;
  let sx = cam.center.x * w + e.x;
  let sy = cam.center.y * w + e.y;
  var out: VOut;
  out.pos = vec4f(sx / cam.view.x * 2.0 - w, w - sy / cam.view.y * 2.0, cam.near / cam.persp, w);
  out.local = local;
  out.half = half;
  out.idx = ii;
  return out;
}

fn roundRect(p: vec2f, b: vec2f, r: f32) -> f32 {
  let d = abs(p) - b + vec2f(r);
  return length(max(d, vec2f(0.0))) + min(max(d.x, d.y), 0.0) - r;
}

fn poster(cls: u32, uv: vec2f, layer: i32, gx: vec2f, gy: vec2f) -> vec4f {
  switch cls {
    case 0u: { return textureSampleGrad(tex0, samp, uv, layer, gx, gy); }
    case 1u: { return textureSampleGrad(tex1, samp, uv, layer, gx, gy); }
    case 2u: { return textureSampleGrad(tex2, samp, uv, layer, gx, gy); }
    default: { return textureSampleGrad(tex3, samp, uv, layer, gx, gy); }
  }
}

/** The premultiplied colour of gradient \`base\` at \`t\` (CSS interpolates premultiplied). */
fn grad(base: u32, t: f32) -> vec4f {
  let n = u32(grads[base].x);
  var pt = grads[base + 1u].x;
  var pc = grads[base + 2u];
  if (t <= pt) {
    return pc;
  }
  for (var i = 1u; i < n; i++) {
    let st = grads[base + 1u + 2u * i].x;
    let sc = grads[base + 2u + 2u * i];
    if (t <= st) {
      return mix(pc, sc, clamp((t - pt) / max(st - pt, 0.00001), 0.0, 1.0));
    }
    pt = st;
    pc = sc;
  }
  return pc;
}

/** CSS linear-gradient(angle): 0 = to top, clockwise; the line spans the box's corners. */
fn linearT(local: vec2f, half: vec2f, angle: f32) -> f32 {
  let dir = vec2f(sin(angle), -cos(angle));
  let len = abs(2.0 * half.x * dir.x) + abs(2.0 * half.y * dir.y);
  return dot(local, dir) / max(len, 0.0001) + 0.5;
}

/** Coverage of the stripes [per - lw, per) of every period along u, box-filtered over [u0, u1]. */
fn stripesIn(u0: f32, u1: f32, per: f32, lw: f32) -> f32 {
  let a = floor(u0 / per) * lw + clamp(u0 - floor(u0 / per) * per - (per - lw), 0.0, lw);
  let b = floor(u1 / per) * lw + clamp(u1 - floor(u1 / per) * per - (per - lw), 0.0, lw);
  return (b - a) / max(u1 - u0, 0.0001);
}

fn tinted(pm: vec4f, tint: vec4f) -> vec4f {
  return vec4f(pm.rgb * tint.rgb, pm.a) * tint.a;
}

@fragment
fn fs(in: VOut) -> @location(0) vec4f {
  // Screen-space rates first (derivatives need uniform control flow).
  let q = quads[in.idx];
  let radius = min(q.size.z, min(in.half.x, in.half.y));
  let d0 = roundRect(in.local, in.half, radius);
  let aa = max(fwidth(d0), 0.0001);
  let gx = dpdx(in.local) / (2.0 * in.half);
  let gy = dpdy(in.local) / (2.0 * in.half);
  let fu = max(fwidth(in.local.x), 0.0001);
  // An opening cut out of a face (a doorway): |x| < uv.y below y = uv.z, open at the bottom.
  // One quad with a hole has no inner seams; two that meet would each antialias the joint.
  let dOpen = max(abs(in.local.x) - q.uv.y, q.uv.z - in.local.y);
  let aOpen = max(fwidth(dOpen), 0.0001);
  let kind = u32(q.size.w);
  let flags = u32(q.fx.x);
  let f = clamp(in.local / (2.0 * in.half) + 0.5, vec2f(0.0), vec2f(1.0));
  var col = vec4f(0.0);
  if (kind == K_TEXT) {
    let cover = textureSampleLevel(glyphs, samp, mix(q.uv.xy, q.uv.zw, f), 0.0).r;
    col = vec4f(q.color.rgb, 1.0) * q.color.a * cover;
  } else if (kind == K_SHADOW) {
    let blur = max(q.tex.y, 0.5);
    var cover = 0.0;
    if ((flags & F_INSET) != 0u) {
      // An inner shadow: darkest at the rect's edge, gone \`blur\` inside it.
      cover = smoothstep(-blur, 0.0, d0) * clamp(0.5 - d0 / aa, 0.0, 1.0);
    } else {
      let d = roundRect(in.local, max(in.half - vec2f(blur), vec2f(0.5)), radius);
      cover = 1.0 - smoothstep(-blur * 0.5, blur * 0.5, d);
    }
    col = vec4f(q.color.rgb, 1.0) * q.color.a * cover;
  } else {
    var cover = 1.0;
    if ((flags & F_NOAA) != 0u) {
      cover = select(1.0, 0.0, d0 > 0.0);
    } else {
      cover = clamp(0.5 - d0 / aa, 0.0, 1.0);
    }
    if ((flags & F_CUT) != 0u) {
      cover = cover * clamp(0.5 + dOpen / aOpen, 0.0, 1.0);
    }
    if (kind == K_RING) {
      cover = cover - clamp(0.5 - (d0 + q.tex.y) / aa, 0.0, 1.0);
      col = vec4f(q.color.rgb, 1.0) * q.color.a * cover;
    } else if (kind == K_LINEAR) {
      col = tinted(grad(u32(q.tex.x), linearT(in.local, in.half, q.uv.x)), q.color) * cover;
    } else if (kind == K_RADIAL) {
      let t = length((f - q.uv.xy) / max(q.uv.zw, vec2f(0.0001)));
      col = tinted(grad(u32(q.tex.x), t), q.color) * cover;
    } else if (kind == K_FLOOR) {
      // The boards: a stripe every uv.x along the floor's width, box-filtered over the
      // pixel's footprint, so they never shimmer into moire at a grazing angle.
      let u = in.local.x + in.half.x;
      let lines = stripesIn(u - fu * 0.5, u + fu * 0.5, q.uv.x, q.uv.y);
      let base = vec4f(q.color.rgb, 1.0) * q.color.a;
      let board = grad(u32(q.tex.y), 0.0);
      let boards = board * lines + base * (1.0 - board.a * lines);
      let sheen = grad(u32(q.tex.x), f.x);
      col = (sheen + boards * (1.0 - sheen.a)) * cover;
    } else if (kind == K_POSTER) {
      var c = q.color;
      if ((flags & F_GRADIENT) != 0u) {
        let dir = vec2f(0.342, 0.940);
        let len = abs(in.half.x * dir.x) + abs(in.half.y * dir.y);
        let g = clamp(dot(in.local, dir) / (2.0 * len) + 0.5, 0.0, 1.0);
        c = mix(q.color, q.uv, g);
      }
      if (q.tex.z >= 0.0) {
        let code = u32(q.tex.x);
        let span = q.uv.zw - q.uv.xy;
        var s = poster(code >> 12u, q.uv.xy + span * f, i32(code & 4095u), gx * span, gy * span);
        if ((flags & F_GRAY) != 0u) {
          let g = dot(s.rgb, vec3f(0.2126, 0.7152, 0.0722)) * 0.6;
          s = vec4f(vec3f(g), s.a);
        }
        let fade = clamp((cam.time - q.tex.z) / 0.28, 0.0, 1.0);
        c = mix(c, vec4f(s.rgb, 1.0), fade);
      }
      col = vec4f(c.rgb, 1.0) * c.a * cover;
    } else {
      col = vec4f(q.color.rgb, 1.0) * q.color.a * cover;
    }
    if ((flags & F_HAIRLINE) != 0u) {
      // CSS \`inset 0 0 0 1px rgb(255 255 255 / 0.07)\`.
      let edge = smoothstep(-1.0 - aa, -1.0, d0);
      col = vec4f(mix(col.rgb, vec3f(col.a), 0.07 * edge), col.a);
    }
  }
  col = vec4f(col.rgb * (1.0 + q.fx.w), col.a);
  var a = q.tex.w;
  if ((flags & F_MIRROR) != 0u) {
    a = a * q.fx.z;
  }
  // Nothing here (a cut-out, the clear part of a gradient or a glyph): leave no depth
  // behind, or what is drawn later through it would fail the depth test.
  if (col.a * a <= 0.002) {
    discard;
  }
  return col * a;
}
`;

/** Where the camera stands and looks (CSS px and radians); see the module notes. */
export interface RoomCamera {
  /** Canvas size, CSS px. */
  width: number;
  height: number;
  /** Perspective origin, CSS px. */
  centerX: number;
  centerY: number;
  /** CSS `perspective`. */
  persp: number;
  /** CSS `rotateY`, radians. */
  yaw: number;
  /** CSS `translate3d(-x, -y, d)` before the turn. */
  eyeX: number;
  eyeY: number;
  eyeZ: number;
  /** The near plane: view distance, CSS px. */
  near: number;
  dpr: number;
  /** Seconds on the shared clock (poster fade-ins). */
  time: number;
}

type CamPose = Pick<RoomCamera, 'persp' | 'yaw' | 'eyeX' | 'eyeY' | 'eyeZ'>;

/** A world point in the camera's space (the eye at z = persp, looking down -z), as the shader does it. */
export function eyeSpace(x: number, y: number, z: number, cam: CamPose): [number, number, number] {
  const ex = x + cam.eyeX;
  const ey = y + cam.eyeY;
  const ez = z + cam.eyeZ;
  const c = Math.cos(cam.yaw);
  const s = Math.sin(cam.yaw);
  return [c * ex + s * ez, ey, -s * ex + c * ez + cam.persp];
}

/** A point's distance from the eye along the view axis. */
export function viewDistance(x: number, y: number, z: number, cam: CamPose): number {
  return cam.persp - eyeSpace(x, y, z, cam)[2];
}

/** Where a world point lands on the canvas (CSS px), or null in front of the near plane. */
export function projectRoom(x: number, y: number, z: number, cam: CamPose & Pick<RoomCamera, 'centerX' | 'centerY' | 'near'>): { x: number; y: number; scale: number; dist: number } | null {
  const [vx, vy, vz] = eyeSpace(x, y, z, cam);
  const dist = cam.persp - vz;
  if (dist < cam.near) return null;
  const w = dist / cam.persp;
  return { x: cam.centerX + vx / w, y: cam.centerY + vy / w, scale: 1 / w, dist };
}

/** A quad's four corners on the canvas (top-left, top-right, bottom-left, bottom-right), as the shader places them. */
export function roomCorners(
  q: { x: number; y: number; z: number; w: number; h: number; rx?: number; ry?: number; rz?: number },
  cam: CamPose & Pick<RoomCamera, 'centerX' | 'centerY' | 'near'>,
): ({ x: number; y: number } | null)[] {
  const out: ({ x: number; y: number } | null)[] = [];
  for (const [cx, cy] of [
    [-1, -1],
    [1, -1],
    [-1, 1],
    [1, 1],
  ]) {
    let px = (cx * q.w) / 2;
    let py = (cy * q.h) / 2;
    let pz = 0;
    const rx = q.rx ?? 0;
    const ry = q.ry ?? 0;
    const rz = q.rz ?? 0;
    [py, pz] = [Math.cos(rx) * py - Math.sin(rx) * pz, Math.sin(rx) * py + Math.cos(rx) * pz];
    [px, pz] = [Math.cos(ry) * px + Math.sin(ry) * pz, -Math.sin(ry) * px + Math.cos(ry) * pz];
    [px, py] = [Math.cos(rz) * px - Math.sin(rz) * py, Math.sin(rz) * px + Math.cos(rz) * py];
    out.push(projectRoom(px + q.x, py + q.y, pz + q.z, cam));
  }
  return out;
}

/**
 * How far apart in float32 depth two parallel surfaces `gap` px apart are, at
 * `dist` px from the eye, in units of the depth value's own precision (ulps).
 * Above a few, the depth test orders them without z-fighting.
 */
export function depthMargin(dist: number, gap: number, near: number): number {
  const far = Math.fround(near / dist);
  const close = Math.fround(near / (dist - gap));
  // One ulp of a float32 at `far`: 2^(exponent - 23).
  const ulp = 2 ** (Math.floor(Math.log2(far)) - 23);
  return (close - far) / ulp;
}

/**
 * The gradient table the shader reads: per gradient a header and its stops, each
 * stop's colour premultiplied (as CSS interpolates). Returns each gradient's offset.
 */
export function packGradients(list: readonly (readonly [number, readonly [number, number, number, number]])[][]): { data: Float32Array; offsets: number[] } {
  const offsets: number[] = [];
  let n = 0;
  for (const g of list) n += 1 + 2 * g.length;
  const data = new Float32Array(Math.max(4, n * 4));
  let at = 0;
  for (const g of list) {
    offsets.push(at);
    data[at * 4] = g.length;
    at++;
    for (const [t, c] of g) {
      data[at * 4] = t;
      at++;
      data.set([c[0] * c[3], c[1] * c[3], c[2] * c[3], c[3]], at * 4);
      at++;
    }
  }
  return { data, offsets };
}

export interface RoomTextures {
  posters: readonly GPUTextureView[];
  glyphs: GPUTextureView;
  key: string;
}

interface SharedRoom {
  layout: GPUBindGroupLayout;
  reflect: GPURenderPipeline;
  opaque: GPURenderPipeline;
  decal: GPURenderPipeline;
  sampler: GPUSampler;
}

const shared = new WeakMap<GPUDevice, SharedRoom>();
const preparing = new WeakMap<GPUDevice, Promise<boolean>>();

const DEPTH_FORMAT: GPUTextureFormat = 'depth32float';

/** Compile the room pipelines once per device. False if they don't compile here (use the DOM lens). */
export function prepareRoom3D(gpu: Gpu): Promise<boolean> {
  let p = preparing.get(gpu.device);
  if (!p) {
    p = (async () => {
      try {
        const device = gpu.device;
        const module = device.createShaderModule({ label: 'flux rooms', code: ROOM3D_WGSL });
        const VERTEX = 0x1;
        const FRAGMENT = 0x2;
        const layout = device.createBindGroupLayout({
          label: 'flux rooms',
          entries: [
            { binding: 0, visibility: VERTEX | FRAGMENT, buffer: { type: 'uniform' } },
            { binding: 1, visibility: VERTEX | FRAGMENT, buffer: { type: 'read-only-storage' } },
            { binding: 2, visibility: FRAGMENT, sampler: { type: 'filtering' } },
            { binding: 3, visibility: FRAGMENT, texture: { sampleType: 'float', viewDimension: '2d-array' } },
            { binding: 4, visibility: FRAGMENT, texture: { sampleType: 'float', viewDimension: '2d-array' } },
            { binding: 5, visibility: FRAGMENT, texture: { sampleType: 'float', viewDimension: '2d-array' } },
            { binding: 6, visibility: FRAGMENT, texture: { sampleType: 'float', viewDimension: '2d-array' } },
            { binding: 7, visibility: FRAGMENT, texture: { sampleType: 'float', viewDimension: '2d' } },
            { binding: 8, visibility: FRAGMENT, buffer: { type: 'read-only-storage' } },
          ],
        });
        const pipelineLayout = device.createPipelineLayout({ label: 'flux rooms', bindGroupLayouts: [layout] });
        const blend: GPUBlendState = { color: { srcFactor: 'one', dstFactor: 'one-minus-src-alpha', operation: 'add' }, alpha: { srcFactor: 'one', dstFactor: 'one-minus-src-alpha', operation: 'add' } };
        const make = (label: string, depthWriteEnabled: boolean, depthCompare: GPUCompareFunction) =>
          device.createRenderPipelineAsync({
            label: `flux rooms: ${label}`,
            layout: pipelineLayout,
            vertex: { module, entryPoint: 'vs' },
            fragment: { module, entryPoint: 'fs', targets: [{ format: gpu.format, blend }] },
            primitive: { topology: 'triangle-strip' },
            depthStencil: { format: DEPTH_FORMAT, depthWriteEnabled, depthCompare },
          });
        // Reversed-Z: nearer is greater.
        const [reflect, opaque, decal] = await Promise.all([make('reflections', false, 'always'), make('architecture', true, 'greater'), make('decals', false, 'greater')]);
        const sampler = device.createSampler({ magFilter: 'linear', minFilter: 'linear', mipmapFilter: 'linear', addressModeU: 'clamp-to-edge', addressModeV: 'clamp-to-edge', maxAnisotropy: 16 });
        shared.set(device, { layout, reflect, opaque, decal, sampler });
        return true;
      } catch (err) {
        console.error('flux: the room pipelines failed to compile; using the DOM lens', err);
        return false;
      }
    })();
    preparing.set(gpu.device, p);
  }
  return p;
}

/** How a batch is split into the three passes (consecutive runs, in this order). */
export interface RoomPasses {
  reflections: number;
  architecture: number;
  decals: number;
}

export class Room3DRenderer {
  readonly canvas: HTMLCanvasElement;
  private gpu: Gpu;
  private ctx: GPUCanvasContext;
  private uniforms: GPUBuffer;
  private storage: GPUBuffer | null = null;
  private capacity = 0;
  private gradients: GPUBuffer | null = null;
  private gradientBytes = 0;
  private depth: GPUTexture | null = null;
  private bindGroup: GPUBindGroup | null = null;
  private bindKey = '';
  private u = new Float32Array(CAM_FLOATS);

  constructor(gpu: Gpu, canvas: HTMLCanvasElement) {
    this.gpu = gpu;
    this.canvas = canvas;
    const ctx = canvas.getContext('webgpu') as GPUCanvasContext | null;
    if (!ctx) throw new Error('This canvas has no WebGPU context.');
    this.ctx = ctx;
    ctx.configure({ device: gpu.device, format: gpu.format, alphaMode: 'premultiplied' });
    this.uniforms = gpu.device.createBuffer({ label: 'flux room camera', size: CAM_FLOATS * 4, usage: BUFFER.UNIFORM | BUFFER.COPY_DST });
  }

  resize(width: number, height: number): void {
    const w = Math.max(1, Math.round(width));
    const h = Math.max(1, Math.round(height));
    if (this.canvas.width !== w) this.canvas.width = w;
    if (this.canvas.height !== h) this.canvas.height = h;
  }

  /** The gradient table (`packGradients`); kept until replaced. */
  setGradients(data: Float32Array): void {
    const device = this.gpu.device;
    if (!this.gradients || data.byteLength > this.gradientBytes) {
      this.gradients?.destroy();
      this.gradientBytes = Math.max(256, data.byteLength);
      this.gradients = device.createBuffer({ label: 'flux room gradients', size: this.gradientBytes, usage: BUFFER.STORAGE | BUFFER.COPY_DST });
      this.bindKey = '';
    }
    device.queue.writeBuffer(this.gradients, 0, data.buffer, data.byteOffset, data.byteLength);
  }

  private depthView(): GPUTextureView {
    const { width, height } = this.canvas;
    if (!this.depth || this.depth.width !== width || this.depth.height !== height) {
      this.depth?.destroy();
      this.depth = this.gpu.device.createTexture({ label: 'flux room depth', size: [width, height], format: DEPTH_FORMAT, usage: TEXTURE.RENDER_ATTACHMENT });
    }
    return this.depth.createView();
  }

  /** Draw `batch` from `cam`. `fresh`: the batch changed since the last draw (else only the camera is sent). */
  draw(batch: Quad3DBatch, passes: RoomPasses, cam: RoomCamera, tex: RoomTextures, fresh = true): void {
    const s = shared.get(this.gpu.device);
    if (!s) throw new Error('prepareRoom3D() first');
    if (!this.gradients) this.setGradients(new Float32Array(4));
    const device = this.gpu.device;
    const n = batch.n;
    const bytes = Math.max(1, n) * QUAD3D_FLOATS * 4;
    if (!this.storage || bytes > this.capacity) {
      this.storage?.destroy();
      this.capacity = Math.max(bytes, this.capacity * 2, 256 * QUAD3D_FLOATS * 4);
      this.storage = device.createBuffer({ label: 'flux room quads', size: this.capacity, usage: BUFFER.STORAGE | BUFFER.COPY_DST });
      this.bindKey = '';
      fresh = true;
    }
    if (n && fresh) device.queue.writeBuffer(this.storage, 0, batch.data.buffer, batch.data.byteOffset, n * QUAD3D_FLOATS * 4);
    const u = this.u;
    u[0] = cam.width;
    u[1] = cam.height;
    u[2] = cam.centerX;
    u[3] = cam.centerY;
    u[4] = cam.eyeX;
    u[5] = cam.eyeY;
    u[6] = cam.eyeZ;
    u[7] = 0;
    u[8] = cam.persp;
    u[9] = cam.yaw;
    u[10] = cam.near;
    u[11] = cam.time;
    u[12] = cam.dpr;
    device.queue.writeBuffer(this.uniforms, 0, u);
    if (!this.bindGroup || this.bindKey !== tex.key) {
      this.bindGroup = device.createBindGroup({
        label: 'flux rooms',
        layout: s.layout,
        entries: [
          { binding: 0, resource: { buffer: this.uniforms } },
          { binding: 1, resource: { buffer: this.storage } },
          { binding: 2, resource: s.sampler },
          { binding: 3, resource: tex.posters[0] },
          { binding: 4, resource: tex.posters[1] },
          { binding: 5, resource: tex.posters[2] },
          { binding: 6, resource: tex.posters[3] },
          { binding: 7, resource: tex.glyphs },
          { binding: 8, resource: { buffer: this.gradients as GPUBuffer } },
        ],
      });
      this.bindKey = tex.key;
    }
    const encoder = device.createCommandEncoder({ label: 'flux room frame' });
    const pass = encoder.beginRenderPass({
      colorAttachments: [{ view: this.ctx.getCurrentTexture().createView(), clearValue: { r: 0, g: 0, b: 0, a: 0 }, loadOp: 'clear', storeOp: 'store' }],
      depthStencilAttachment: { view: this.depthView(), depthClearValue: 0, depthLoadOp: 'clear', depthStoreOp: 'discard' },
    });
    pass.setBindGroup(0, this.bindGroup);
    let first = 0;
    for (const [pipeline, count] of [
      [s.reflect, passes.reflections],
      [s.opaque, passes.architecture],
      [s.decal, passes.decals],
    ] as const) {
      if (count > 0) {
        pass.setPipeline(pipeline);
        pass.draw(4, count, 0, first);
      }
      first += count;
    }
    pass.end();
    device.queue.submit([encoder.finish()]);
  }

  destroy(): void {
    try {
      this.ctx.unconfigure();
    } catch {
      // already gone with the device
    }
    this.storage?.destroy();
    this.gradients?.destroy();
    this.depth?.destroy();
    this.uniforms.destroy();
  }
}
