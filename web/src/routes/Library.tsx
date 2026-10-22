/**
 * Library grid: the whole library in one virtualised grid, filterable as you
 * type, sortable by the server, with an A–Z rail for title order.
 */
import { useDeferredValue, useEffect, useMemo, useRef } from 'react';
import { setQuery, type RouteProps } from '../app/router';
import { useTitle } from '../app/useTitle';
import { guardRefusal } from '../api/client';
import { subscribeEvents } from '../api/events';
import { foldForSearch, formatAgo, formatCount, titleSortKey } from '../api/format';
import { refreshLibrary, useLibraryList } from '../api/library-list';
import { useLibraries } from '../api/queries';
import { LIBRARY_SORTS, type Id, type ItemSummary, type Library as LibraryInfo, type LibrarySort } from '../api/types';
import { PosterCard } from '../components/Cards';
import { Icon } from '../components/Icon';
import { VirtualGrid, type CellArgs, type VirtualGridHandle } from '../components/VirtualGrid';
import styles from './Library.module.css';
import { VideoFolders } from './VideoFolders';

const SORT_LABELS: Record<LibrarySort, string> = {
  title: 'Title A–Z',
  '-title': 'Title Z–A',
  year: 'Oldest first',
  '-year': 'Newest first',
  added: 'First added',
  '-added': 'Recently added',
};

type Filter = 'unwatched' | 'inprogress' | '4k' | 'hdr';
const FILTERS: [Filter, string, (i: ItemSummary) => boolean][] = [
  ['unwatched', 'Unwatched', (i) => !i.progress?.watched],
  ['inprogress', 'In progress', (i) => Boolean(i.progress && !i.progress.watched && i.progress.position_ms > 0)],
  ['4k', '4K', (i) => i.badges.includes('4K')],
  ['hdr', 'HDR', (i) => i.badges.some((b) => b === 'HDR10' || b === 'HDR10+' || b === 'DV' || b === 'HLG')],
];

const LETTERS = '#ABCDEFGHIJKLMNOPQRSTUVWXYZ'.split('');

function letterOf(title: string): string {
  const c = titleSortKey(title).charAt(0).toUpperCase();
  return c >= 'A' && c <= 'Z' ? c : '#';
}

function SkeletonCell() {
  return (
    <div className={styles.skeletonCell} aria-hidden="true">
      <span className="skeleton" />
      <span className="skeleton" />
    </div>
  );
}

export default function Library({ params, query }: RouteProps) {
  const id = Number(params.id);
  const libs = useLibraries();
  const lib = libs.data?.find((l) => l.id === id);
  useTitle(lib?.name ?? 'Library');
  // A videos library opens by folder (the whole list is a click away); its
  // kind is known once the library list is in.
  if (lib?.kind === 'videos' && query.get('view') !== 'all') return <VideoFolders lib={lib} query={query} />;
  if (libs.data === undefined && libs.status !== 'error') return <div className={styles.page} aria-busy="true" />;
  return <LibraryGrid id={id} lib={lib} query={query} />;
}

