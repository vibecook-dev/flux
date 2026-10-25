/**
 * Which Flow to mount. For now the DOM Flow; the GPU port replaces this with the
 * same choice the Wall makes (wall/load.ts): WebGPU where there's a device, DOM elsewhere.
 */
import type { LensModule } from '../types';

export async function loadFlow(): Promise<LensModule> {
  return (await import('./flow')).flow;
}
