/**
 * The lens contract. A lens decides how a space looks and moves; the shell owns
 * everything else: spaces, search, filters, sort, the island, the lens bar, the
 * detail overlay, playback, identity and the network.
 *
 * Lenses are plain DOM modules (no React), so they are cheap to write, easy to
 * load on demand, and portable to other clients. Rules every lens follows:
 *
 * 1. Draw content only: no menus, buttons or fields of its own (one accessory at most).
 * 2. Keep the focused title visible and report focus with `ctx.setFocus`.
 * 3. Move with arrows (`move`) and open with Enter (`key`); Escape belongs to the shell.
 * 4. Keep key content clear of `ctx.safe`; art may run under the glass.
 * 5. Honour `env.reducedMotion` (no camera drift, no springs) and `env.mini`.
 * 6. Build artwork with `art.ts`, so progress, watched, new and source labels look
 *    the same everywhere and frame/AI art is always labelled.
 * 7. Stay fast at 10,000 titles: render only what is near the focus or on screen.
 */
import type { Id, ItemSummary } from '../api/types';

export type Dir = 'left' | 'right' | 'up' | 'down';

export interface Insets {
  top: number;
  bottom: number;
  left: number;
  right: number;
}

export interface LensEnv {
  /** A small live preview in the lens gallery: no input, no accessory, no focus reports. */
  mini: boolean;
  reducedMotion: boolean;
  theme: 'dark' | 'light';
  phone: boolean;
}

export interface LensContext {
  /** Searched, filtered and sorted by the shell. Grows while a library is still loading. */
  readonly items: readonly ItemSummary[];
  /** Titles in the space; more than `items.length` while pages are still arriving. */
  readonly total: number;
  /** The focused title, shared by every lens: switching lens keeps your place. */
  readonly focus: Id | null;
  /** Where the floating glass sits; keep key content clear of it. */
  readonly safe: Insets;
  readonly env: LensEnv;
  setFocus(id: Id): void;
  /** Open the title's detail; `art` is where its poster is now (the shell flies it from there). */
  open(id: Id, art?: Element | null): void;
  play(id: Id): void;
  /** One small control lent to the lens bar (zoom, a scrubber, a tour). */
  setAccessory(node: HTMLElement | null): void;
  /** Lets the shell tuck the bars away while you browse. */
  scrolled(dir: 'down' | 'up' | 'top'): void;
  /** Section label under the current sort ("1990s", "A", "This month"; "" for none). */
  sectionOf(item: ItemSummary): string;
  /** Hover or focus intent: warm the detail page's data. */
  intent(item: ItemSummary): void;
}

export interface LensView {
  /** New results (search typing, a filter, a page of a loading library). */
  update(items: readonly ItemSummary[], total: number): void;
  /** Arrow keys and TV remotes; true when handled. */
  move(dir: Dir): boolean;
  /** Lens-specific keys (Enter opens; +/− zoom…); true when handled. */
  key?(e: KeyboardEvent): boolean;
  /** Where a title's artwork is on screen, for the flight into detail and back. */
  rectOf(id: Id): DOMRect | null;
  artOf?(id: Id): HTMLElement | null;
  /** Bring a title into view and focus it (search Enter, coming back from detail). */
  reveal?(id: Id): void;
  /** Hide one title's art (its poster is flying into the detail page), or show it again with null. */
  hide?(id: Id | null): void;
  /** Stop animation loops while covered by detail or the player. */
  pause?(paused: boolean): void;
  destroy(): void;
}

export interface LensModule {
  mount(root: HTMLElement, ctx: LensContext): LensView;
}

export interface LensDef {
  id: string;
  name: string;
  /** SVG markup on a 24 grid, stroked with currentColor. */
  icon: string;
  /** One sentence for the lens gallery. */
  blurb: string;
  /** Wants the scroll-edge blur under the chrome (lenses that scroll art beneath the bars). */
  edges?: boolean;
  /** Who made it: flux itself, the lens kit examples, you on this device, or a
   * lens kept on the server (`lens.add`, often by an agent) for every device. */
  author: 'flux' | 'kit' | 'you' | 'server';
  /** For a server lens: the agent that added it, if one did. */
  by?: string | null;
  /** For a server lens: kept for everyone, not just you. */
  everyone?: boolean;
  /** Library kinds it suits; the lens bar hides it elsewhere. Default: all but `videos`. */
  kinds?: readonly ('movies' | 'shows' | 'mixed' | 'videos' | 'all')[];
  load(): Promise<LensModule>;
}
