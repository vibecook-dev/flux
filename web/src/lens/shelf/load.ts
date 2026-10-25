/**
 * Which Shelf to mount. For now the DOM Shelf; the GPU port replaces this with the
 * same choice the Wall makes (wall/load.ts): WebGPU where there's a device, DOM elsewhere.
 */
import type { LensModule } from '../types';

export async function loadShelf(): Promise<LensModule> {
  return (await import('./shelf')).shelf;
}
