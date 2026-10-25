/**
 * Which Wall to mount: the WebGPU one where the browser has a GPU device for us,
 * the DOM one everywhere else (older browsers, TV browsers, `flux.render` = "dom").
 */
import { gpu } from '../gpu/device';
import type { LensModule } from '../types';

export async function loadWall(): Promise<LensModule> {
  // Ask for the device while the GPU Wall's code downloads.
  const maybeGpu = 'gpu' in navigator && Boolean(navigator.gpu);
  const [g, mod] = await Promise.all([gpu(), maybeGpu ? import('./gpu-wall').catch(() => null) : Promise.resolve(null)]);
  const gpuWall = g && mod ? await mod.gpuWall(g) : null;
  return gpuWall ?? (await import('./wall')).wall;
}
