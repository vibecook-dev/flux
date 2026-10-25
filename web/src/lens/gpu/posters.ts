/**
 * Poster textures for GPU lenses, shared by every lens on the device.
 *
 * Posters live in four size classes (texture arrays with mipmaps, one poster per
 * layer), so each zoom level samples a texture close to its on-screen size: sharp,
 * and no wasted memory. A class grows with demand (up to a cap) and is released
 * when no lens has used it for a while.
 *
 * Loading never costs a frame: images are fetched nearest-first with a few in
 * flight, decoded off the main thread (createImageBitmap), and uploaded within a
 * small time budget per frame. A lens asks with `get` while drawing; until the
 * size it wants is ready it gets the nearest size that is, or nothing (and draws
 * the placeholder colour).
 */
import { artUrl } from '../../api/client';
import type { ArtRef } from '../../api/types';
import { TEXTURE, type Gpu } from './device';
import { clock } from './quads';
import { MIP_WGSL } from './shaders';
import { Slots } from './slots';

export interface PosterClass {
  w: number;
  h: number;
  mips: number;
  /** The server's resize bucket to ask for (`?w=`). */
  bucket: number;
  /** Most layers this class may grow to. */
  max: number;
}

export const CLASSES: readonly PosterClass[] = [
  { w: 128, h: 192, mips: 3, bucket: 160, max: 1024 },
  { w: 256, h: 384, mips: 4, bucket: 320, max: 384 },
  { w: 384, h: 576, mips: 4, bucket: 480, max: 192 },
  { w: 640, h: 960, mips: 5, bucket: 640, max: 72 },
];

/** The smallest class that is sharp `devicePx` wide (a little upscaling allowed). */
export function classFor(devicePx: number): number {
  for (let c = 0; c < CLASSES.length; c++) if (CLASSES[c].w >= devicePx * 0.92) return c;
  return CLASSES.length - 1;
}

export interface PosterHit {
  /** Texture code for the shader: class * 4096 + layer. */
  code: number;
  /** Texture width of the class (the shader picks the mip level from it). */
  texWidth: number;
  /** When this art first appeared (fades in from the placeholder). */
  readyAt: number;
}

type State = 'want' | 'loading' | 'decoded' | 'ready' | 'failed';

interface Entry {
  key: string;
  url: string;
  art: string;
  cls: number;
  state: State;
  prio: number;
  /** Last frame a lens asked for it. */
  seen: number;
  /** The frame `prio` belongs to (the best ask of that frame wins). */
  prioFrame: number;
  slot: number;
  ctl: AbortController | null;
  bitmap: ImageBitmap | null;
  /** The part of `bitmap` to show (cover-cropped to 2:3), in its pixels. */
  crop: Crop | null;
  retryAt: number;
}

interface Crop {
  x: number;
  y: number;
  w: number;
  h: number;
}

const MAX_LOADING = 8;
const UPLOAD_BUDGET_MS = 3;
const IDLE_RELEASE_MS = 20_000;

/** The centred part of a `sw`×`sh` image with the aspect of `w`×`h` (what "cover" shows). */
export function coverCrop(sw: number, sh: number, w: number, h: number): Crop {
  const want = w / h;
  const have = sw / sh;
  if (have > want * 1.02) {
    const cw = Math.max(1, Math.round(sh * want));
    return { x: Math.round((sw - cw) / 2), y: 0, w: cw, h: sh };
  }
  if (have < want / 1.02) {
    const ch = Math.max(1, Math.round(sw / want));
    return { x: 0, y: Math.round((sh - ch) / 2), w: sw, h: ch };
  }
  return { x: 0, y: 0, w: sw, h: sh };
}

/**
 * Fetch and decode a poster. Real art (JPEG, WebP, PNG) decodes off the main
 * thread and is not resized here: resizing an ImageBitmap runs on the main thread
 * in Chrome (12 ms for a 640×960 poster at "high"), so the GPU fits it instead
 * (PosterCache.fit). The mock's SVG posters can only be drawn on the main thread;
 * they are drawn straight at the slot's size.
 */
