/**
 * Instanced quads on a WebGPU canvas: a lens fills a QuadBatch (plain numbers
 * in one Float32Array, no objects per quad), and the renderer uploads it and
 * draws it in one call. The canvas is transparent where nothing is drawn, so
 * the room light behind the library still shows.
 */
import { BUFFER, type Gpu } from './device';
import { QUAD_WGSL } from './shaders';

/** Floats per quad (six vec4s; see `Quad` in shaders.ts). */
export const QUAD_FLOATS = 24;

export const KIND = { solid: 0, poster: 1, text: 2, ring: 3, shadow: 4, skeleton: 5 } as const;
export const FLAG = { gray: 1, hairline: 2, gradient: 8 } as const;

const T0 = performance.now();
/** Seconds on the clock every GPU lens and cache shares (poster fades are timed against it). */
export function clock(): number {
  return (performance.now() - T0) / 1000;
}

export type RGBA = readonly [number, number, number, number];

export class QuadBatch {
  data: Float32Array;
  n = 0;

  constructor(capacity = 1024) {
    this.data = new Float32Array(capacity * QUAD_FLOATS);
  }

  reset(): void {
    this.n = 0;
  }

  /** A new quad: rect set, `from` unset (static), opacity 1, no texture. Returns its float offset. */
  add(x: number, y: number, w: number, h: number, kind: number, radius = 0): number {
    if ((this.n + 1) * QUAD_FLOATS > this.data.length) {
      const grown = new Float32Array(this.data.length * 2);
      grown.set(this.data);
      this.data = grown;
    }
    const o = this.n * QUAD_FLOATS;
    const d = this.data;
    d[o] = x;
    d[o + 1] = y;
    d[o + 2] = w;
    d[o + 3] = h;
    d.fill(0, o + 4, o + QUAD_FLOATS);
    d[o + 16] = radius;
    d[o + 17] = kind;
    d[o + 19] = 1;
    d[o + 20] = -1;
    this.n++;
    return o;
  }

  from(o: number, x: number, y: number, w: number, h: number): void {
    const d = this.data;
    d[o + 4] = x;
    d[o + 5] = y;
    d[o + 6] = w;
    d[o + 7] = h;
  }

  color(o: number, c: RGBA, alpha = 1): void {
    const d = this.data;
    d[o + 8] = c[0];
    d[o + 9] = c[1];
    d[o + 10] = c[2];
    d[o + 11] = c[3] * alpha;
  }

  /** Texture rect, or a second colour for gradients and skeletons. */
  uv(o: number, a: number, b: number, c: number, e: number): void {
    const d = this.data;
    d[o + 12] = a;
    d[o + 13] = b;
    d[o + 14] = c;
    d[o + 15] = e;
  }

  texture(o: number, code: number, readyAt: number, texWidth: number): void {
    const d = this.data;
    d[o + 18] = code;
    d[o + 20] = readyAt;
    d[o + 22] = texWidth;
  }

  opacity(o: number, a: number): void {
    this.data[o + 19] = a;
  }

  extra(o: number, v: number): void {
    this.data[o + 22] = v;
  }

  flags(o: number, f: number): void {
    this.data[o + 23] = f;
  }
}

export interface FrameState {
  /** Canvas size in CSS pixels. */
  width: number;
  height: number;
  scrollX: number;
  scrollY: number;
  /** Pinch: everything scales by `scale` about (focalX, focalY), in screen CSS pixels. */
  focalX: number;
  focalY: number;
  scale: number;
  /** Device pixels per CSS pixel. */
  dpr: number;
  time: number;
  /** Morph progress, already eased. */
  t: number;
}

export interface Textures {
  /** Poster classes 0–3 (2D array views) and the glyph atlas. */
  posters: readonly GPUTextureView[];
  glyphs: GPUTextureView;
  /** Changes whenever any of the views is a different texture. */
  key: string;
}

interface Shared {
  pipeline: GPURenderPipeline;
  sampler: GPUSampler;
}

const shared = new WeakMap<GPUDevice, Shared>();
const preparing = new WeakMap<GPUDevice, Promise<boolean>>();

/**
 * Compile the quad pipeline, once per device however many lenses ask at the same
 * time (one pipeline, so every bind group matches it). False if it doesn't
 * compile on this GPU: the lens then uses its DOM version instead of drawing nothing.
 */
export function prepareQuads(gpu: Gpu): Promise<boolean> {
  let p = preparing.get(gpu.device);
  if (!p) {
    p = compileQuads(gpu);
    preparing.set(gpu.device, p);
  }
  return p;
}

