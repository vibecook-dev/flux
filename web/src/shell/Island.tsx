/**
 * The island: one glass shape at the top that is where you are, the search field,
 * the space switcher and the place live events appear. It changes shape instead
 * of opening pages. Typing anywhere opens it with your first letter.
 */
import { type RefObject, useEffect, useId, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { api } from '../api/client';
import { formatCount, formatRemaining } from '../api/format';
import { prefetchItem, rememberSummaries, useHome } from '../api/queries';
import type { SpaceId } from '../api/space-list';
import type { ItemSummary, Library } from '../api/types';
import { Art } from '../components/Art';
import { resultEdge } from '../components/card-utils';
import { Icon } from '../components/Icon';
import { SHOW_LABELS, SHOWS, SORT_LABELS, SORTS, type Show, type Sort } from '../lens/sections';
import { refreshGlass } from './glass-light';
import styles from './Island.module.css';
import { dismissActivity, setIslandReady, useLive, type Activity, type LiveRing } from './live';

export type IslandMode = 'rest' | 'open' | 'activity';

export interface IslandProps {
  spaceId: SpaceId;
  spaceName: string;
  /** Titles on the wall now (after search and filter). */
  count: number | null;
  text: string;
  show: Show;
  sort: Sort;
  libraries: readonly Library[] | undefined;
  open: boolean;
  /** Text to start with when opened by typing. */
  seed: string | null;
  /** Something covers the island (player, lens gallery): hold live events. */
  covered: boolean;
  onOpenChange: (open: boolean) => void;
  onText: (text: string) => void;
  onShow: (show: Show) => void;
  onSort: (sort: Sort) => void;
  onSpace: (id: SpaceId) => void;
  onOpenItem: (item: ItemSummary) => void;
  onResume: (item: ItemSummary) => void;
}

const MARK = (
  <svg className={styles.mark} viewBox="0 0 35 28" aria-hidden="true">
    <path d="M2 26 L20 2 L27 2 L9 26 Z" fill="currentColor" />
    <path d="M16 26 L28 10 L33 10 L21 26 Z" fill="currentColor" fillOpacity=".45" />
  </svg>
);

const CIRC = 2 * Math.PI * 9;

function offline(l: Library): boolean {
  return l.scan.state === 'unavailable' || l.roots.some((r) => !r.available);
}

export function Island(props: IslandProps) {
  const { open, covered } = props;
  const node = useRef<HTMLDivElement>(null);
  const restRef = useRef<HTMLDivElement>(null);
  const openRef = useRef<HTMLDivElement>(null);
  const actRef = useRef<HTMLDivElement>(null);
  const input = useRef<HTMLInputElement>(null);
  const live = useLive();
  const activity = live.current;
  const mode: IslandMode = open ? 'open' : activity ? 'activity' : 'rest';
  const listId = useId();

  // ── live events: one at a time, only when nothing covers the island ──
  useEffect(() => {
    setIslandReady(!open && !covered);
  }, [open, covered]);
  useEffect(() => () => setIslandReady(false), []);

  // Closing with Esc (or the scrim) puts keyboard focus back on the island, not nowhere.
  const wasOpen = useRef(open);
  useEffect(() => {
    if (wasOpen.current && !open) {
      const a = document.activeElement;
      if (!a || a === document.body) restRef.current?.querySelector<HTMLElement>('button')?.focus({ preventScroll: true });
    }
    wasOpen.current = open;
  }, [open]);

  useEffect(() => {
    if (!activity) return;
    const el = node.current;
    let hover = false;
    const enter = () => (hover = true);
    const leave = () => (hover = false);
    el?.addEventListener('pointerenter', enter);
    el?.addEventListener('pointerleave', leave);
    let left = activity.ms ?? 5200;
    const t = setInterval(() => {
      if (!hover) left -= 100;
      if (left <= 0) dismissActivity();
    }, 100);
    return () => {
      clearInterval(t);
      el?.removeEventListener('pointerenter', enter);
      el?.removeEventListener('pointerleave', leave);
    };
  }, [activity]);

  // ── shape: the island takes the size of whichever layer is showing ──
  useLayoutEffect(() => {
    const el = node.current;
    const layer = mode === 'open' ? openRef.current : mode === 'activity' ? actRef.current : restRef.current;
    if (!el || !layer) return;
    const fit = () => {
      const panelW = Math.min(window.innerWidth <= 640 ? window.innerWidth - 32 : 620, window.innerWidth - 32);
      el.style.setProperty('--panel-w', `${panelW}px`);
      const w = mode === 'open' ? panelW : layer.offsetWidth;
      el.style.setProperty('--iw', `${w}px`);
      el.style.setProperty('--ih', `${layer.offsetHeight}px`);
      el.style.setProperty('--ir', mode === 'rest' ? '22px' : `${Math.min(30, layer.offsetHeight / 2)}px`);
      document.documentElement.dataset.island = mode;
      refreshGlass();
    };
    fit();
    const ro = new ResizeObserver(fit);
    ro.observe(layer);
    window.addEventListener('resize', fit);
    return () => {
      ro.disconnect();
      window.removeEventListener('resize', fit);
    };
  });

  return (
    <div ref={node} className={`${styles.island} glass`} data-mode={mode}>
      <div className={styles.clip}>
        <div ref={restRef} className={`${styles.layer} ${styles.rest}`} data-on={mode === 'rest' || undefined} aria-hidden={mode !== 'rest'}>
          <Rest {...props} ring={live.ring} />
        </div>
        <div ref={openRef} className={`${styles.layer} ${styles.open}`} data-on={mode === 'open' || undefined} aria-hidden={mode !== 'open'}>
          {open && <OpenPanel {...props} input={input} listId={listId} />}
        </div>
        <div ref={actRef} className={`${styles.layer} ${styles.act}`} data-on={mode === 'activity' || undefined} role="status" aria-live="polite">
          {activity && <ActivityView a={activity} onDone={dismissActivity} />}
        </div>
      </div>
    </div>
  );
}

function Rest({ spaceName, count, text, onOpenChange, onText, ring }: IslandProps & { ring: LiveRing | null }) {
  return (
    <div className={styles.restRow}>
      <button type="button" className={styles.where} onClick={() => onOpenChange(true)} aria-haspopup="dialog" title="Spaces and search (/)">
        {MARK}
        <span className={styles.space}>{spaceName}</span>
        {count !== null && <span className={`${styles.count} mono`}>{formatCount(count)}</span>}
      </button>
      {text && (
        <span className={styles.query}>
          <span>“{text}”</span>
          <button type="button" aria-label="Clear search" onClick={() => onText('')}>
            <Icon name="close" />
          </button>
        </span>
      )}
      {ring && (
        <button type="button" className={styles.live} onClick={ring.run} aria-label={ring.aria} title={ring.aria}>
          <svg className={styles.ring} viewBox="0 0 24 24" aria-hidden="true" data-spin={ring.progress === null || undefined}>
            <circle className={styles.track} cx="12" cy="12" r="9" />
            <circle className={styles.val} cx="12" cy="12" r="9" strokeDasharray={CIRC.toFixed(2)} strokeDashoffset={(CIRC * (1 - (ring.progress ?? 0.25))).toFixed(2)} />
            {ring.kind === 'session' && <path className={styles.glyph} d="M10 8.6v6.8l5.4-3.4z" />}
          </svg>
          <span className="mono">{ring.label}</span>
        </button>
      )}
      <button type="button" className={styles.find} onClick={() => onOpenChange(true)} aria-label="Search" title="Search (type anywhere)">
        <Icon name="search" />
      </button>
    </div>
  );
}

function OpenPanel(props: IslandProps & { input: RefObject<HTMLInputElement | null>; listId: string }) {
  const { text, seed, onText, onOpenChange, input, listId, onOpenItem } = props;
  const [value, setValue] = useState(seed ?? text);
  const [results, setResults] = useState<ItemSummary[]>([]);
  const [state, setState] = useState<'idle' | 'loading' | 'done' | 'error'>('idle');
  const [sel, setSel] = useState(0);

  // Focus the field the moment it exists, so fast typing after the first letter lands in it.
  useLayoutEffect(() => {
    const el = input.current;
    if (!el) return;
    el.focus({ preventScroll: true });
    el.setSelectionRange(el.value.length, el.value.length);
  }, [input]);
  useEffect(() => {
    if (seed !== null && seed !== text) onText(seed);
    // Only when the panel opens.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const term = value.trim();
  useEffect(() => {
    if (!term) return;
    const ctl = new AbortController();
    const t = setTimeout(() => {
      setState('loading');
      api
        .search(term, ctl.signal)
        .then((r) => {
          rememberSummaries(r);
          setResults(r);
          setSel(0);
          setState('done');
        })
        .catch(() => {
          if (!ctl.signal.aborted) setState('error');
        });
    }, 80);
    return () => {
      clearTimeout(t);
      ctl.abort();
    };
  }, [term]);

  const shown = useMemo(() => (term ? results.slice(0, 6) : []), [term, results]);
  useEffect(() => {
    const it = shown[sel];
    if (it) prefetchItem(it);
  }, [sel, shown]);

  return (
    <>
      <div className={styles.search}>
        <Icon name="search" />
        <input
          ref={input}
          id="island-search"
          type="search"
          value={value}
          placeholder="Search titles, shows, people"
          autoComplete="off"
          spellCheck={false}
          aria-label="Search your libraries"
          role="combobox"
          aria-expanded={shown.length > 0}
          aria-controls={listId}
          aria-activedescendant={shown[sel] ? `${listId}-${sel}` : undefined}
          onChange={(e) => {
            setValue(e.target.value);
            onText(e.target.value);
          }}
          onKeyDown={(e) => {
            if (e.key === 'ArrowDown' && shown.length) {
              e.preventDefault();
              setSel((s) => (s + 1) % shown.length);
            } else if (e.key === 'ArrowUp' && shown.length) {
              e.preventDefault();
              setSel((s) => (s - 1 + shown.length) % shown.length);
            } else if (e.key === 'Enter') {
              e.preventDefault();
              const it = shown[sel];
              if (it) onOpenItem(it);
              else if (term) onOpenChange(false);
            } else if (e.key === 'Escape') {
              e.preventDefault();
              e.stopPropagation();
              if (value) {
                setValue('');
                onText('');
              } else onOpenChange(false);
            }
          }}
        />
        <kbd className="mono">esc</kbd>
      </div>
      <div className={styles.scroll}>
        {term ? (
          <Results {...props} term={term} shown={shown} state={state} sel={sel} setSel={setSel} listId={listId} />
        ) : (
          <Home {...props} />
        )}
      </div>
    </>
  );
}

function Results({ term, shown, state, sel, setSel, listId, count, spaceName, onOpenItem }: IslandProps & { term: string; shown: ItemSummary[]; state: string; sel: number; setSel: (n: number) => void; listId: string }) {
  return (
    <div className={styles.results}>
      <p className={styles.hint}>
        <span>
          {count !== null ? `${formatCount(count)} on the wall · ${spaceName}` : spaceName}
        </span>
        <span className="mono">↑↓ ↵</span>
      </p>
      <ul id={listId} role="listbox" aria-label="Results">
        {shown.map((it, i) => (
          <li key={it.id} id={`${listId}-${i}`} role="option" aria-selected={i === sel}>
            <button type="button" className={styles.res} data-sel={i === sel || undefined} onPointerEnter={() => setSel(i)} onClick={() => onOpenItem(it)}>
              <Art art={it.poster} width={34} aspect={it.kind === 'episode' ? 'wide' : 'poster'} title={it.title} showSourceLabel={false} className={styles.resArt} />
              <span className={styles.resText}>
                <b>{it.title}</b>
                <small className="mono">{resultEdge(it)}</small>
              </span>
              <span className={`${styles.ret} mono`} aria-hidden="true">
                ↵
              </span>
            </button>
          </li>
        ))}
      </ul>
      {state === 'done' && shown.length === 0 && <p className={styles.empty}>Nothing in your libraries matches “{term}”. Accents and a leading “The” don’t matter.</p>}
      {state === 'error' && <p className={styles.empty}>Search didn’t answer. Check that the server is reachable.</p>}
    </div>
  );
}

function Chip({ on, label, n, onClick, dot }: { on: boolean; label: string; n?: string; onClick: () => void; dot?: boolean }) {
  return (
    <button type="button" className={styles.chip} aria-pressed={on} onClick={onClick}>
      {dot && <span className={styles.chipDot} aria-label="Partly offline" />}
      {label}
      {n && <span className={`${styles.n} mono`}>{n}</span>}
    </button>
  );
}

function Home({ spaceId, libraries, show, sort, onSpace, onShow, onSort, onResume }: IslandProps) {
  const home = useHome();
  const conts = (home.data?.continue_watching ?? []).slice(0, 4);
  const total = (libraries ?? []).filter((l) => l.kind !== 'videos').reduce((n, l) => n + l.item_count, 0);
  return (
    <>
      {conts.length > 0 && (
        <section className={styles.sec}>
          <h3>Continue</h3>
          <div className={styles.conts}>
            {conts.map((it) => {
              const p = it.progress;
              const frac = p && p.duration_ms ? p.position_ms / p.duration_ms : 0;
              return (
                <button key={it.id} type="button" className={styles.cont} onClick={() => onResume(it)} onPointerEnter={() => prefetchItem(it)}>
                  <Art art={it.kind === 'episode' ? it.poster : (it.poster ?? it.backdrop)} width={40} aspect={it.kind === 'episode' ? 'wide' : 'poster'} title={it.title} showSourceLabel={false} className={it.kind === 'episode' ? styles.contWide : styles.contArt} />
                  <span className={styles.contText}>
                    <b>{it.kind === 'episode' && it.parent_title ? it.parent_title : it.title}</b>
                    <small>{p ? formatRemaining(p.position_ms, p.duration_ms) : ''}</small>
                    <span className={styles.bar}>
                      <i style={{ width: `${Math.round(frac * 100)}%` }} />
                    </span>
                  </span>
                </button>
              );
            })}
          </div>
        </section>
      )}
      <section className={styles.sec}>
        <h3>Spaces</h3>
        <div className={styles.chips}>
          <Chip on={spaceId === 'all'} label="All" n={libraries ? formatCount(total) : undefined} onClick={() => onSpace('all')} />
          {(libraries ?? []).map((l) => (
            <Chip key={l.id} on={spaceId === l.id} label={l.name} n={formatCount(l.item_count)} dot={offline(l)} onClick={() => onSpace(l.id)} />
          ))}
        </div>
      </section>
      <section className={styles.sec}>
        <h3>Show</h3>
        <div className={styles.chips}>
          {SHOWS.map((s) => (
            <Chip key={s} on={show === s} label={SHOW_LABELS[s]} onClick={() => onShow(s)} />
          ))}
        </div>
      </section>
      <section className={styles.sec}>
        <h3>Sort</h3>
        <div className={styles.chips}>
          {SORTS.map((s) => (
            <Chip key={s} on={sort === s} label={SORT_LABELS[s]} onClick={() => onSort(s)} />
          ))}
        </div>
      </section>
    </>
  );
}

function ActivityView({ a, onDone }: { a: Activity; onDone: () => void }) {
  const thumbs = (a.thumbs ?? []).filter((t) => t !== undefined).slice(0, 3);
  return (
    <div className={styles.actRow}>
      {thumbs.length > 0 ? (
        <span className={styles.thumbs}>
          {thumbs.map((t, i) => (
            <Art key={i} art={t} width={34} aspect="poster" title="" showSourceLabel={false} className={styles.thumb} />
          ))}
        </span>
      ) : (
        <span className={styles.glyphTile}>
          <Icon name={a.icon ?? 'sparkle'} />
        </span>
      )}
      <span className={styles.actText}>
        <b>{a.title}</b>
        {a.detail && <small>{a.detail}</small>}
      </span>
      {a.action && (
        <button
          type="button"
          className={styles.actBtn}
          onClick={() => {
            a.action?.run();
            onDone();
          }}
        >
          {a.action.label}
        </button>
      )}
    </div>
  );
}
