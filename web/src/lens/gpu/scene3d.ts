/**
 * Perspective quads for the 3D lenses (Flow's covers, Shelf's tapes): the same
 * shading as the flat quads (signed-distance rounded rectangles, poster textures,
 * glyphs), placed with CSS's own 3D maths, so a lens ported from CSS 3D lands
 * where it did:
 *
 * - each quad is `translate3d(pos + origin) rotateZ(rz) rotateY(ry) rotateX(rx)
 *   translate3d(-origin)` applied to its corners (CSS order: X first);
 * - the camera is CSS `perspective: persp` with `perspective-origin: origin`
 *   (screen = origin + (p − origin) · persp / (persp − z)), x right, y down,
 *   z towards the viewer;
 * - texture coordinates interpolate perspective-correctly (the GPU divides by w).
 *
 * Draw order is the batch order (painter's algorithm, back to front): there is no
 * depth buffer, so antialiased edges and fades blend correctly.
 */
import { BUFFER, type Gpu } from './device';

/** Floats per quad (eight vec4s; see `Q` below). */
export const QUAD3D_FLOATS = 32;

export const KIND3D = { solid: 0, poster: 1, text: 2, ring: 3, shadow: 4, sprite: 6 } as const;
export const FLAG3D = { gray: 1, hairline: 2, gradient: 8, mirror: 16 } as const;

