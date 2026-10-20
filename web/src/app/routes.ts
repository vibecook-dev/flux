/** Route table. Each screen is its own chunk, preloaded on link hover/focus. */
import { defineRoutes, preloadRoute } from './router';

/** Start loading the player's code (Play intent); records it so Play renders it without suspending. */
export const loadPlayer = () => preloadRoute('/play/0');

defineRoutes([
  { name: 'home', path: '/', load: () => import('../routes/Home') },
  { name: 'library', path: '/library/:id', load: () => import('../routes/Library') },
  { name: 'item', path: '/item/:id', load: () => import('../routes/Item') },
  { name: 'play', path: '/play/:id', load: () => import('../routes/Player'), bare: true },
  { name: 'connect', path: '/connect', load: () => import('../routes/Connect') },
  { name: 'admin', path: '/admin', load: () => import('../routes/Admin') },
]);