async function compileQuads(gpu: Gpu): Promise<boolean> {
  try {
    const device = gpu.device;
    const module = device.createShaderModule({ label: 'flux quads', code: QUAD_WGSL });
    const pipeline = await device.createRenderPipelineAsync({
      label: 'flux quads',
      layout: 'auto',
      vertex: { module, entryPoint: 'vs' },
      fragment: {
        module,
        entryPoint: 'fs',
        targets: [
          {
            format: gpu.format,
            blend: {
              color: { srcFactor: 'one', dstFactor: 'one-minus-src-alpha', operation: 'add' },
              alpha: { srcFactor: 'one', dstFactor: 'one-minus-src-alpha', operation: 'add' },
            },
          },
        ],
      },
      primitive: { topology: 'triangle-strip' },
    });
    const sampler = device.createSampler({ magFilter: 'linear', minFilter: 'linear', mipmapFilter: 'linear', addressModeU: 'clamp-to-edge', addressModeV: 'clamp-to-edge' });
    shared.set(device, { pipeline, sampler });
    return true;
  } catch (err) {
    console.error('flux: the GPU lens pipeline failed to compile; using the DOM lens', err);
    return false;
  }
}

function sharedFor(gpu: Gpu): Shared {
  const s = shared.get(gpu.device);
  if (!s) throw new Error('prepareQuads() first');
  return s;
}

export class QuadRenderer {
  readonly canvas: HTMLCanvasElement;
  private gpu: Gpu;
  private ctx: GPUCanvasContext;
  private uniforms: GPUBuffer;
  private storage: GPUBuffer | null = null;
  private capacity = 0;
  private bindGroup: GPUBindGroup | null = null;
  private bindKey = '';
  private u = new Float32Array(12);

  constructor(gpu: Gpu, canvas: HTMLCanvasElement) {
    this.gpu = gpu;
    this.canvas = canvas;
    const ctx = canvas.getContext('webgpu') as GPUCanvasContext | null;
    if (!ctx) throw new Error('This canvas has no WebGPU context.');
    this.ctx = ctx;
    ctx.configure({ device: gpu.device, format: gpu.format, alphaMode: 'premultiplied' });
    this.uniforms = gpu.device.createBuffer({ label: 'flux frame', size: 48, usage: BUFFER.UNIFORM | BUFFER.COPY_DST });
  }

  /** Size the drawing buffer (device pixels). */
  resize(width: number, height: number): void {
    const w = Math.max(1, Math.round(width));
    const h = Math.max(1, Math.round(height));
    if (this.canvas.width !== w) this.canvas.width = w;
    if (this.canvas.height !== h) this.canvas.height = h;
  }

  draw(batch: QuadBatch, f: FrameState, tex: Textures): void {
    const device = this.gpu.device;
    const { pipeline, sampler } = sharedFor(this.gpu);
    const n = batch.n;
    const bytes = Math.max(1, n) * QUAD_FLOATS * 4;
    if (!this.storage || bytes > this.capacity) {
      this.storage?.destroy();
      this.capacity = Math.max(bytes, this.capacity * 2, 64 * QUAD_FLOATS * 4);
      this.storage = device.createBuffer({ label: 'flux quads', size: this.capacity, usage: BUFFER.STORAGE | BUFFER.COPY_DST });
      this.bindKey = '';
    }
    if (n) device.queue.writeBuffer(this.storage, 0, batch.data.buffer, batch.data.byteOffset, n * QUAD_FLOATS * 4);
    const u = this.u;
    u[0] = f.width;
    u[1] = f.height;
    u[2] = f.scrollX;
    u[3] = f.scrollY;
    u[4] = f.focalX;
    u[5] = f.focalY;
    u[6] = f.scale;
    u[7] = f.dpr;
    u[8] = f.time;
    u[9] = f.t;
    device.queue.writeBuffer(this.uniforms, 0, u);
    if (!this.bindGroup || this.bindKey !== tex.key) {
      this.bindGroup = device.createBindGroup({
        label: 'flux quads',
        layout: pipeline.getBindGroupLayout(0),
        entries: [
          { binding: 0, resource: { buffer: this.uniforms } },
          { binding: 1, resource: { buffer: this.storage } },
          { binding: 2, resource: sampler },
          { binding: 3, resource: tex.posters[0] },
          { binding: 4, resource: tex.posters[1] },
          { binding: 5, resource: tex.posters[2] },
          { binding: 6, resource: tex.posters[3] },
          { binding: 7, resource: tex.glyphs },
        ],
      });
      this.bindKey = tex.key;
    }
    const encoder = device.createCommandEncoder({ label: 'flux frame' });
    const pass = encoder.beginRenderPass({
      colorAttachments: [{ view: this.ctx.getCurrentTexture().createView(), clearValue: { r: 0, g: 0, b: 0, a: 0 }, loadOp: 'clear', storeOp: 'store' }],
    });
    if (n) {
      pass.setPipeline(pipeline);
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
