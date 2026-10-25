/**
 * Every lens flux knows: its own four, the lens-kit examples, and lenses you
 * added (kit specs kept on this device). Names, icons and blurbs are here so the
 * lens bar and the gallery can show them; the code loads only when a lens is used.
 */
import type { LibraryKind } from '../api/types';
import { prefs } from './dom';
import type { LensDef, LensModule } from './types';

const s = (d: string) =>
  `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${d}</svg>`;

export const LENS_ICONS = {
  wall: s('<rect x="3.5" y="3.5" width="4.6" height="7" rx="1.1"/><rect x="9.7" y="3.5" width="4.6" height="7" rx="1.1"/><rect x="15.9" y="3.5" width="4.6" height="7" rx="1.1"/><rect x="3.5" y="13.5" width="4.6" height="7" rx="1.1"/><rect x="9.7" y="13.5" width="4.6" height="7" rx="1.1"/><rect x="15.9" y="13.5" width="4.6" height="7" rx="1.1"/>'),
  flow: s('<rect x="8.2" y="4" width="7.6" height="11.5" rx="1.2"/><path d="M5.6 5.8 2.6 7.3v6.9l3 1.5zM18.4 5.8l3 1.5v6.9l-3 1.5z"/><path d="M8.2 19.5h7.6" opacity=".55"/>'),
  shelf: s('<path d="M2.5 20.5h19"/><rect x="4" y="5" width="3" height="15.5" rx=".5"/><rect x="8" y="7.5" width="3" height="13" rx=".5"/><rect x="12" y="4" width="3" height="16.5" rx=".5"/><path d="m16.4 8.6 2.9-.8 3.1 12.5"/>'),
  gallery: s('<path d="M2.5 4 8.5 7.5v9l-6 3.5M21.5 4l-6 3.5v9l6 3.5"/><rect x="10.4" y="9.6" width="3.2" height="4.4" rx=".4"/><path d="M8.5 7.5h7M8.5 16.5h7"/>'),
  folders: s('<path d="M3.5 6.5h6l2 2h9v10h-17z"/><path d="M3.5 10.5h17" opacity=".5"/>'),
  helix: s('<path d="M7 3c0 4.5 10 4.5 10 9s-10 4.5-10 9M17 3c0 4.5-10 4.5-10 9s10 4.5 10 9"/><path d="M8.6 6h6.8M8.6 18h6.8" opacity=".55"/>'),
  timeline: s('<path d="M2.5 16.5h19"/><path d="M5 16.5v-9M10 16.5v-6M15 16.5v-10M20 16.5v-4"/><circle cx="5" cy="7" r="1"/><circle cx="10" cy="10" r="1"/><circle cx="15" cy="6" r="1"/><circle cx="20" cy="12" r="1"/>'),
  strip: s('<rect x="2.5" y="5" width="19" height="14" rx="1.5"/><path d="M2.5 8h19M2.5 16h19M8.8 8v8M15.2 8v8"/><path d="M5 6.4h.01M9 6.4h.01M13 6.4h.01M17 6.4h.01M5 17.6h.01M9 17.6h.01M13 17.6h.01M17 17.6h.01"/>'),
  pin: s('<rect x="3.5" y="5.5" width="8" height="10" rx="1" transform="rotate(-8 7.5 10.5)"/><rect x="11" y="8" width="9" height="11" rx="1" transform="rotate(7 15.5 13.5)"/><circle cx="7.3" cy="5.4" r="1"/><circle cx="15.7" cy="8" r="1"/>'),
  deck: s('<rect x="6.5" y="6" width="11" height="15" rx="1.6"/><path d="M8.5 3.5h7M7.5 4.75h9" opacity=".6"/>'),
  sparkle: s('<path d="M12 3.5c.6 4.3 2.2 5.9 6.5 6.5-4.3.6-5.9 2.2-6.5 6.5-.6-4.3-2.2-5.9-6.5-6.5 4.3-.6 5.9-2.2 6.5-6.5z"/><path d="M18.5 15.5c.25 1.8.95 2.5 2.75 2.75-1.8.25-2.5.95-2.75 2.75-.25-1.8-.95-2.5-2.75-2.75 1.8-.25 2.5-.95 2.75-2.75z"/>'),
  more: s('<rect x="4" y="4" width="6.5" height="6.5" rx="1.6"/><rect x="13.5" y="4" width="6.5" height="6.5" rx="1.6"/><rect x="4" y="13.5" width="6.5" height="6.5" rx="1.6"/><path d="M16.75 13.5v6.5M13.5 16.75H20"/>'),
} as const;

