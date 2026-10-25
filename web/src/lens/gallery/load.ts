/**
 * Which Gallery to mount: the WebGPU one where the browser has a GPU device for us,
 * the DOM one everywhere else (older browsers, TV browsers, `flux.render` = "dom").
 */
import { gpu } from '../gpu/device';
import type { LensModule } from '../types';

export async function loadGallery(): Promise<LensModule> {
  // Ask for the device while the GPU Gallery's code downloads.
  const maybeGpu = 'gpu' in navigator && Boolean(navigator.gpu);
  const [g, mod] = await Promise.all([gpu(), maybeGpu ? import('./gpu-gallery').catch(() => null) : Promise.resolve(null)]);
  const gpuGallery = g && mod ? await mod.gpuGallery(g) : null;
  return gpuGallery ?? (await import('./gallery')).gallery;
}
