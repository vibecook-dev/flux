/** Route table. Each overlay screen is its own chunk, preloaded on link hover/focus. */
import { defineRoutes, preloadRoute } from './router';

/** Start loading the player's code (Play intent); records it so Play renders it without suspending. */
export const loadPlayer = () => preloadRoute('/play/0');

/** Space routes draw nothing themselves: the shell's lens is the screen. */
const space = () => Promise.resolve({ default: () => null });

defineRoutes([
  { name: 'home', path: '/', layer: 'space', load: space },
  { name: 'library', path: '/library/:id', layer: 'space', load: space },
  { name: 'collection', path: '/collection/:id', layer: 'space', load: space },
  { name: 'item', path: '/item/:id', layer: 'overlay', load: () => import('../routes/Item') },
  { name: 'play', path: '/play/:id', load: () => import('../routes/Player'), bare: true },
  { name: 'connect', path: '/connect', layer: 'overlay', load: () => import('../routes/Connect') },
  { name: 'admin', path: '/admin', layer: 'overlay', load: () => import('../routes/Admin') },
  { name: 'changes', path: '/changes', layer: 'overlay', load: () => import('../routes/Changes') },
  { name: 'setup', path: '/setup', layer: 'overlay', load: () => import('../routes/Setup') },
  { name: 'own', path: '/own', layer: 'overlay', load: () => import('../routes/Own') },
]);