async function decodePoster(url: string, w: number, h: number, signal: AbortSignal): Promise<{ bitmap: ImageBitmap; crop: Crop }> {
  const res = await fetch(url, { signal });
  if (!res.ok) throw new Error(`art ${res.status}`);
  const blob = await res.blob();
  if (!blob.type.includes('svg')) {
    try {
      const bitmap = await createImageBitmap(blob);
      return { bitmap, crop: coverCrop(bitmap.width, bitmap.height, w, h) };
    } catch {
      // not a raster image after all: try it as an image element below
    }
  }
  const u = URL.createObjectURL(blob);
  try {
    const img = new Image();
    img.decoding = 'async';
    img.src = u;
    await img.decode();
    const opts: ImageBitmapOptions = { resizeWidth: w, resizeHeight: h, resizeQuality: 'low' };
    const sw = img.naturalWidth;
    const sh = img.naturalHeight;
    const c = sw && sh ? coverCrop(sw, sh, w, h) : null;
    const bitmap = c ? await createImageBitmap(img, c.x, c.y, c.w, c.h, opts) : await createImageBitmap(img, opts);
    return { bitmap, crop: { x: 0, y: 0, w: bitmap.width, h: bitmap.height } };
  } finally {
    URL.revokeObjectURL(u);
  }
}

class ClassStore {
  readonly spec: PosterClass;
  readonly index: number;
  tex: GPUTexture | null = null;
  view: GPUTextureView | null = null;
  layers = 0;
  slots: Slots;
  /** Distinct posters asked for in the last frame (sizes the texture). */
  demand = 0;
  lastUsed = 0;

  constructor(spec: PosterClass, index: number) {
    this.spec = spec;
    this.index = index;
    this.slots = new Slots(0);
  }
}

export class PosterCache {
  private gpu: Gpu;
  private stores: ClassStore[];
  private entries = new Map<string, Entry>();
  /** Per art URL: how many classes hold it, and when it first appeared. */
  private shown = new Map<string, { count: number; at: number }>();
  private decoded: Entry[] = [];
  private loading = 0;
  private dummy: GPUTexture;
  private dummyView: GPUTextureView;
  private mipPipeline: GPURenderPipeline | null = null;
  private mipSampler: GPUSampler;
  private listeners = new Set<() => void>();
  private demandNow: number[] = [0, 0, 0, 0];
  private demandSeen: Set<string>[] = [new Set(), new Set(), new Set(), new Set()];
  frame = 0;
  /** Bumps whenever a class texture is a different texture (renderers rebuild bind groups). */
  version = 1;
  /** Renderers keep animating until then (posters still fading in). */
  fadeUntil = 0;

  constructor(gpu: Gpu) {
    this.gpu = gpu;
    // No class grows past what this device allows.
    const cap = gpu.device.limits.maxTextureArrayLayers;
    this.stores = CLASSES.map((c, i) => new ClassStore({ ...c, max: Math.min(c.max, cap) }, i));
    this.dummy = gpu.device.createTexture({ label: 'flux poster placeholder', size: [1, 1, 1], format: 'rgba8unorm', usage: TEXTURE.TEXTURE_BINDING, dimension: '2d' });
    this.dummyView = this.dummy.createView({ dimension: '2d-array' });
    this.mipSampler = gpu.device.createSampler({ minFilter: 'linear', magFilter: 'linear', mipmapFilter: 'linear' });
  }

  /** Call when a decoded poster is waiting to upload (renderers redraw). */
  onReady(fn: () => void): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  /** Start a frame: demand counting and staleness are per frame. */
  begin(): void {
    this.frame++;
    for (let c = 0; c < this.stores.length; c++) {
      this.stores[c].demand = Math.max(this.demandNow[c], Math.round(this.stores[c].demand * 0.98));
      this.demandNow[c] = 0;
      this.demandSeen[c].clear();
    }
  }

  private count(cls: number, url: string): void {
    const seen = this.demandSeen[cls];
    if (!seen.has(url)) {
      seen.add(url);
      this.demandNow[cls]++;
    }
  }

  /**
   * The texture to draw `art` with at class `cls`, or null for the placeholder.
   * With `load`, a missing size is fetched (`prio`: lower is sooner).
   */
  get(art: ArtRef, cls: number, prio: number, load: boolean): PosterHit | null {
    const url = art.url;
    const store = this.stores[cls];
    store.lastUsed = performance.now();
    this.count(cls, url);
    const e = this.entries.get(`${cls}|${url}`);
    if (e?.state === 'ready') return this.hit(e);
    if (load) this.ask(url, cls, prio);
    else if (e) e.seen = this.frame;
    // Meanwhile, the nearest size that is ready: larger first (it mips down), then smaller.
    for (let d = 1; d < CLASSES.length; d++) {
      const up = cls + d < CLASSES.length ? this.entries.get(`${cls + d}|${url}`) : undefined;
      if (up?.state === 'ready') return this.hit(up);
      const down = cls - d >= 0 ? this.entries.get(`${cls - d}|${url}`) : undefined;
      if (down?.state === 'ready') return this.hit(down);
    }
    return null;
  }

