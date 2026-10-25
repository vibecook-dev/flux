/**
 * The shell: the library fills the window (drawn by the current lens) and the
 * controls float over it as glass: the island at the top (where you are, search,
 * spaces, live events), the lens bar at the bottom (how you see it) and one
 * corner button (who, and the server). Details and settings open over the
 * library, which stays mounted underneath, so closing them puts you back exactly
 * where you were.
 */
import { lazy, type ReactNode, Suspense, useCallback, useDeferredValue, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { registerAmbientElement, setAmbientFromHash } from '../app/ambient';
import { Link } from '../app/Link';
import { back, matchRoute, navigateWhenLoaded, preloadRoute, type Location } from '../app/router';
import { useTheme } from '../app/theme';
import { guardRefusal } from '../api/client';
import { holdPaging } from '../api/library-list';
import { prefetchItem, useLibraries, useServer, useServerLenses } from '../api/queries';
import { useSpaceList } from '../api/space-list';
import type { Id, ItemSummary, LibraryKind } from '../api/types';
import { LensHost } from '../lens/LensHost';
import { activeLens, lensFocus, setLensFocus } from '../lens/bridge';
import { prefs } from '../lens/dom';
import { rememberOrigin } from '../lens/flight';
import { allLenses, DEFAULT_PINS, FOLDERS_LENS, lensById, setServerLenses, suits } from '../lens/registry';
import { sectionOf, serverSort, visibleItems } from '../lens/sections';
import type { Dir, LensDef } from '../lens/types';
import { preloadPlayer, startPlay } from '../player/start';
import { Corner } from './Corner';
import { useEventsBridge } from './events-bridge';
import { startGlassLight } from './glass-light';
import { Island } from './Island';
import { LensBar } from './LensBar';
import { postActivity } from './live';
import { Sheet } from './Sheet';
import styles from './Shell.module.css';
import { goToSpace, setSpaceQuery, spaceFor, spaceKey } from './space';
import { useInsets, usePhone } from './viewport';

const SHEETS: Record<string, string> = { admin: 'Libraries & scanning', connect: 'Remote access', changes: 'Agents & changes' };

const toldOffline = new Set<string>();

// Loaded when needed: folders only for videos libraries, the gallery when you open it.
const VideoFolders = lazy(() => import('../routes/VideoFolders').then((m) => ({ default: m.VideoFolders })));
const LensGallery = lazy(() => import('./LensGallery').then((m) => ({ default: m.LensGallery })));

export function Shell({ loc }: { loc: Location }) {
  const m = matchRoute(loc.pathname);
  const overlay = m && m.route.layer === 'overlay' ? m : null;
  const space = spaceFor(loc.pathname, loc.search);
  const libs = useLibraries();
  const lib = space.id === 'all' ? undefined : libs.data?.find((l) => l.id === space.id);
  const kind: LibraryKind | 'all' = space.id === 'all' ? 'all' : (lib?.kind ?? 'movies');
  const phone = usePhone();
  const insets = useInsets();
  const { resolved: theme } = useTheme();
  // Overlays set their own title; the space sets it otherwise.
  const libName = lib?.name;
  useEffect(() => {
    if (!overlay) document.title = space.id === 'all' || !libName ? 'flux' : `${libName} · flux`;
  }, [overlay, space.id, libName]);
  useEventsBridge();
  useEffect(() => startGlassLight(), []);

  // ── results ──
  const list = useSpaceList(space.id, serverSort(space.sort), libs.data);
  const text = useDeferredValue(space.text);
  const visible = useMemo(
    () => visibleItems(list.items, { show: space.show, text, sort: space.sort }),
    // list.version tracks list.items
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [list.version, list.items, space.show, text, space.sort],
  );
  const filtering = space.show !== 'all' || text.trim() !== '';
  const total = filtering ? visible.length : Math.max(list.total ?? list.items.length, list.items.length);

  // ── lens choice: per space, remembered on this device ──
  const [choice, setChoice] = useState<Record<string, string>>(() => prefs.get('lens.choice', {}));
  const [pins, setPins] = useState<string[]>(() => prefs.get('lens.pins', DEFAULT_PINS));
  // Bumped when you add or remove a lens of your own.
  const [lensVersion, setLensVersion] = useState(0);
  // Lenses kept on the server (an agent's `lens.add`): every device has them.
  const server = useServer();
  const serverLensList = useServerLenses(Boolean(server.data?.you)).data?.lenses;
  const lenses = useMemo(
    () => {
      // The registry keeps the server's list, so lensById finds them too.
      setServerLenses(serverLensList ?? []);
      return [...(kind === 'videos' ? [FOLDERS_LENS] : []), ...allLenses().filter((d) => suits(d, kind) || (d.id === 'wall' && kind === 'videos'))];
    },
    // lensVersion: your lenses live in prefs, read again when they change.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [kind, lensVersion, serverLensList],
  );
  const key = spaceKey(space.id);
  const fallback = kind === 'videos' ? 'folders' : 'wall';
  const lensId = lenses.some((l) => l.id === choice[key]) ? choice[key] : fallback;
  const def = useMemo<LensDef>(() => lensById(lensId) ?? lenses[0], [lensId, lenses]);
  const barLenses = useMemo(() => {
    const pinned = lenses.filter((l) => l.id === 'folders' || pins.includes(l.id));
    return pinned.some((l) => l.id === lensId) ? pinned : [...pinned, ...lenses.filter((l) => l.id === lensId)];
  }, [lenses, pins, lensId]);
  const pickLens = useCallback(
    (id: string) => {
      setChoice((c) => {
        const next = { ...c, [key]: id };
        prefs.set('lens.choice', next);
        return next;
      });
    },
    [key],
  );
  const stepLens = (d: 1 | -1) => {
    const i = barLenses.findIndex((l) => l.id === lensId);
    const next = barLenses[(i + d + barLenses.length) % barLenses.length];
    if (next) pickLens(next.id);
  };
  const pin = (id: string, on: boolean) => {
    setPins((p) => {
      const next = on ? [...p.filter((x) => x !== id), id].slice(-6) : p.filter((x) => x !== id);
      prefs.set('lens.pins', next);
      return next;
    });
  };

  // ── chrome state ──
  const [islandOpen, setIslandOpen] = useState(false);
  const [seed, setSeed] = useState<string | null>(null);
  // The lens bar shrinks while you scroll down (a new lens starts with it full: LensHost says so).
  const [mini, setMini] = useState(false);
  const [accessory, setAccessory] = useState<HTMLElement | null>(null);
  const [gallery, setGallery] = useState(false);
  const [failure, setFailure] = useState<{ id: string; name: string } | null>(null);
  const covered = Boolean(overlay) || gallery;

  useEffect(() => {
    document.documentElement.dataset.overlay = overlay ? overlay.route.name : gallery ? 'gallery' : '';
    return () => {
      document.documentElement.dataset.overlay = '';
    };
  }, [overlay, gallery]);

  // While a page covers the library (or the player replaces the shell), its remaining pages wait.
  const hidden = Boolean(overlay);
  useEffect(() => {
    holdPaging(hidden);
    return () => holdPaging(true);
  }, [hidden]);

  const openIsland = (s: string | null = null) => {
    setSeed(s);
    setIslandOpen(true);
  };

  // ── what the lens can ask for ──
  const onFocus = useCallback(
    (id: Id) => {
      setLensFocus(id);
      const it = visible.find((i) => i.id === id);
      if (it) setAmbientFromHash(it.poster?.blurhash ?? it.backdrop?.blurhash);
    },
    [visible],
  );
  const onOpen = useCallback((id: Id, art: Element | null) => {
    rememberOrigin(id, art?.getBoundingClientRect() ?? activeLens()?.view.rectOf(id));
    setIslandOpen(false);
    navigateWhenLoaded(`/item/${id}`);
  }, []);
  const onIntent = useCallback((item: ItemSummary) => {
    prefetchItem(item);
    void preloadRoute('/item/0');
  }, []);
  const onScrolled = useCallback((dir: 'down' | 'up' | 'top') => setMini(dir === 'down'), []);
  const sectionFor = useCallback((item: ItemSummary) => sectionOf(item, space.sort), [space.sort]);
  const onError = useCallback((d: LensDef, err: unknown) => {
    console.error(`lens ${d.id} failed`, err);
    setFailure({ id: d.id, name: d.name });
  }, []);
  const failed = failure?.id === def.id ? failure.name : null;

  const closeOverlay = useCallback(() => back(space.href), [space.href]);

  // ── keyboard: type anywhere to search, arrows move in the lens, Esc steps back ──
  const keyState = useRef({ overlay, islandOpen, gallery, text: space.text });
  const stepRef = useRef(stepLens);
  useEffect(() => {
    keyState.current = { overlay, islandOpen, gallery, text: space.text };
    stepRef.current = stepLens;
  });
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const s = keyState.current;
      const t = e.target as HTMLElement | null;
      const typing = t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.tagName === 'SELECT' || t.isContentEditable);
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') {
        e.preventDefault();
        setIslandOpen((o) => !o);
        return;
      }
      if (e.key === 'Escape') {
        if (e.defaultPrevented) return;
        if (s.gallery) return setGallery(false);
        if (s.overlay) {
          // Dialogs inside overlays (selects, menus) handle their own Escape first.
          if (t?.closest('dialog, [role="menu"]')) return;
          e.preventDefault();
          return closeOverlay();
        }
        if (s.islandOpen) return setIslandOpen(false);
        if (s.text) return setSpaceQuery({ q: null });
        return;
      }
      // The island is open but its field lost focus (you clicked a chip): letters go back to the field.
      if (s.islandOpen && !typing && !s.overlay && !s.gallery && e.key.length === 1 && /\S/.test(e.key) && !e.metaKey && !e.ctrlKey && !e.altKey) {
        const field = document.getElementById('island-search');
        if (field instanceof HTMLInputElement) {
          field.focus({ preventScroll: true });
          field.setSelectionRange(field.value.length, field.value.length);
        }
        return;
      }
      if (typing || s.overlay || s.gallery || s.islandOpen || e.altKey) return;
      if (e.key === '/') {
        e.preventDefault();
        openIsland();
        return;
      }
      if (e.key === '[' || e.key === ']') {
        stepRef.current(e.key === ']' ? 1 : -1);
        return;
      }
      if (e.key === '\\') {
        setGallery(true);
        return;
      }
      const dir: Dir | undefined = ({ ArrowLeft: 'left', ArrowRight: 'right', ArrowUp: 'up', ArrowDown: 'down' } as const)[e.key as 'ArrowLeft'];
      const view = activeLens()?.view;
      if (dir && view) {
        if (view.move(dir)) e.preventDefault();
        return;
      }
      if (view?.key?.(e)) {
        e.preventDefault();
        return;
      }
      if (e.key === 'Enter') {
        const id = lensFocus();
        if (id !== null) onOpen(id, activeLens()?.view.artOf?.(id) ?? null);
        return;
      }
      // Printable characters start a search: the lens filters as you type.
      if (e.key.length === 1 && !e.metaKey && !e.ctrlKey && /\S/.test(e.key)) {
        e.preventDefault();
        openIsland((s.text || '') + e.key);
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [closeOverlay, onOpen]);

  // ── focus: an overlay takes it (and the library behind goes inert); closing gives it back ──
  const lastFocus = useRef<HTMLElement | null>(null);
  useEffect(() => {
    // Remember the last place you were in the library or its chrome (never inside an overlay).
    const onFocusIn = (e: FocusEvent) => {
      if (e.target instanceof HTMLElement && e.target.closest(`#main, .${styles.top}, .${styles.bottom}`)) lastFocus.current = e.target;
    };
    document.addEventListener('focusin', onFocusIn);
    return () => document.removeEventListener('focusin', onFocusIn);
  }, []);
  const wasCovered = useRef(covered);
  useLayoutEffect(() => {
    if (wasCovered.current && !covered) {
      const a = document.activeElement;
      if (!a || a === document.body) {
        // Back where you were: the button of the menu you left from, the element itself, or the
        // focused title (if the lens redrew it meanwhile).
        const last = lastFocus.current;
        const menuButton = last?.closest('[role="menu"]')?.parentElement?.querySelector<HTMLElement>('[aria-haspopup="menu"]');
        const id = lensFocus();
        const tile = id !== null ? activeLens()?.view.artOf?.(id)?.closest<HTMLElement>('a[href], [tabindex]') : null;
        for (const target of [menuButton, last, tile]) {
          if (!target?.isConnected) continue;
          target.focus({ preventScroll: true });
          if (document.activeElement === target) break;
        }
      }
    }
    wasCovered.current = covered;
  }, [covered]);

  // A space with a drive that isn't answering says so, once per browser session.
  useEffect(() => {
    for (const l of space.id === 'all' ? (libs.data ?? []) : lib ? [lib] : []) {
      const off = l.roots.filter((r) => !r.available);
      if (!off.length) continue;
      const key = `flux.told-offline.${l.id}.${off.map((r) => r.path).join('|')}`;
      let told = toldOffline.has(key);
      try {
        told ||= sessionStorage.getItem(key) !== null;
      } catch {
        // storage blocked: once per page instead
      }
      if (told) continue;
      toldOffline.add(key);
      postActivity({
        id: `offline:${l.id}`,
        icon: 'archive',
        title: `Part of ${l.name} is offline`,
        detail: `${off.map((r) => r.path.split(/[\\/]/).filter(Boolean).pop() ?? r.path).join(', ')} isn’t answering. Its titles stay here and play again when it’s back.`,
        ms: 6500,
        // Told for this browser session only once you could see it (not while a sheet covered it).
        onShown: () => {
          try {
            sessionStorage.setItem(key, '1');
          } catch {
            // storage blocked
          }
        },
      });
    }
  }, [libs.data, lib, space.id]);

  const safe = useMemo(() => ({ top: 68 + insets.top, bottom: 84 + insets.bottom, left: 16, right: 16 }), [insets]);
  const OverlayComponent = overlay?.route.Component;
  const spaceName = space.id === 'all' ? 'All' : (lib?.name ?? 'Library');

  return (
    <div className={styles.shell} data-lens={def.id}>
      <div className={styles.room} aria-hidden="true" ref={registerRoom} />
      {def.id === 'folders' && lib ? (
        <main className={styles.flow} id="main" aria-label={spaceName} inert={covered}>
          <Suspense fallback={null}>
            <VideoFolders lib={lib} query={new URLSearchParams(space.href.split('?')[1] ?? '')} />
          </Suspense>
        </main>
      ) : (
        <main id="main" aria-label={spaceName} inert={covered}>
          <LensHost
            def={def}
            items={visible}
            total={total}
            paused={covered}
            safe={safe}
            phone={phone}
            theme={theme}
            onFocus={onFocus}
            onOpen={onOpen}
            onPlay={startPlay}
            onAccessory={setAccessory}
            onScrolled={onScrolled}
            onIntent={onIntent}
            sectionOf={sectionFor}
            onError={onError}
          />
          <Empty list={list} visible={visible.length} filtering={filtering} hasLibraries={libs.data ? libs.data.length > 0 : true} failed={failed} />
        </main>
      )}
      {def.edges && !overlay && (
        <>
          <div className="scroll-edge" data-side="top" aria-hidden="true" />
          <div className="scroll-edge" data-side="bottom" aria-hidden="true" />
        </>
      )}
      <div className={styles.scrim} data-on={islandOpen || undefined} onClick={() => setIslandOpen(false)} aria-hidden="true" />

      {/* The island stays usable over an item page; sheets and the lens gallery hide it. */}
      <header className={styles.top} inert={gallery || Boolean(overlay && SHEETS[overlay.route.name])}>
        <span className={styles.left} />
        <Island
          spaceId={space.id}
          spaceName={spaceName}
          count={list.total === null && !filtering ? null : total}
          text={space.text}
          show={space.show}
          sort={space.sort}
          libraries={libs.data}
          open={islandOpen}
          seed={seed}
          covered={covered}
          onOpenChange={(o) => {
            setIslandOpen(o);
            if (!o) setSeed(null);
          }}
          onText={(q) => setSpaceQuery({ q: q || null })}
          onShow={(s) => setSpaceQuery({ show: s === 'all' ? null : s })}
          onSort={(s) => setSpaceQuery({ sort: s === 'title' ? null : s })}
          onSpace={(id) => {
            setIslandOpen(false);
            goToSpace(id, space.sort);
          }}
          onOpenItem={(it) => {
            setIslandOpen(false);
            onOpen(it.id, activeLens()?.view.artOf?.(it.id) ?? null);
          }}
          onResume={(it) => {
            setIslandOpen(false);
            preloadPlayer();
            startPlay(it.id);
          }}
        />
        <Corner onLenses={() => setGallery(true)} />
      </header>

      <nav className={styles.bottom} aria-label="Lenses" data-mini={mini || undefined} inert={covered}>
        <LensBar lenses={barLenses} current={def.id} mini={mini} accessory={def.id === 'folders' ? null : accessory} onPick={pickLens} onMore={() => setGallery(true)} />
      </nav>

      {gallery && (
        <Suspense fallback={null}>
        <LensGallery
          lenses={lenses}
          current={def.id}
          pins={pins}
          items={visible}
          total={total}
          theme={theme}
          phone={phone}
          sectionOf={sectionFor}
          onChoose={(id) => {
            pickLens(id);
            setGallery(false);
          }}
          onPin={pin}
          onLensesChanged={() => setLensVersion((v) => v + 1)}
          onClose={() => setGallery(false)}
        />
        </Suspense>
      )}

      {overlay && OverlayComponent && (
        <div className={styles.overlay} data-kind={overlay.route.name}>
          <Suspense fallback={<div className={styles.overlayFallback} aria-busy="true" aria-label="Loading" />}>
            {SHEETS[overlay.route.name] ? (
              <Sheet label={SHEETS[overlay.route.name]} onClose={closeOverlay}>
                <OverlayComponent params={overlay.params} query={new URLSearchParams(loc.search)} />
              </Sheet>
            ) : (
              <OverlayComponent params={overlay.params} query={new URLSearchParams(loc.search)} />
            )}
          </Suspense>
        </div>
      )}
      {!m && <NotFound />}
      <Coach />
    </div>
  );
}