const FILM_KINDS: readonly LibraryKind[] = ['movies', 'shows', 'mixed'];

export const FIRST_PARTY: LensDef[] = [
  {
    id: 'wall',
    name: 'Wall',
    icon: LENS_ICONS.wall,
    blurb: 'Everything at a glance. Pinch, or press + and −, to go from a mosaic of decades to posters with captions.',
    edges: true,
    author: 'flux',
    load: () => import('./wall/load').then((m) => m.loadWall()),
  },
  {
    id: 'flow',
    name: 'Flow',
    icon: LENS_ICONS.flow,
    blurb: 'One poster faces you and the rest wait in two stacks you flick through, like an iPod.',
    author: 'flux',
    load: () => import('./flow/flow').then((m) => m.flow),
  },
  {
    id: 'shelf',
    name: 'Shelf',
    icon: LENS_ICONS.shelf,
    blurb: 'Tapes on a shelf, the way a video shop kept them. Point at one to pull it out.',
    author: 'flux',
    load: () => import('./shelf/shelf').then((m) => m.shelf),
  },
  {
    id: 'gallery',
    name: 'Gallery',
    icon: LENS_ICONS.gallery,
    blurb: 'Walk an exhibition of your library: framed posters, wall labels, and a room for every decade.',
    author: 'flux',
    load: () => import('./gallery/gallery').then((m) => m.gallery),
  },
];

/** A videos library's own view: its folders, as on disk (not a lens module; the shell renders it). */
export const FOLDERS_LENS: LensDef = {
  id: 'folders',
  name: 'Folders',
  icon: LENS_ICONS.folders,
  blurb: 'Home videos by folder, as they are on disk.',
  author: 'flux',
  kinds: ['videos'],
  load: () => Promise.reject(new Error('Folders is drawn by the shell')),
};

const KIT_EXAMPLES: { id: string; name: string; icon: keyof typeof LENS_ICONS; blurb: string }[] = [
  { id: 'tonight', name: 'Tonight', icon: 'deck', blurb: 'One title at a time, the rest waiting behind it. For nights you can’t decide.' },
  { id: 'helix', name: 'Helix', icon: 'helix', blurb: 'Your library wound into a slow spiral. Scroll to turn it.' },
  { id: 'timeline', name: 'Timeline', icon: 'timeline', blurb: 'Every title on one line of years, gaps included. Titles from the same year pile up.' },
  { id: 'strip', name: 'Film strip', icon: 'strip', blurb: 'Posters as frames on a reel of 35 mm, edge codes and all.' },
  { id: 'corkboard', name: 'Corkboard', icon: 'pin', blurb: 'Posters pinned up a little crooked, the way they were in your first flat.' },
];

export const KIT_LENSES: LensDef[] = KIT_EXAMPLES.map((k) => ({
  id: k.id,
  name: k.name,
  icon: LENS_ICONS[k.icon],
  blurb: k.blurb,
  edges: true,
  author: 'kit',
  load: (): Promise<LensModule> => import('./kit/examples').then((m) => m.exampleLens(k.id)),
}));

/** A kit spec you added on this device (see lens/kit). */
export interface StoredLens {
  id: string;
  name: string;
  blurb: string;
  spec: unknown;
}

export function yourLenses(): LensDef[] {
  const stored = prefs.get<StoredLens[]>('lenses.mine', []);
  return (Array.isArray(stored) ? stored : [])
    .filter((l) => l && typeof l.id === 'string' && typeof l.name === 'string')
    .map((l) => ({
      id: l.id,
      name: l.name,
      icon: LENS_ICONS.sparkle,
      blurb: l.blurb,
      edges: true,
      author: 'you' as const,
      load: (): Promise<LensModule> => import('./kit/runtime').then((m) => m.kitModule(l.spec)),
    }));
}

export function allLenses(): LensDef[] {
  return [...FIRST_PARTY, ...KIT_LENSES, ...yourLenses()];
}

export function lensById(id: string): LensDef | undefined {
  if (id === FOLDERS_LENS.id) return FOLDERS_LENS;
  return allLenses().find((l) => l.id === id);
}

/** Lenses that suit a space ("all" or a library kind). */
export function suits(def: LensDef, kind: LibraryKind | 'all'): boolean {
  const kinds = def.kinds ?? [...FILM_KINDS, 'all'];
  return (kinds as readonly string[]).includes(kind);
}

export const DEFAULT_PINS = ['wall', 'flow', 'shelf', 'gallery'];
