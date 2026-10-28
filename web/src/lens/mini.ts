/**
 * A lens as a small live preview (the lens gallery's cards): mounted with
 * `env.mini`, no input, no accessory, no focus reports. Render it at full window
 * size inside a scaled container so it looks exactly as it will.
 */
import type { ItemSummary } from '../api/types';
import { lensFocus } from './bridge';
import { reducedMotion } from './dom';
import type { Insets, LensContext, LensDef, LensView } from './types';

export interface MiniOptions {
  items: readonly ItemSummary[];
  total: number;
  theme: 'dark' | 'light';
  phone: boolean;
  sectionOf: (item: ItemSummary) => string;
  /** Where the shell's glass will sit: the preview keeps clear of it as the lens will (default: small margins). */
  safe?: Insets;
}

export interface MiniLens {
  view: LensView;
  destroy(): void;
}

export async function mountMini(def: LensDef, container: HTMLElement, o: MiniOptions): Promise<MiniLens> {
  const mod = await def.load();
  const root = document.createElement('div');
  root.style.cssText = 'position:absolute;inset:0;overflow:hidden';
  root.dataset.lens = def.id;
  container.append(root);
  const ctx: LensContext = {
    items: o.items,
    total: o.total,
    // At the title you're on, as the lens itself would open (choosing it grows the preview into it).
    focus: lensFocus(),
    safe: o.safe ?? { top: 24, bottom: 24, left: 16, right: 16 },
    env: { mini: true, reducedMotion: reducedMotion(), theme: o.theme, phone: o.phone },
    setFocus: () => undefined,
    open: () => undefined,
    play: () => undefined,
    setAccessory: () => undefined,
    scrolled: () => undefined,
    sectionOf: o.sectionOf,
    intent: () => undefined,
  };
  const view = mod.mount(root, ctx);
  return {
    view,
    destroy() {
      view.destroy();
      root.remove();
    },
  };
}