  /** Ask for a poster without drawing it (the rows just off screen). */
  want(art: ArtRef, cls: number, prio: number): void {
    this.count(cls, art.url);
    this.ask(art.url, cls, prio);
  }

  private ask(url: string, cls: number, prio: number): void {
    const key = `${cls}|${url}`;
    const e = this.entries.get(key);
    if (!e) {
      this.entries.set(key, { key, url: artUrl({ url } as ArtRef, CLASSES[cls].bucket), art: url, cls, state: 'want', prio, prioFrame: this.frame, seen: this.frame, slot: -1, ctl: null, bitmap: null, crop: null, retryAt: 0 });
      return;
    }
    e.seen = this.frame;
    if (e.prioFrame !== this.frame) {
      e.prio = prio;
      e.prioFrame = this.frame;
    } else if (prio < e.prio) e.prio = prio;
    if (e.state === 'failed' && performance.now() > e.retryAt) e.state = 'want';
  }

  private hit(e: Entry): PosterHit {
    this.stores[e.cls].slots.touch(e.slot, this.frame);
    e.seen = this.frame;
    const at = this.shown.get(e.art)?.at ?? 0;
    return { code: e.cls * 4096 + e.slot, texWidth: CLASSES[e.cls].w, readyAt: at };
  }

  /** Start fetches for the most wanted posters; drop the ones nobody wants any more. */
  pump(): void {
    const stale = this.frame - 2;
    const waiting: Entry[] = [];
    for (const e of this.entries.values()) {
      if (e.state === 'want') {
        if (e.seen < stale) this.entries.delete(e.key);
        else waiting.push(e);
      } else if (e.state === 'loading' && e.seen < this.frame - 30) {
        e.ctl?.abort();
      }
    }
    if (!waiting.length || this.loading >= MAX_LOADING) return;
    waiting.sort((a, b) => a.prio - b.prio);
    for (const e of waiting) {
      if (this.loading >= MAX_LOADING) break;
      this.start(e);
    }
  }

  private start(e: Entry): void {
    const spec = CLASSES[e.cls];
    e.state = 'loading';
    e.ctl = new AbortController();
    this.loading++;
    decodePoster(e.url, spec.w, spec.h, e.ctl.signal).then(
      ({ bitmap, crop }) => {
        this.loading--;
        e.ctl = null;
        if (this.entries.get(e.key) !== e) {
          bitmap.close();
          return;
        }
        e.bitmap = bitmap;
        e.crop = crop;
        e.state = 'decoded';
        this.decoded.push(e);
        for (const l of this.listeners) l();
      },
      () => {
        this.loading--;
        e.ctl = null;
        if (this.entries.get(e.key) !== e) return;
        if (e.seen < this.frame - 30) this.entries.delete(e.key);
        else {
          e.state = 'failed';
          e.retryAt = performance.now() + 30_000;
        }
      },
    );
  }

  /** True when decoded posters are waiting to upload. */
  get pending(): boolean {
    return this.decoded.length > 0;
  }

  /** Upload decoded posters within the frame's budget (call before drawing). */
  upload(): boolean {
    if (!this.decoded.length) return false;
    const device = this.gpu.device;
    const t0 = performance.now();
    const now = clock();
    this.decoded.sort((a, b) => a.prio - b.prio);
    const encoder = device.createCommandEncoder({ label: 'flux poster mips' });
    const staging: GPUTexture[] = [];
    let any = false;
    const keep: Entry[] = [];
    for (let i = 0; i < this.decoded.length; i++) {
      const e = this.decoded[i];
      if (performance.now() - t0 > UPLOAD_BUDGET_MS) {
        keep.push(...this.decoded.slice(i));
        break;
      }
      const bmp = e.bitmap;
      if (!bmp || this.entries.get(e.key) !== e) {
        bmp?.close();
        continue;
      }
      // Nobody has looked at it for a while: don't spend a slot on it.
      if (e.seen < this.frame - 60) {
        bmp.close();
        this.entries.delete(e.key);
        continue;
      }
      const store = this.stores[e.cls];
      this.reserve(store, encoder);
      const { slot, evicted } = store.slots.claim(e.key, this.frame, this.frame - 3);
      if (slot < 0) {
        // Every slot is on screen right now. Wait for the class to grow; at its cap, let it go for a while.
        if (store.layers < store.spec.max) keep.push(e);
        else {
          bmp.close();
          e.bitmap = null;
          e.state = 'failed';
          e.retryAt = performance.now() + 5000;
        }
        continue;
      }
      if (evicted) this.evict(evicted);
      const crop = e.crop ?? { x: 0, y: 0, w: bmp.width, h: bmp.height };
      if (crop.x === 0 && crop.y === 0 && crop.w === store.spec.w && crop.h === store.spec.h && bmp.width === crop.w && bmp.height === crop.h) {
        // Already the slot's size: copy it straight in.
        device.queue.copyExternalImageToTexture({ source: bmp }, { texture: store.tex as GPUTexture, origin: [0, 0, slot], premultipliedAlpha: true }, [store.spec.w, store.spec.h, 1]);
      } else this.fit(encoder, store, slot, bmp, crop, staging);
      bmp.close();
      e.bitmap = null;
      e.crop = null;
      this.mips(encoder, store, slot);
      e.slot = slot;
      e.state = 'ready';
      const s = this.shown.get(e.art);
      if (s) s.count++;
      else this.shown.set(e.art, { count: 1, at: now });
      this.fadeUntil = Math.max(this.fadeUntil, now + 0.32);
      any = true;
    }
    device.queue.submit([encoder.finish()]);
    if (staging.length) void device.queue.onSubmittedWorkDone().then(() => staging.forEach((t) => t.destroy()));
    this.decoded = keep;
    return any;
  }