export const SCENE3D_WGSL = /* wgsl */ `
struct Cam {
  view: vec2f,
  origin: vec2f,
  persp: f32,
  dpr: f32,
  time: f32,
  _p: f32,
};

struct Q {
  pos: vec4f,     // centre x, y, z
  size: vec4f,    // width, height, corner radius, kind
  rot: vec4f,     // rotateX, rotateY, rotateZ, radians (X applied first, as CSS lists them right to left)
  origin: vec4f,  // transform origin, relative to the centre
  color: vec4f,   // straight-alpha colour (posters: placeholder; text and sprites: tint)
  uv: vec4f,      // texture rect, or a second colour (gradients)
  tex: vec4f,     // texture (posters: class * 4096 + layer; sprites: layer), texture width, ready-at seconds (< 0: none), opacity
  fx: vec4f,      // flags, mirror line y, mirror strength, sheen
};

@group(0) @binding(0) var<uniform> cam: Cam;
@group(0) @binding(1) var<storage, read> quads: array<Q>;
@group(0) @binding(2) var samp: sampler;
@group(0) @binding(3) var tex0: texture_2d_array<f32>;
@group(0) @binding(4) var tex1: texture_2d_array<f32>;
@group(0) @binding(5) var tex2: texture_2d_array<f32>;
@group(0) @binding(6) var tex3: texture_2d_array<f32>;
@group(0) @binding(7) var glyphs: texture_2d<f32>;
@group(0) @binding(8) var sprites: texture_2d_array<f32>;

const K_SOLID: u32 = 0u;
const K_POSTER: u32 = 1u;
const K_TEXT: u32 = 2u;
const K_RING: u32 = 3u;
const K_SHADOW: u32 = 4u;
const K_SPRITE: u32 = 6u;

const F_GRAY: u32 = 1u;
const F_HAIRLINE: u32 = 2u;
const F_GRADIENT: u32 = 8u;
const F_MIRROR: u32 = 16u;

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

@vertex
fn vs(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> VOut {
  let q = quads[ii];
  let kind = u32(q.size.w);
  let flags = u32(q.fx.x);
  let half = q.size.xy * 0.5;
  // A little margin so the edges can antialias (not for text or soft shadows, which fill their rect).
  var pad = 1.5;
  if (kind == K_TEXT || kind == K_SHADOW) {
    pad = 0.0;
  }
  let corner = vec2f(f32(vi & 1u), f32(vi >> 1u)) * 2.0 - 1.0;
  let local = corner * (half + vec2f(pad));
  var p = vec3f(local, 0.0) - q.origin.xyz;
  p = rotZ(rotY(rotX(p, q.rot.x), q.rot.y), q.rot.z);
  var v = p + q.origin.xyz + q.pos.xyz;
  if ((flags & F_MIRROR) != 0u) {
    v.y = 2.0 * q.fx.y - v.y;
  }
  var w = 1.0;
  if (cam.persp > 0.0) {
    w = max((cam.persp - v.z) / cam.persp, 0.0001);
  }
  let s = cam.origin + (v.xy - cam.origin) / w;
  var out: VOut;
  out.pos = vec4f((s.x / cam.view.x * 2.0 - 1.0) * w, (1.0 - s.y / cam.view.y * 2.0) * w, 0.5 * w, w);
  out.local = local;
  out.half = half;
  out.idx = ii;
  return out;
}

fn roundRect(p: vec2f, b: vec2f, r: f32) -> f32 {
  let d = abs(p) - b + vec2f(r);
  return length(max(d, vec2f(0.0))) + min(max(d.x, d.y), 0.0) - r;
}

// Explicit gradients: anisotropic filtering keeps a cover seen at a steep angle sharp
// along its long axis (an isotropic mip level would blur it).
fn poster(cls: u32, uv: vec2f, layer: i32, gx: vec2f, gy: vec2f) -> vec4f {
  switch cls {
    case 0u: { return textureSampleGrad(tex0, samp, uv, layer, gx, gy); }
    case 1u: { return textureSampleGrad(tex1, samp, uv, layer, gx, gy); }
    case 2u: { return textureSampleGrad(tex2, samp, uv, layer, gx, gy); }
    default: { return textureSampleGrad(tex3, samp, uv, layer, gx, gy); }
  }
}

@fragment
fn fs(in: VOut) -> @location(0) vec4f {
  // Screen-space rates first (derivatives need uniform control flow). Antialias with
  // the distance field's own rate, so a face seen nearly edge-on (a tape's side, a
  // cover turned away) keeps crisp edges; a width taken from the quad's coordinates
  // would smear them across the steep axis. The face's rate of change per device
  // pixel (gx, gy) drives anisotropic texture filtering.
  let q = quads[in.idx];
  let d0 = roundRect(in.local, in.half, min(q.size.z, min(in.half.x, in.half.y)));
  let aa = max(fwidth(d0), 0.0001);
  let gx = dpdx(in.local) / (2.0 * in.half);
  let gy = dpdy(in.local) / (2.0 * in.half);
  let kind = u32(q.size.w);
  let flags = u32(q.fx.x);
  let radius = min(q.size.z, min(in.half.x, in.half.y));
  let f = clamp(in.local / (2.0 * in.half) + 0.5, vec2f(0.0), vec2f(1.0));
  var c = q.color;
  var cover = 1.0;
  if (kind == K_TEXT) {
    cover = textureSampleLevel(glyphs, samp, mix(q.uv.xy, q.uv.zw, f), 0.0).r;
  } else if (kind == K_SHADOW) {
    let blur = max(q.tex.y, 0.5);
    let d = roundRect(in.local, max(in.half - vec2f(blur), vec2f(0.5)), radius);
    cover = 1.0 - smoothstep(-blur * 0.5, blur * 0.5, d);
  } else {
    let d = d0;
    cover = clamp(0.5 - d / aa, 0.0, 1.0);
    if (kind == K_RING) {
      cover = cover - clamp(0.5 - (d + q.tex.y) / aa, 0.0, 1.0);
    } else if (kind == K_SPRITE) {
      let s = textureSampleLevel(sprites, samp, mix(q.uv.xy, q.uv.zw, f), i32(q.tex.x), 0.0);
      // Sprites are stored premultiplied; tint multiplies.
      c = vec4f(s.rgb / max(s.a, 0.0001) * q.color.rgb, s.a * q.color.a);
    } else if (kind == K_POSTER) {
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
      if ((flags & F_HAIRLINE) != 0u) {
        let edge = smoothstep(-1.0 - aa, -1.0, d);
        c = vec4f(mix(c.rgb, vec3f(1.0), 0.07 * edge), c.a);
      }
    }
    // Sheen: a soft highlight across the face (Cover Flow's light on the glass).
    if (q.fx.w != 0.0) {
      let band = 1.0 - smoothstep(0.0, 0.55, abs(f.x - 0.25 - f.y * 0.3));
      c = vec4f(mix(c.rgb, vec3f(1.0), clamp(q.fx.w * band, 0.0, 1.0)), c.a);
    }
  }
  var a = c.a * cover * q.tex.w;
  if ((flags & F_MIRROR) != 0u) {
    // A reflection: strongest at the edge that touches the floor, gone a third of the way down.
    a = a * q.fx.z * smoothstep(0.64, 1.0, f.y);
  }
  return vec4f(c.rgb * a, a);
}
`;

