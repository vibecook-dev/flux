/**
 * One WebGPU device for every GPU lens. They share textures too: a poster
 * uploaded for the Wall also serves its preview in the lens gallery.
 *
 * `gpu()` resolves to null when the browser has no WebGPU, the adapter is a
 * software fallback (slower than the DOM), or `flux.render` is set to "dom".
 * Lenses then use their DOM versions. A device can also be lost later (driver
 * reset, GPU process crash); `onLost` tells the lenses so they can fall back too.
 */
import { prefs } from '../dom';

export interface Gpu {
  device: GPUDevice;
  /** The canvas format this platform prefers (bgra8unorm on Apple GPUs). */
  format: GPUTextureFormat;
  lost: boolean;
  onLost(fn: () => void): () => void;
}

/** Usage flags, with the spec's values (TypeScript's DOM types don't declare the constants). */
export const BUFFER = { COPY_DST: 0x08, UNIFORM: 0x40, STORAGE: 0x80 } as const;
export const TEXTURE = { COPY_SRC: 0x01, COPY_DST: 0x02, TEXTURE_BINDING: 0x04, RENDER_ATTACHMENT: 0x10 } as const;

export type RenderMode = 'auto' | 'gpu' | 'dom';

/** `flux.render` in localStorage: "dom" forces the DOM lenses, "gpu" allows a software adapter. */
export function renderMode(): RenderMode {
  const m = prefs.get<string>('render', 'auto');
  return m === 'gpu' || m === 'dom' ? m : 'auto';
}

let current: Gpu | null = null;
let pending: Promise<Gpu | null> | null = null;

function within<T>(p: Promise<T>, ms: number): Promise<T | null> {
  return Promise.race([p, new Promise<null>((r) => setTimeout(() => r(null), ms))]);
}

async function init(): Promise<Gpu | null> {
  try {
    const adapter = await within(navigator.gpu.requestAdapter(), 3000);
    if (!adapter) return null;
    const fallback = (adapter as unknown as { isFallbackAdapter?: boolean }).isFallbackAdapter ?? (adapter.info as unknown as { isFallbackAdapter?: boolean }).isFallbackAdapter;
    if (fallback && renderMode() !== 'gpu') return null;
    // Posters live in texture arrays: ask for more layers than the default 256 where the GPU has them.
    const layers = Math.min(adapter.limits.maxTextureArrayLayers, 1024);
    const device = await within(adapter.requestDevice({ requiredLimits: { maxTextureArrayLayers: layers } }), 3000);
    if (!device) return null;
    const listeners = new Set<() => void>();
    const g: Gpu = {
      device,
      format: navigator.gpu.getPreferredCanvasFormat(),
      lost: false,
      onLost(fn) {
        listeners.add(fn);
        return () => listeners.delete(fn);
      },
    };
    void device.lost.then((info) => {
      g.lost = true;
      if (current === g) current = null;
      if (info.reason !== 'destroyed') console.warn(`flux: the GPU device was lost (${info.message}); lenses fall back to the DOM`);
      for (const l of listeners) l();
    });
    // Each distinct error once: a broken frame would otherwise log every frame.
    const told = new Set<string>();
    device.addEventListener('uncapturederror', (e) => {
      const msg = e.error.message;
      if (told.has(msg) || told.size > 20) return;
      told.add(msg);
      console.error('flux: GPU error', msg);
    });
    current = g;
    return g;
  } catch {
    return null;
  }
}

/** The shared device, or null when lenses should draw with the DOM. */
export function gpu(): Promise<Gpu | null> {
  if (current && !current.lost) return Promise.resolve(current);
  if (renderMode() === 'dom' || typeof navigator === 'undefined' || !('gpu' in navigator) || !navigator.gpu) return Promise.resolve(null);
  pending ??= init().finally(() => {
    pending = null;
  });
  return pending;
}