  /**
   * Resample a decoded poster into a slot on the GPU: copy the visible part as it
   * came (no CPU resize), give a much larger source its own mip chain, then draw
   * it into the slot through the linear sampler.
   */
  private fit(encoder: GPUCommandEncoder, store: ClassStore, layer: number, bmp: ImageBitmap, crop: Crop, staging: GPUTexture[]): void {
    const device = this.gpu.device;
    const ratio = Math.max(crop.w / store.spec.w, crop.h / store.spec.h);
    const levels = ratio >= 2 ? Math.min(1 + Math.floor(Math.log2(ratio)), 8) : 1;
    const tex = device.createTexture({
      label: 'flux poster staging',
      size: [crop.w, crop.h],
      mipLevelCount: levels,
      format: 'rgba8unorm',
      usage: TEXTURE.TEXTURE_BINDING | TEXTURE.COPY_DST | TEXTURE.RENDER_ATTACHMENT,
    });
    staging.push(tex);
    device.queue.copyExternalImageToTexture({ source: bmp, origin: [crop.x, crop.y] }, { texture: tex, premultipliedAlpha: true }, [crop.w, crop.h]);
    for (let m = 1; m < levels; m++) {
      this.blit(encoder, tex.createView({ baseMipLevel: m - 1, mipLevelCount: 1 }), tex.createView({ baseMipLevel: m, mipLevelCount: 1 }));
    }
    const slot = (store.tex as GPUTexture).createView({ dimension: '2d', baseMipLevel: 0, mipLevelCount: 1, baseArrayLayer: layer, arrayLayerCount: 1 });
    this.blit(encoder, tex.createView(), slot);
  }

  /** Draw `src` over all of `dst` through the linear sampler (mip levels, fits). */
  private blit(encoder: GPUCommandEncoder, src: GPUTextureView, dst: GPUTextureView): void {
    if (!this.mipPipeline) return;
    const bind = this.gpu.device.createBindGroup({
      layout: this.mipPipeline.getBindGroupLayout(0),
      entries: [
        { binding: 0, resource: src },
        { binding: 1, resource: this.mipSampler },
      ],
    });
    const pass = encoder.beginRenderPass({ colorAttachments: [{ view: dst, loadOp: 'clear', storeOp: 'store', clearValue: { r: 0, g: 0, b: 0, a: 0 } }] });
    pass.setPipeline(this.mipPipeline);
    pass.setBindGroup(0, bind);
    pass.draw(3);
    pass.end();
  }

  private evict(key: string): void {
    const e = this.entries.get(key);
    if (!e) return;
    this.entries.delete(key);
    const s = this.shown.get(e.art);
    if (s && --s.count <= 0) this.shown.delete(e.art);
  }