export class Quad3DBatch {
  data: Float32Array;
  n = 0;

  constructor(capacity = 256) {
    this.data = new Float32Array(capacity * QUAD3D_FLOATS);
  }

  reset(): void {
    this.n = 0;
  }

  /** A new quad centred at (x, y, z), w × h, facing the viewer; returns its float offset. */
  add(x: number, y: number, z: number, w: number, h: number, kind: number, radius = 0): number {
    if ((this.n + 1) * QUAD3D_FLOATS > this.data.length) {
      const grown = new Float32Array(this.data.length * 2);
      grown.set(this.data);
      this.data = grown;
    }
    const o = this.n * QUAD3D_FLOATS;
    const d = this.data;
    d.fill(0, o, o + QUAD3D_FLOATS);
    d[o] = x;
    d[o + 1] = y;
    d[o + 2] = z;
    d[o + 4] = w;
    d[o + 5] = h;
    d[o + 6] = radius;
    d[o + 7] = kind;
    d[o + 27] = 1;
    d[o + 26] = -1;
    this.n++;
    return o;
  }

  /** Radians, applied X then Y then Z (CSS `rotateZ() rotateY() rotateX()`), about the origin. */
  rotate(o: number, rx: number, ry: number, rz = 0): void {
    const d = this.data;
    d[o + 8] = rx;
    d[o + 9] = ry;
    d[o + 10] = rz;
  }

  /** The transform origin, relative to the quad's centre (CSS `transform-origin`). */
  origin(o: number, x: number, y: number, z = 0): void {
    const d = this.data;
    d[o + 12] = x;
    d[o + 13] = y;
    d[o + 14] = z;
  }

  color(o: number, c: readonly [number, number, number, number], alpha = 1): void {
    const d = this.data;
    d[o + 16] = c[0];
    d[o + 17] = c[1];
    d[o + 18] = c[2];
    d[o + 19] = c[3] * alpha;
  }

  uv(o: number, a: number, b: number, c: number, e: number): void {
    const d = this.data;
    d[o + 20] = a;
    d[o + 21] = b;
    d[o + 22] = c;
    d[o + 23] = e;
  }

  /** A poster (code from the poster cache) or a sprite layer, its texture width, and when it appeared. */
  texture(o: number, code: number, texWidth: number, readyAt: number): void {
    const d = this.data;
    d[o + 24] = code;
    d[o + 25] = texWidth;
    d[o + 26] = readyAt;
  }

  /** Ring width or shadow blur. */
  extra(o: number, v: number): void {
    this.data[o + 25] = v;
  }

  opacity(o: number, a: number): void {
    this.data[o + 27] = a;
  }

  flags(o: number, f: number): void {
    this.data[o + 28] = f;
  }

  /** Draw this quad mirrored about the horizontal line `y` (a floor), fading with `strength`. */
  mirror(o: number, y: number, strength: number): void {
    const d = this.data;
    d[o + 28] = d[o + 28] | FLAG3D.mirror;
    d[o + 29] = y;
    d[o + 30] = strength;
  }

  sheen(o: number, amount: number): void {
    this.data[o + 31] = amount;
  }
}

export interface Camera {
  /** Canvas size, CSS pixels. */
  width: number;
  height: number;
  /** CSS `perspective` (0: none) and `perspective-origin`, CSS pixels. */
  persp: number;
  originX: number;
  originY: number;
  dpr: number;
  /** Seconds on the shared clock (quads.ts `clock`): poster fade-ins are timed against it. */
  time: number;
}

export interface Textures3D {
  posters: readonly GPUTextureView[];
  glyphs: GPUTextureView;
  sprites: GPUTextureView;
  key: string;
}