function registerRoom(node: HTMLDivElement | null) {
  registerAmbientElement(node);
}

function Empty({ list, visible, filtering, hasLibraries, failed }: { list: ReturnType<typeof useSpaceList>; visible: number; filtering: boolean; hasLibraries: boolean; failed: string | null }) {
  let body: ReactNode = null;
  if (failed) body = <p>The {failed} lens couldn’t start. Pick another lens below.</p>;
  else if (!hasLibraries)
    body = (
      <>
        <h1 className="display-l">Your library is empty</h1>
        <p>Add a folder of films or shows and flux will have it browsable in seconds.</p>
        <Link to="/admin" className="btn btn-primary">
          Add a library
        </Link>
      </>
    );
  else if (list.error !== null && list.items.length === 0)
    body = (
      <>
        <h1 className="display-l">{guardRefusal(list.error) ? 'flux refused this page' : 'Can’t reach the server'}</h1>
        <p>{guardRefusal(list.error) ?? 'flux didn’t answer. Check that it is running, then try again.'}</p>
        <button type="button" className="btn" onClick={list.reload}>
          Try again
        </button>
      </>
    );
  else if (visible === 0 && filtering && (list.done || list.items.length > 0))
    body = (
      <>
        <p>Nothing here matches. Try fewer words, or another filter in the island.</p>
        <button type="button" className="btn" onClick={() => setSpaceQuery({ q: null, show: null })}>
          Clear search and filter
        </button>
      </>
    );
  else if (list.done && list.items.length === 0) body = <p>This library is empty. Add files to its folders and flux will pick them up.</p>;
  if (!body) return null;
  return (
    <div className={styles.empty} role="status">
      {body}
    </div>
  );
}