  /** Make room in a class texture for the posters it is asked for (grows by doubling, up to its cap). */
  private reserve(store: ClassStore, encoder: GPUCommandEncoder): void {
    const want = Math.min(store.spec.max, Math.max(16, Math.ceil((store.demand + 8) * 1.25)));
    if (store.tex && store.layers >= want) return;
    if (store.tex && store.slots.size < store.layers) return;
    let layers = Math.max(16, store.layers);
    while (layers < want) layers *= 2;
    layers = Math.min(layers, store.spec.max);
    if (store.tex && layers <= store.layers) return;
    const device = this.gpu.device;
    const tex = device.createTexture({
      label: `flux posters ${store.spec.w}`,
      size: [store.spec.w, store.spec.h, layers],
      mipLevelCount: store.spec.mips,
      format: 'rgba8unorm',
      usage: TEXTURE.TEXTURE_BINDING | TEXTURE.COPY_DST | TEXTURE.COPY_SRC | TEXTURE.RENDER_ATTACHMENT,
    });
    if (store.tex) {
      for (let m = 0; m < store.spec.mips; m++) {
        encoder.copyTextureToTexture({ texture: store.tex, mipLevel: m }, { texture: tex, mipLevel: m }, [store.spec.w >> m, store.spec.h >> m, store.layers]);
      }
      // Destroyed once the copy has run.
      const old = store.tex;
      queueMicrotask(() => void device.queue.onSubmittedWorkDone().then(() => old.destroy()));
    }
    store.tex = tex;
    store.view = tex.createView({ dimension: '2d-array' });
    store.layers = layers;
    store.slots.grow(layers);
    this.version++;
  }

  private preparing: Promise<boolean> | null = null;

  /** Compile the mip pipeline (once) before the first poster arrives. False if it doesn't compile here. */
  prepare(): Promise<boolean> {
    this.preparing ??= this.compileMips();
    return this.preparing;
  }

  private async compileMips(): Promise<boolean> {
    try {
      const device = this.gpu.device;
      const module = device.createShaderModule({ label: 'flux mips', code: MIP_WGSL });
      this.mipPipeline = await device.createRenderPipelineAsync({
        label: 'flux mips',
        layout: 'auto',
        vertex: { module, entryPoint: 'vs' },
        fragment: { module, entryPoint: 'fs', targets: [{ format: 'rgba8unorm' }] },
        primitive: { topology: 'triangle-list' },
      });
      return true;
    } catch (err) {
      console.error('flux: the poster mip pipeline failed to compile', err);
      return false;
    }
  }

  private mips(encoder: GPUCommandEncoder, store: ClassStore, layer: number): void {
    const tex = store.tex as GPUTexture;
    for (let m = 1; m < store.spec.mips; m++) {
      const src = tex.createView({ dimension: '2d', baseMipLevel: m - 1, mipLevelCount: 1, baseArrayLayer: layer, arrayLayerCount: 1 });
      const dst = tex.createView({ dimension: '2d', baseMipLevel: m, mipLevelCount: 1, baseArrayLayer: layer, arrayLayerCount: 1 });
      this.blit(encoder, src, dst);
    }
  }

  /** Texture views for the shader, with a stand-in for classes not allocated yet. */
  views(): GPUTextureView[] {
    return this.stores.map((s) => s.view ?? this.dummyView);
  }

  private holders = 0;
  private releaseT: ReturnType<typeof setTimeout> | null = null;

  /**
   * A lens drawing from the cache holds it; call the returned function when it
   * unmounts. 30 s after the last one lets go, every class is freed.
   */
  retain(): () => void {
    this.holders++;
    if (this.releaseT) {
      clearTimeout(this.releaseT);
      this.releaseT = null;
    }
    let released = false;
    return () => {
      if (released) return;
      released = true;
      if (--this.holders > 0) return;
      this.releaseT = setTimeout(() => {
        this.releaseT = null;
        if (this.holders === 0) this.trim(true);
      }, 30_000);
    };
  }

  /** Free classes no lens has drawn from for a while (memory goes back to the system). */
  trim(force = false): void {
    const now = performance.now();
    for (const store of this.stores) {
      if (!store.tex || (!force && now - store.lastUsed < IDLE_RELEASE_MS)) continue;
      const tex = store.tex;
      void this.gpu.device.queue.onSubmittedWorkDone().then(() => tex.destroy());
      store.tex = null;
      store.view = null;
      store.layers = 0;
      store.slots = new Slots(0);
      store.demand = 0;
      for (const [k, e] of this.entries) if (e.cls === store.index && (e.state === 'ready' || e.state === 'decoded')) {
        e.bitmap?.close();
        this.entries.delete(k);
        const s = this.shown.get(e.art);
        if (s && e.state === 'ready' && --s.count <= 0) this.shown.delete(e.art);
      }
      this.decoded = this.decoded.filter((e) => e.cls !== store.index);
      this.version++;
    }
  }
}

const caches = new WeakMap<GPUDevice, PosterCache>();

/** The poster cache for this device (one per app). */
export function posterCache(gpu: Gpu): PosterCache {
  let c = caches.get(gpu.device);
  if (!c) {
    c = new PosterCache(gpu);
    caches.set(gpu.device, c);
  }
  return c;
}
