/**
 * Painted sprites for GPU lenses: artwork a lens draws itself with Canvas 2D (a
 * VHS spine, a wall label), cached as the layers of one texture array. Each one
 * is painted once and kept while it is drawn; layers are reused least recently
 * drawn first (slots.ts), and a frame paints at most a few new ones, so a lens
 * flying through 10,000 titles only paints what comes into view, spread over frames.
 *
 * Sizes are device pixels: a lens keys its sprites by what they show and its
 * pixel ratio, and paints them at that ratio.
 */
import { TEXTURE, type Gpu } from './device';
import { Slots } from './slots';

export interface Sprite {
  /** Layer in the sheet (the shader's texture index for sprites). */
  layer: number;
  /** The painted part of the cell, as texture coordinates (0,0)–(u1,v1). */
  u1: number;
  v1: number;
}

export class SpriteSheet {
  /** Cell size, device pixels. */
  readonly cellW: number;
  readonly cellH: number;
  /** Bumps when the texture is a different one (renderers rebuild bind groups). */
  version = 1;
  private gpu: Gpu;
  private tex: GPUTexture;
  private texView: GPUTextureView;
  private layers: number;
  private max: number;
  private slots: Slots;
  private canvas: HTMLCanvasElement;
  private ctx: CanvasRenderingContext2D;
  private frame = 0;
  private painted = 0;

  constructor(gpu: Gpu, cellW: number, cellH: number, max = 256) {
    this.gpu = gpu;
    this.cellW = Math.max(1, Math.round(cellW));
    this.cellH = Math.max(1, Math.round(cellH));
    this.max = Math.min(max, gpu.device.limits.maxTextureArrayLayers);
    this.layers = Math.min(32, this.max);
    this.slots = new Slots(this.layers);
    this.tex = this.create(this.layers);
    this.texView = this.tex.createView({ dimension: '2d-array' });
    this.canvas = document.createElement('canvas');
    this.canvas.width = this.cellW;
    this.canvas.height = this.cellH;
    const ctx = this.canvas.getContext('2d');
    if (!ctx) throw new Error('No 2D canvas for sprites.');
    this.ctx = ctx;
  }

  get view(): GPUTextureView {
    return this.texView;
  }

  private create(layers: number): GPUTexture {
    return this.gpu.device.createTexture({
      label: `flux sprites ${this.cellW}×${this.cellH}`,
      size: [this.cellW, this.cellH, layers],
      format: 'rgba8unorm',
      usage: TEXTURE.TEXTURE_BINDING | TEXTURE.COPY_DST | TEXTURE.COPY_SRC | TEXTURE.RENDER_ATTACHMENT,
    });
  }

  /** Start a frame (each frame may paint `budget` new sprites). */
  begin(): void {
    this.frame++;
    this.painted = 0;
  }

  /**
   * The sprite for `key`, painting it with `paint(ctx, w, h)` (device pixels, a
   * clean transparent cell) the first time. `w`×`h` is the part of the cell it
   * uses (default: all of it). Null when this frame's painting budget is spent
   * or every layer is on screen: draw a placeholder and ask again next frame.
   */
  get(key: string, paint: (ctx: CanvasRenderingContext2D, w: number, h: number) => void, w = this.cellW, h = this.cellH, budget = 10): Sprite | null {
    const cw = Math.min(this.cellW, Math.max(1, Math.round(w)));
    const ch = Math.min(this.cellH, Math.max(1, Math.round(h)));
    const have = this.slots.find(key);
    if (have >= 0) {
      this.slots.touch(have, this.frame);
      return { layer: have, u1: cw / this.cellW, v1: ch / this.cellH };
    }
    if (this.painted >= budget) return null;
    if (this.slots.size >= this.layers && this.layers < this.max) this.grow();
    const { slot } = this.slots.claim(key, this.frame, this.frame - 2);
    if (slot < 0) return null;
    const ctx = this.ctx;
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.clearRect(0, 0, this.cellW, this.cellH);
    ctx.save();
    paint(ctx, cw, ch);
    ctx.restore();
    this.gpu.device.queue.copyExternalImageToTexture({ source: this.canvas }, { texture: this.tex, origin: [0, 0, slot], premultipliedAlpha: true }, [this.cellW, this.cellH, 1]);
    this.painted++;
    return { layer: slot, u1: cw / this.cellW, v1: ch / this.cellH };
  }

  /** Forget a sprite (its content changed); it is painted again when next asked for. */
  forget(key: string): void {
    this.slots.release(key);
  }

  private grow(): void {
    const layers = Math.min(this.max, this.layers * 2);
    const tex = this.create(layers);
    const device = this.gpu.device;
    const encoder = device.createCommandEncoder({ label: 'flux sprites grow' });
    encoder.copyTextureToTexture({ texture: this.tex }, { texture: tex }, [this.cellW, this.cellH, this.layers]);
    device.queue.submit([encoder.finish()]);
    const old = this.tex;
    void device.queue.onSubmittedWorkDone().then(() => old.destroy());
    this.tex = tex;
    this.texView = tex.createView({ dimension: '2d-array' });
    this.layers = layers;
    this.slots.grow(layers);
    this.version++;
  }

  destroy(): void {
    this.tex.destroy();
  }
}