function NotFound() {
  return (
    <div className={styles.empty} role="alert">
      <h1 className="display-l">Nothing here</h1>
      <p>This address doesn’t match a screen in flux.</p>
      <Link to="/" className="btn">
        Go to your library
      </Link>
    </div>
  );
}

function Coach() {
  const [done, setDone] = useState(() => prefs.get('coach.done', false));
  if (done) return null;
  const close = () => {
    prefs.set('coach.done', true);
    setDone(true);
  };
  return (
    <aside className={`${styles.coach} glass`} aria-label="How flux works now">
      <button type="button" className={styles.coachX} onClick={close} aria-label="Dismiss">
        ×
      </button>
      <b>Your library is the home screen</b>
      <ul>
        <li data-kb>
          <kbd className="mono">type</kbd>
          <span>Search from anywhere; the library filters as you type.</span>
        </li>
        <li data-kb>
          <kbd className="mono">+ −</kbd>
          <span>Zoom the wall, or pinch.</span>
        </li>
        <li data-kb>
          <kbd className="mono">[ ]</kbd>
          <span>Switch lenses; the bar below has them all.</span>
        </li>
        <li data-touch>
          <kbd className="mono">tap</kbd>
          <span>The island at the top: search, spaces, what’s on.</span>
        </li>
        <li data-touch>
          <kbd className="mono">bar</kbd>
          <span>Switch lenses at the bottom; the last button shows them all.</span>
        </li>
      </ul>
    </aside>
  );
}
