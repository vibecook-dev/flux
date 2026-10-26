/**
 * Which Flow to mount: the WebGPU one where the browser has a GPU device for us,
 * the DOM one everywhere else (older browsers, TV browsers, `flux.render` = "dom").
 */
import { gpu } from '../gpu/device';
import type { LensModule } from '../types';

export async function loadFlow(): Promise<LensModule> {
  // Ask for the device while the GPU Flow's code downloads.
  const maybeGpu = 'gpu' in navigator && Boolean(navigator.gpu);
  const [g, mod] = await Promise.all([gpu(), maybeGpu ? import('./gpu-flow').catch(() => null) : Promise.resolve(null)]);
  const gpuFlow = g && mod ? await mod.gpuFlow(g) : null;
  return gpuFlow ?? (await import('./flow')).flow;
}