function LibraryGrid({ id, lib, query }: { id: Id; lib: LibraryInfo | undefined; query: URLSearchParams }) {
  const sortParam = query.get('sort') ?? 'title';
  const sort: LibrarySort = (LIBRARY_SORTS as readonly string[]).includes(sortParam) ? (sortParam as LibrarySort) : 'title';
  const text = query.get('q') ?? '';
  const deferredText = useDeferredValue(text);
  const active = new Set((query.get('f') ?? '').split(',').filter(Boolean) as Filter[]);
  const activeKey = [...active].sort().join(',');

  const list = useLibraryList(id, sort);
  const grid = useRef<VirtualGridHandle>(null);

  // Live updates: new items from a running scan appear without a reload.
  useEffect(() => {
    let timer: ReturnType<typeof setTimeout> | null = null;
    const unsub = subscribeEvents((e) => {
      if (e.type === 'items_changed' && e.library_id === id && !timer) {
        timer = setTimeout(() => {
          timer = null;
          refreshLibrary(id);
        }, 1200);
      }
    });
    return () => {
      unsub();
      if (timer) clearTimeout(timer);
    };
  }, [id]);

  const filtering = deferredText.trim() !== '' || active.size > 0;
  const visible = useMemo(() => {
    if (!filtering) return list.items;
    const needle = foldForSearch(deferredText.trim());
    const preds = FILTERS.filter(([f]) => activeKey.split(',').includes(f)).map(([, , p]) => p);
    return list.items.filter((it) => (!needle || foldForSearch(it.title).includes(needle)) && preds.every((p) => p(it)));
    // list.version captures mutation of list.items
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [list.version, list.items, deferredText, activeKey, filtering]);

  const count = filtering ? visible.length : (list.total ?? list.items.length);
  const letters = useMemo(() => {
    const firstIndex = new Map<string, number>();
    if (sort !== 'title' || filtering) return firstIndex;
    visible.forEach((it, i) => {
      const l = letterOf(it.title);
      if (!firstIndex.has(l)) firstIndex.set(l, i);
    });
    return firstIndex;
  }, [visible, sort, filtering]);

  const renderCell = ({ index, width, tabIndex, deferImage, onKeyDown }: CellArgs) => {
    const item = visible[index];
    if (!item) return <SkeletonCell />;
    return <PosterCard item={item} width={width} tabIndex={tabIndex} deferImage={deferImage} onKeyDown={onKeyDown} data-index={index} />;
  };

  const toggle = (f: Filter) => {
    const next = new Set(active);
    if (next.has(f)) next.delete(f);
    else next.add(f);
    setQuery({ f: [...next].join(',') || null });
  };

  const noun = lib?.kind === 'shows' ? 'shows' : lib?.kind === 'mixed' ? 'titles' : lib?.kind === 'videos' ? 'videos' : 'films';
  // Roots say for themselves whether they answer (RootStatus), no scan-state parsing.
  const offlineRoots = lib?.roots.filter((r) => !r.available) ?? [];

  return (
    <div className={styles.page}>
      <header className={styles.header}>
        <h1 className="display-l">{lib?.name ?? ' '}</h1>
        <p className="edge">
          {list.total !== null ? `${formatCount(list.total)} ${noun}` : 'Loading'}
          {!list.done && list.total !== null && list.items.length < list.total && ` · loaded ${formatCount(list.items.length)}`}
          {lib && ` · ${lib.roots.length === 1 ? lib.roots[0].path : `${lib.roots.length} folders`}`}
        </p>
      </header>

      {offlineRoots.length > 0 && (
        <p className={styles.notice} role="note">
          <Icon name="warning" />
          <span>
            {offlineRoots.map((r) => r.path).join(', ')} {offlineRoots.length === 1 ? 'is' : 'are'} not responding
            {offlineRoots[0].last_seen ? ` (last seen ${formatAgo(offlineRoots[0].last_seen)})` : ''}. Its items stay here and play again when it’s back.
          </span>
        </p>
      )}

      <div className={styles.toolbar}>
        <label className={styles.filter}>
          <Icon name="search" />
          <span className="visually-hidden">Filter this library</span>
          <input
            type="search"
            value={text}
            placeholder={`Filter ${list.total !== null ? formatCount(list.total) : ''} ${noun}`}
            onChange={(e) => setQuery({ q: e.target.value || null })}
          />
        </label>
        {lib?.kind === 'videos' && (
          <div className={`${styles.chips} ${styles.viewChips}`} role="group" aria-label="View">
            <button type="button" className={styles.chip} aria-pressed="false" onClick={() => setQuery({ view: null, q: null, f: null })}>
              Folders
            </button>
            <button type="button" className={styles.chip} aria-pressed="true">
              All videos
            </button>
          </div>
        )}
        <div className={styles.chips} role="group" aria-label="Filters">
          {FILTERS.map(([f, label]) => (
            <button key={f} type="button" className={styles.chip} aria-pressed={active.has(f)} onClick={() => toggle(f)}>
              {label}
            </button>
          ))}
        </div>
        <label className={styles.sort}>
          <span className="visually-hidden">Sort</span>
          <select className="select" value={sort} onChange={(e) => setQuery({ sort: e.target.value === 'title' ? null : e.target.value })}>
            {LIBRARY_SORTS.map((s) => (
              <option key={s} value={s}>
                {SORT_LABELS[s]}
              </option>
            ))}
          </select>
        </label>
      </div>

      {filtering && (
        <p className={`edge ${styles.resultCount}`} role="status">
          {formatCount(visible.length)} of {formatCount(list.items.length)}
          {!list.done ? ' loaded so far' : ''}
        </p>
      )}

      {list.error !== null && list.items.length === 0 ? (
        <div className={styles.empty}>
          <p>{guardRefusal(list.error) ?? 'Couldn’t load this library. The server didn’t answer.'}</p>
          <button type="button" className="btn" onClick={() => list.load()}>
            Try again
          </button>
        </div>
      ) : count === 0 && (list.done || filtering) ? (
        <div className={styles.empty}>
          <p>{filtering ? 'Nothing matches these filters.' : 'This library is empty. Add files to its folders and flux will pick them up.'}</p>
          {filtering && (
            <button type="button" className="btn" onClick={() => setQuery({ q: null, f: null })}>
              Clear filters
            </button>
          )}
        </div>
      ) : (
        <div className={styles.gridWrap}>
          <VirtualGrid ref={grid} count={count} renderCell={renderCell} label={`${lib?.name ?? 'Library'} ${noun}`} minCellWidth={150} stickyOffset={72} />
          {letters.size > 1 && (
            <nav className={styles.rail} aria-label="Jump to letter">
              {LETTERS.map((l) => {
                const at = letters.get(l);
                return (
                  <button key={l} type="button" disabled={at === undefined} onClick={() => at !== undefined && grid.current?.scrollToIndex(at)} aria-label={`Jump to ${l === '#' ? 'numbers' : l}`}>
                    {l}
                  </button>
                );
              })}
            </nav>
          )}
        </div>
      )}
    </div>
  );
}
