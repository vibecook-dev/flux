/**
 * Which Shelf to mount: the WebGPU one where the browser has a GPU device for us,
 * the DOM one everywhere else (older browsers, TV browsers, `flux.render` = "dom").
 */
import { gpu } from '../gpu/device';
import type { LensModule } from '../types';

export async function loadShelf(): Promise<LensModule> {
  // Ask for the device while the GPU Shelf's code downloads.
  const maybeGpu = 'gpu' in navigator && Boolean(navigator.gpu);
  const [g, mod] = await Promise.all([gpu(), maybeGpu ? import('./gpu-shelf').catch(() => null) : Promise.resolve(null)]);
  const gpuShelf = g && mod ? await mod.gpuShelf(g) : null;
  return gpuShelf ?? (await import('./shelf')).shelf;
}
