/**
 * Instant search (/, ⌘K). Each keystroke aborts the previous request; results
 * render as they arrive. Up/Down move through results, Enter opens one.
 * Built on <dialog> for the focus trap, Escape and inert background for free.
 */
import { useEffect, useId, useRef, useState } from 'react';
import { navigate, preloadRoute } from '../app/router';
import { api } from '../api/client';
import { prefetchItem, rememberSummaries } from '../api/queries';
import type { ItemSummary } from '../api/types';
import { Art } from './Art';
import { itemHref, resultEdge } from './card-utils';
import { Icon } from './Icon';
import styles from './SearchOverlay.module.css';

export function SearchOverlay({ open, onClose }: { open: boolean; onClose: () => void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  const input = useRef<HTMLInputElement>(null);
  const [q, setQ] = useState('');
  const [results, setResults] = useState<ItemSummary[]>([]);
  const [state, setState] = useState<'idle' | 'loading' | 'done' | 'error'>('idle');
  const [active, setActive] = useState(0);
  const listId = useId();

  useEffect(() => {
    const d = dialog.current;
    if (!d) return;
    if (open && !d.open) {
      d.showModal();
      input.current?.select();
      void preloadRoute('/item/0');
    } else if (!open && d.open) d.close();
  }, [open]);

  // Results only ever arrive asynchronously; an empty query derives "idle" below.
  useEffect(() => {
    const term = q.trim();
    if (!term) return;
    const ctl = new AbortController();
    // A 60 ms debounce keeps fast typists from sending a request per key.
    const t = setTimeout(() => {
      api
        .search(term, ctl.signal)
        .then((r) => {
          rememberSummaries(r);
          setResults(r);
          setActive(0);
          setState('done');
        })
        .catch(() => {
          if (!ctl.signal.aborted) setState('error');
        });
    }, 60);
    return () => {
      clearTimeout(t);
      ctl.abort();
    };
  }, [q]);

  const term = q.trim();
  const shown = term ? results : [];
  const status = term ? state : 'idle';

  const go = (item: ItemSummary) => {
    onClose();
    navigate(itemHref(item));
  };

  useEffect(() => {
    const item = results[active];
    if (item) prefetchItem(item);
  }, [active, results]);

  return (
    <dialog ref={dialog} className={styles.dialog} onClose={onClose} aria-label="Search the library" onClick={(e) => e.target === dialog.current && onClose()}>
      <div className={styles.panel}>
        <div className={styles.inputRow}>
          <Icon name="search" />
          <input
            ref={input}
            className={styles.input}
            type="search"
            placeholder="Search films and shows"
            value={q}
            onChange={(e) => {
              setQ(e.target.value);
              if (e.target.value.trim()) setState('loading');
            }}
            role="combobox"
            aria-expanded={shown.length > 0}
            aria-controls={listId}
            aria-activedescendant={shown[active] ? `${listId}-${active}` : undefined}
            aria-autocomplete="list"
            onKeyDown={(e) => {
              if (e.key === 'ArrowDown') {
                e.preventDefault();
                setActive((a) => Math.min(shown.length - 1, a + 1));
              } else if (e.key === 'ArrowUp') {
                e.preventDefault();
                setActive((a) => Math.max(0, a - 1));
              } else if (e.key === 'Enter' && shown[active]) {
                e.preventDefault();
                go(shown[active]);
              }
            }}
          />
          <button type="button" className={styles.close} onClick={onClose} aria-label="Close search">
            <kbd>esc</kbd>
          </button>
        </div>
        <ul id={listId} role="listbox" className={styles.results} aria-label="Results">
          {shown.map((item, i) => (
            <li
              key={item.id}
              id={`${listId}-${i}`}
              role="option"
              aria-selected={i === active}
              className={styles.result}
              onPointerEnter={() => setActive(i)}
              onClick={() => go(item)}
            >
              <Art art={item.poster} width={48} aspect="poster" title={item.title} showSourceLabel={false} className={styles.thumb} />
              <span className={styles.resultText}>
                <span className={styles.resultTitle}>{item.title}</span>
                <span className="edge">
                  {resultEdge(item)}
                </span>
              </span>
              {i === active && <Icon name="forward" className={styles.go} />}
            </li>
          ))}
        </ul>
        <p className={styles.status} role="status" aria-live="polite">
          {status === 'done' && shown.length === 0 && `Nothing in your libraries matches “${term}”.`}
          {status === 'error' && 'Search failed. Check that the server is reachable and try again.'}
          {status === 'idle' && 'Type a title. Accents and leading “The” don’t matter.'}
          {status === 'done' && shown.length > 0 && `${shown.length} result${shown.length === 1 ? '' : 's'}`}
        </p>
      </div>
    </dialog>
  );
}
