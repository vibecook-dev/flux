/**
 * A videos library by folder (Plex's "Other Videos" browsing): breadcrumbs,
 * the folder's subfolders as tiles, then its videos in the virtualised grid,
 * by name or by date. Folders are links (Back goes up again); sort and filter
 * live in the query string like the library grid's.
 */
import { useDeferredValue, useEffect, useRef } from 'react';
import { Link } from '../app/Link';
import { setQuery } from '../app/router';
import { guardRefusal } from '../api/client';
import { subscribeEvents } from '../api/events';
import { refreshFolders, useFolderList } from '../api/folder-list';
import { breadcrumbs, FOLDER_SORT_LABELS, folderHref, parseFolderSort } from '../api/folders';
import { foldForSearch, formatCount } from '../api/format';
import { FOLDER_SORTS, type Library } from '../api/types';
import { PosterCard } from '../components/Cards';
import { Icon } from '../components/Icon';
import { VirtualGrid, type CellArgs, type VirtualGridHandle } from '../components/VirtualGrid';
import styles from './Library.module.css';

const plural = (n: number, one: string, many: string) => `${formatCount(n)} ${n === 1 ? one : many}`;

export function VideoFolders({ lib, query }: { lib: Library; query: URLSearchParams }) {
  const folder = query.get('folder') ?? '';
  const sort = parseFolderSort(query.get('fsort'));
  const text = query.get('q') ?? '';
  const deferredText = useDeferredValue(text);
  const list = useFolderList(lib.id, folder, sort);
  const grid = useRef<VirtualGridHandle>(null);

  // Live updates: a running scan's new videos appear without a reload.
  useEffect(() => {
    let timer: ReturnType<typeof setTimeout> | null = null;
    const unsub = subscribeEvents((e) => {
      if (e.type === 'items_changed' && e.library_id === lib.id && !timer) {
        timer = setTimeout(() => {
          timer = null;
          refreshFolders(lib.id);
        }, 1200);
      }
    });
    return () => {
      unsub();
      if (timer) clearTimeout(timer);
    };
  }, [lib.id]);

  const needle = foldForSearch(deferredText.trim());
  const folders = needle ? list.folders.filter((f) => foldForSearch(f.name).includes(needle)) : list.folders;
  const items = needle ? list.items.filter((i) => foldForSearch(i.title).includes(needle)) : list.items;
  const count = needle ? items.length : (list.total ?? list.items.length);
  const crumbs = breadcrumbs(list.parents, list.at);
  const atTop = folder === '';

  const renderCell = ({ index, width, tabIndex, deferImage, onKeyDown }: CellArgs) => {
    const item = items[index];
    if (!item) {
      return (
        <div className={styles.skeletonCell} aria-hidden="true">
          <span className="skeleton" />
          <span className="skeleton" />
        </div>
      );
    }
    return <PosterCard item={item} width={width} tabIndex={tabIndex} deferImage={deferImage} onKeyDown={onKeyDown} data-index={index} />;
  };

  const summary = atTop
    ? `${plural(lib.item_count, 'video', 'videos')}${lib.roots.length === 1 ? ` · ${lib.roots[0].path}` : ` · ${lib.roots.length} folders`}`
    : list.total !== null
      ? `${plural(list.total, 'video', 'videos')} here${list.folders.length ? ` · ${plural(list.folders.length, 'folder', 'folders')}` : ''}`
      : 'Loading';

  return (
    <div className={styles.page}>
      <header className={styles.header}>
        <h1 className="display-l">{atTop ? lib.name : (list.at?.name ?? ' ')}</h1>
        <p className="edge">{summary}</p>
      </header>

      {crumbs.length > 1 && (
        <nav className={styles.crumbs} aria-label="Folder">
          <ol>
            {crumbs.map((c) => (
              <li key={c.ref.folder}>
                {c.current ? (
                  <span aria-current="page">{c.ref.name}</span>
                ) : (
                  <Link to={folderHref(lib.id, c.ref.folder, sort)}>{c.ref.name}</Link>
                )}
              </li>
            ))}
          </ol>
        </nav>
      )}

      <div className={styles.toolbar}>
        <label className={styles.filter}>
          <Icon name="search" />
          <span className="visually-hidden">Filter this folder</span>
          <input type="search" value={text} placeholder="Filter this folder" onChange={(e) => setQuery({ q: e.target.value || null })} />
        </label>
        <div className={`${styles.chips} ${styles.viewChips}`} role="group" aria-label="View">
          <button type="button" className={styles.chip} aria-pressed="true">
            Folders
          </button>
          <button type="button" className={styles.chip} aria-pressed="false" onClick={() => setQuery({ view: 'all', folder: null, q: null })}>
            All videos
          </button>
        </div>
        <label className={styles.sort}>
          <span className="visually-hidden">Sort</span>
          <select className="select" value={sort} onChange={(e) => setQuery({ fsort: e.target.value === 'title' ? null : e.target.value })}>
            {FOLDER_SORTS.map((s) => (
              <option key={s} value={s}>
                {FOLDER_SORT_LABELS[s]}
              </option>
            ))}
          </select>
        </label>
      </div>

      {list.error !== null && list.items.length === 0 && list.folders.length === 0 ? (
        <div className={styles.empty}>
          <p>{guardRefusal(list.error) ?? 'Couldn’t load this folder. It may be gone, or the server didn’t answer.'}</p>
          <div className={styles.emptyActions}>
            <button type="button" className="btn" onClick={() => list.load()}>
              Try again
            </button>
            {!atTop && (
              <Link className="btn btn-ghost" to={folderHref(lib.id, '', sort)}>
                Back to {lib.name}
              </Link>
            )}
          </div>
        </div>
      ) : list.done && count === 0 && folders.length === 0 ? (
        <div className={styles.empty}>
          <p>{needle ? 'Nothing here matches.' : 'No videos here yet. Add files to this folder and flux will pick them up.'}</p>
          {needle && (
            <button type="button" className="btn" onClick={() => setQuery({ q: null })}>
              Clear filter
            </button>
          )}
        </div>
      ) : (
        <>
          {folders.length > 0 && (
            <ul className={styles.folders} aria-label="Folders">
              {folders.map((f) => (
                <li key={f.folder}>
                  <Link to={folderHref(lib.id, f.folder, sort)} className={styles.folderTile}>
                    <Icon name="folder" />
                    <span className={styles.folderName}>{f.name}</span>
                    <span className="edge">{plural(f.item_count, 'video', 'videos')}</span>
                  </Link>
                </li>
              ))}
            </ul>
          )}
          {count > 0 && (
            <div className={styles.gridWrap}>
              <VirtualGrid ref={grid} count={count} renderCell={renderCell} label={`${list.at?.name ?? lib.name} videos`} minCellWidth={150} stickyOffset={72} />
            </div>
          )}
        </>
      )}
    </div>
  );
}