/** Where a point lands on screen under `cam` (CSS pixels), or null behind the camera. Matches the shader. */
export function project(x: number, y: number, z: number, cam: Pick<Camera, 'persp' | 'originX' | 'originY'>): { x: number; y: number; scale: number } | null {
  if (cam.persp <= 0) return { x, y, scale: 1 };
  const w = (cam.persp - z) / cam.persp;
  if (w <= 0.0001) return null;
  return { x: cam.originX + (x - cam.originX) / w, y: cam.originY + (y - cam.originY) / w, scale: 1 / w };
}

/** The four corners of a quad as the shader places them (top-left, top-right, bottom-left, bottom-right), for hit tests. */
export function corners(
  q: { x: number; y: number; z: number; w: number; h: number; rx?: number; ry?: number; rz?: number; ox?: number; oy?: number; oz?: number },
  cam: Pick<Camera, 'persp' | 'originX' | 'originY'>,
): ({ x: number; y: number } | null)[] {
  const out: ({ x: number; y: number } | null)[] = [];
  for (const [cx, cy] of [
    [-1, -1],
    [1, -1],
    [-1, 1],
    [1, 1],
  ]) {
    let px = (cx * q.w) / 2 - (q.ox ?? 0);
    let py = (cy * q.h) / 2 - (q.oy ?? 0);
    let pz = -(q.oz ?? 0);
    const rx = q.rx ?? 0;
    const ry = q.ry ?? 0;
    const rz = q.rz ?? 0;
    [py, pz] = [Math.cos(rx) * py - Math.sin(rx) * pz, Math.sin(rx) * py + Math.cos(rx) * pz];
    [px, pz] = [Math.cos(ry) * px + Math.sin(ry) * pz, -Math.sin(ry) * px + Math.cos(ry) * pz];
    [px, py] = [Math.cos(rz) * px - Math.sin(rz) * py, Math.sin(rz) * px + Math.cos(rz) * py];
    out.push(project(px + (q.ox ?? 0) + q.x, py + (q.oy ?? 0) + q.y, pz + (q.oz ?? 0) + q.z, cam));
  }
  return out;
}

/** Whether a screen point is inside a projected quad (its corners from `corners`). */
export function inside(px: number, py: number, c: ({ x: number; y: number } | null)[]): boolean {
  if (c.some((p) => !p)) return false;
  const [a, b, d, e] = c as { x: number; y: number }[];
  // Corners in order around the edge: top-left, top-right, bottom-right, bottom-left.
  const ring = [a, b, e, d];
  let sign = 0;
  for (let i = 0; i < 4; i++) {
    const p = ring[i];
    const q = ring[(i + 1) % 4];
    const cross = (q.x - p.x) * (py - p.y) - (q.y - p.y) * (px - p.x);
    if (cross === 0) continue;
    const s = Math.sign(cross);
    if (sign === 0) sign = s;
    else if (s !== sign) return false;
  }
  return true;
}

interface Shared3D {
  pipeline: GPURenderPipeline;
  sampler: GPUSampler;
}

const shared = new WeakMap<GPUDevice, Shared3D>();
const preparing = new WeakMap<GPUDevice, Promise<boolean>>();

/** Compile the 3D pipeline once per device. False if it doesn't compile here (use the DOM lens). */
export function prepareScene3D(gpu: Gpu): Promise<boolean> {
  let p = preparing.get(gpu.device);
  if (!p) {
    p = (async () => {
      try {
        const device = gpu.device;
        const module = device.createShaderModule({ label: 'flux 3d quads', code: SCENE3D_WGSL });
        const pipeline = await device.createRenderPipelineAsync({
          label: 'flux 3d quads',
          layout: 'auto',
          vertex: { module, entryPoint: 'vs' },
          fragment: {
            module,
            entryPoint: 'fs',
            targets: [{ format: gpu.format, blend: { color: { srcFactor: 'one', dstFactor: 'one-minus-src-alpha', operation: 'add' }, alpha: { srcFactor: 'one', dstFactor: 'one-minus-src-alpha', operation: 'add' } } }],
          },
          primitive: { topology: 'triangle-strip' },
        });
        const sampler = device.createSampler({ magFilter: 'linear', minFilter: 'linear', mipmapFilter: 'linear', addressModeU: 'clamp-to-edge', addressModeV: 'clamp-to-edge', maxAnisotropy: 16 });
        shared.set(device, { pipeline, sampler });
        return true;
      } catch (err) {
        console.error('flux: the 3D lens pipeline failed to compile; using the DOM lens', err);
        return false;
      }
    })();
    preparing.set(gpu.device, p);
  }
  return p;
}

export class Scene3DRenderer {
  readonly canvas: HTMLCanvasElement;
  private gpu: Gpu;
  private ctx: GPUCanvasContext;
  private uniforms: GPUBuffer;
  private storage: GPUBuffer | null = null;
  private capacity = 0;
  private bindGroup: GPUBindGroup | null = null;
  private bindKey = '';
  private u = new Float32Array(8);

  constructor(gpu: Gpu, canvas: HTMLCanvasElement) {
    this.gpu = gpu;
    this.canvas = canvas;
    const ctx = canvas.getContext('webgpu') as GPUCanvasContext | null;
    if (!ctx) throw new Error('This canvas has no WebGPU context.');
    this.ctx = ctx;
    ctx.configure({ device: gpu.device, format: gpu.format, alphaMode: 'premultiplied' });
    this.uniforms = gpu.device.createBuffer({ label: 'flux camera', size: 32, usage: BUFFER.UNIFORM | BUFFER.COPY_DST });
  }

  resize(width: number, height: number): void {
    const w = Math.max(1, Math.round(width));
    const h = Math.max(1, Math.round(height));
    if (this.canvas.width !== w) this.canvas.width = w;
    if (this.canvas.height !== h) this.canvas.height = h;
  }

  draw(batch: Quad3DBatch, cam: Camera, tex: Textures3D): void {
    const s = shared.get(this.gpu.device);
    if (!s) throw new Error('prepareScene3D() first');
    const device = this.gpu.device;
    const n = batch.n;
    const bytes = Math.max(1, n) * QUAD3D_FLOATS * 4;
    if (!this.storage || bytes > this.capacity) {
      this.storage?.destroy();
      this.capacity = Math.max(bytes, this.capacity * 2, 64 * QUAD3D_FLOATS * 4);
      this.storage = device.createBuffer({ label: 'flux 3d quads', size: this.capacity, usage: BUFFER.STORAGE | BUFFER.COPY_DST });
      this.bindKey = '';
    }
    if (n) device.queue.writeBuffer(this.storage, 0, batch.data.buffer, batch.data.byteOffset, n * QUAD3D_FLOATS * 4);
    const u = this.u;
    u[0] = cam.width;
    u[1] = cam.height;
    u[2] = cam.originX;
    u[3] = cam.originY;
    u[4] = cam.persp;
    u[5] = cam.dpr;
    u[6] = cam.time;
    device.queue.writeBuffer(this.uniforms, 0, u);
    if (!this.bindGroup || this.bindKey !== tex.key) {
      this.bindGroup = device.createBindGroup({
        label: 'flux 3d quads',
        layout: s.pipeline.getBindGroupLayout(0),
        entries: [
          { binding: 0, resource: { buffer: this.uniforms } },
          { binding: 1, resource: { buffer: this.storage } },
          { binding: 2, resource: s.sampler },
          { binding: 3, resource: tex.posters[0] },
          { binding: 4, resource: tex.posters[1] },
          { binding: 5, resource: tex.posters[2] },
          { binding: 6, resource: tex.posters[3] },
          { binding: 7, resource: tex.glyphs },
          { binding: 8, resource: tex.sprites },
        ],
      });
      this.bindKey = tex.key;
    }
    const encoder = device.createCommandEncoder({ label: 'flux 3d frame' });
    const pass = encoder.beginRenderPass({ colorAttachments: [{ view: this.ctx.getCurrentTexture().createView(), clearValue: { r: 0, g: 0, b: 0, a: 0 }, loadOp: 'clear', storeOp: 'store' }] });
    if (n) {
      pass.setPipeline(s.pipeline);
      pass.setBindGroup(0, this.bindGroup);
      pass.draw(4, n);
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
    this.uniforms.destroy();
  }
}
