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
import { CrashBoundary } from '../app/Crash';
import { isStaleCode } from '../app/update';
import { recoverFromStaleCode } from '../app/update-checks';
import { Link } from '../app/Link';
import { back, matchRoute, navigateWhenLoaded, preloadRoute, type Location } from '../app/router';
import { useScreenLeaving } from '../app/screen';
import { useTheme } from '../app/theme';
import { guardRefusal } from '../api/client';
import { watchEventsState } from '../api/events';
import { holdPaging } from '../api/library-list';
import { prefetchItem, useCollections, useLibraries, useServer, useServerLenses } from '../api/queries';
import { collectionOf, useSpaceList } from '../api/space-list';
import type { Id, ItemSummary, LibraryKind } from '../api/types';
import { MOCK } from '../config';
import { LensHost } from '../lens/LensHost';
import { activeLens, lensFocus, setLensFocus } from '../lens/bridge';
import { prefs } from '../lens/dom';
import { spotIn, spotOfElement, takeoff, warm } from '../lens/flight';
import { allLenses, DEFAULT_PINS, FOLDERS_LENS, lensById, setServerLenses, suits } from '../lens/registry';
import { sectionOf, serverSort, visibleItems } from '../lens/sections';
import type { Dir, LensDef } from '../lens/types';
import { preloadPlayer, startPlay } from '../player/start';
import { detailPosterSrc } from '../routes/item-poster';
import { Corner } from './Corner';
import { reportShell, setShellPicker } from './embed';
import { useEventsBridge } from './events-bridge';
import { startGlassLight } from './glass-light';
import { Island } from './Island';
import { LensBar } from './LensBar';
import { postActivity } from './live';
import { type OverlayExit, OverlayExitContext } from './overlay-exit';
import { Sheet } from './Sheet';
import styles from './Shell.module.css';
import { goToSpace, setSpaceQuery, spaceFor, spaceKey } from './space';
import { ReleaseNote, UpdateNote } from './UpdateNote';
import { useInsets, usePhone } from './viewport';

const SHEETS: Record<string, string> = { admin: 'Libraries & scanning', connect: 'Remote access', changes: 'Agents & changes' };

const toldOffline = new Set<string>();

// Loaded when needed: folders only for videos libraries, the gallery soon after the
// shell is up (so its cards come in with the library stepping back, not after it).
const VideoFolders = lazy(() => import('../routes/VideoFolders').then((m) => ({ default: m.VideoFolders })));
const loadLensGallery = () => import('./LensGallery').then((m) => ({ default: m.LensGallery }));
const LensGallery = lazy(loadLensGallery);

type RouteMatch = NonNullable<ReturnType<typeof matchRoute>>;

/** A page open over the library: its route, and the address it opened at (its key: one per page). */
interface OverlayEntry {
  key: string;
  match: RouteMatch;
  search: string;
  /** The history entry it opened in. */
  visit: string;
}

/** The lens bar comes back this long after the lens stops scrolling. */
const BAR_BACK_MS = 480;

export function Shell({ loc }: { loc: Location }) {
  const m = matchRoute(loc.pathname);
  const overlay = m && m.route.layer === 'overlay' ? m : null;
  const active: OverlayEntry | null = overlay ? { key: loc.pathname, match: overlay, search: loc.search, visit: loc.key } : null;
  // A page whose address has gone stays (leaving) until it has animated out (overlay-exit.ts).
  const [shown, setShown] = useState<OverlayEntry | null>(active);
  const [exits, setExits] = useState<OverlayEntry[]>([]);
  // The page there when the shell came up (a link, back from the player) shows at once;
  // the same page opened again later comes in as usual.
  const [firstVisit] = useState(() => (active ? loc.key : null));
  if ((shown?.key ?? null) !== (active?.key ?? null) || (shown && active && shown.search !== active.search)) {
    setShown(active);
    if (shown && shown.key !== active?.key) setExits((x) => [...x.filter((e) => e.key !== shown.key && e.key !== active?.key), shown]);
    else if (active && exits.some((e) => e.key === active.key)) setExits((x) => x.filter((e) => e.key !== active.key));
  }
  const space = spaceFor(loc.pathname, loc.search);
  const libs = useLibraries();
  const collections = useCollections();
  const collectionId = collectionOf(space.id);
  const collection = collectionId === null ? undefined : collections.data?.collections.find((c) => c.id === collectionId);
  const lib = typeof space.id === 'number' ? libs.data?.find((l) => l.id === space.id) : undefined;
  // A collection holds films and shows from any library: browsed like All.
  const kind: LibraryKind | 'all' = typeof space.id === 'number' ? (lib?.kind ?? 'movies') : 'all';
  const phone = usePhone();
  const insets = useInsets();
  const { resolved: theme } = useTheme();
  // Overlays set their own title; the space sets it otherwise.
  const libName = lib?.name ?? collection?.name;
  useEffect(() => {
    if (!overlay) document.title = space.id === 'all' || !libName ? 'flux' : `${libName} · flux`;
  }, [overlay, space.id, libName]);
  useEventsBridge();
  useEffect(() => startGlassLight(), []);
  useEffect(() => {
    // A warm-up: if the code is gone (flux updated), opening the gallery says so.
    const t = setTimeout(() => void loadLensGallery().catch(() => undefined), 2500);
    return () => clearTimeout(t);
  }, []);

  // ── results ──
  const list = useSpaceList(space.id, serverSort(space.sort), libs.data);
  // The server's libraries didn't arrive (flux down when the page opened, from the installed
  // app's kept copy): "All" can't start without them, so the empty state says why.
  const libsFailed = libs.status === 'error' && libs.data === undefined ? libs.error : null;
  const retry = useCallback(() => {
    if (libs.status === 'error') void libs.refetch().catch(() => undefined);
    list.reload();
  }, [libs, list]);
  // flux back (the event stream opens again): fetch what failed while it was away.
  const retryLatest = useRef(retry);
  const failedNow = libsFailed !== null || list.error !== null;
  const failedLatest = useRef(failedNow);
  useEffect(() => {
    retryLatest.current = retry;
    failedLatest.current = failedNow;
  });
  useEffect(
    () =>
      watchEventsState((s) => {
        if (s === 'open' && failedLatest.current) retryLatest.current();
      }),
    [],
  );
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
  // The lens bar tucks away while a lens scrolls (onScrolled below; a new lens starts with it full).
  const [mini, setMini] = useState(false);
  const [accessory, setAccessory] = useState<HTMLElement | null>(null);
  // The lens gallery: open, or closing (it animates away; the library steps forward meanwhile).
  const [galleryState, setGallery] = useState<'open' | 'closing' | null>(null);
  const gallery = galleryState === 'open';
  // A new one each time it opens (one closing never comes back half-gone).
  const [galleryKey, setGalleryKey] = useState(0);
  const openGallery = useCallback(() => {
    setGalleryKey((k) => k + 1);
    setGallery('open');
  }, []);
  const closeGallery = useCallback(() => setGallery((g) => (g ? 'closing' : null)), []);
  const galleryGone = useCallback(() => setGallery(null), []);
  const [failure, setFailure] = useState<{ id: string; name: string } | null>(null);
  // Covered until a page leaving has gone (its poster lands in the lens first, still paused).
  const covered = Boolean(overlay) || gallery || exits.length > 0;

  // Before the first paint: a page there when the shell comes up (back from the player)
  // hides the lens bar from the first frame, not one frame later.
  const overlayName = overlay?.route.name ?? null;
  useLayoutEffect(() => {
    document.documentElement.dataset.overlay = overlayName ?? (gallery ? 'gallery' : '');
    return () => {
      document.documentElement.dataset.overlay = '';
    };
  }, [overlayName, gallery]);

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
  const visibleNow = useRef(visible);
  useEffect(() => {
    visibleNow.current = visible;
  });
  const onOpen = useCallback((id: Id, art: Element | null) => {
    // Where its poster is now: the detail page flies it from there.
    const view = activeLens()?.view ?? null;
    const item = visibleNow.current.find((i) => i.id === id);
    if (item) {
      // Its page paints from the summary at once, with the poster the flight lands on.
      prefetchItem(item);
      takeoff(item, spotIn(view, id) ?? (art ? spotOfElement(art) : null), view);
    }
    setIslandOpen(false);
    navigateWhenLoaded(`/item/${id}`);
  }, []);
  const onIntent = useCallback((item: ItemSummary) => {
    prefetchItem(item);
    void preloadRoute('/item/0');
    // The detail page's poster, decoded before you click (it lands sharp).
    void warm(detailPosterSrc(item));
  }, []);
  // The lens bar tucks away while the lens scrolls and comes back once it stops (or at
  // the top); never while you are on it (pointer or keyboard), so you can always switch.
  const barT = useRef<ReturnType<typeof setTimeout> | null>(null);
  const barHot = useRef(false);
  const showBar = useCallback(() => {
    if (barT.current) clearTimeout(barT.current);
    barT.current = null;
    setMini(false);
  }, []);
  const onScrolled = useCallback(
    (dir: 'down' | 'up' | 'top') => {
      if (dir === 'top' || barHot.current) return showBar();
      if (barT.current) clearTimeout(barT.current);
      setMini(true);
      barT.current = setTimeout(() => {
        barT.current = null;
        setMini(false);
      }, BAR_BACK_MS);
    },
    [showBar],
  );
  useEffect(
    () => () => {
      if (barT.current) clearTimeout(barT.current);
    },
    [],
  );
  const sectionFor = useCallback((item: ItemSummary) => sectionOf(item, space.sort), [space.sort]);
  const onError = useCallback((d: LensDef, err: unknown) => {
    console.error(`lens ${d.id} failed`, err);
    setFailure({ id: d.id, name: d.name });
    // Its code is gone: flux was updated under this page (the other lenses' code went too).
    if (isStaleCode(err)) void recoverFromStaleCode();
  }, []);
  const failed = failure?.id === def.id ? failure.name : null;

  const closeOverlay = useCallback(() => back(space.href), [space.href]);

  // The product site's live demo (a mock build in an iframe) switches lenses from outside, through src/mock/demo-bridge.ts.
  const overlayOpen = Boolean(overlay);
  useEffect(() => {
    if (!MOCK) return;
    setShellPicker((id) => {
      if (overlayOpen) closeOverlay();
      setGallery(null);
      setIslandOpen(false);
      pickLens(id);
    });
    return () => setShellPicker(null);
  }, [overlayOpen, closeOverlay, pickLens]);
  useEffect(() => {
    if (MOCK) reportShell({ lens: def.id, items: visible.length });
  }, [def.id, visible.length]);

  // ── keyboard: type anywhere to search, arrows move in the lens, Esc steps back ──
  // No keys while the shell is on its way out (under the player fading in), and none
  // for the lens while a page or the lens gallery is still leaving (the lens is inert).
  const held = useScreenLeaving();
  const settling = exits.length > 0 || galleryState === 'closing';
  const keyState = useRef({ overlay, islandOpen, gallery, text: space.text, held, settling });
  const stepRef = useRef(stepLens);
  useEffect(() => {
    keyState.current = { overlay, islandOpen, gallery, text: space.text, held, settling };
    stepRef.current = stepLens;
  });
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const s = keyState.current;
      if (s.held) return;
      const t = e.target as HTMLElement | null;
      const typing = t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.tagName === 'SELECT' || t.isContentEditable);
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') {
        e.preventDefault();
        setIslandOpen((o) => !o);
        return;
      }
      if (e.key === 'Escape') {
        if (e.defaultPrevented) return;
        if (s.gallery) return closeGallery();
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
      if (typing || s.overlay || s.gallery || s.settling || s.islandOpen || e.altKey) return;
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
        openGallery();
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
  }, [closeOverlay, onOpen, openGallery, closeGallery]);

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
  // The lens gallery grows the lens you choose into the window itself: the switch behind it just happens.
  const quietSwitch = useRef(false);
  const spaceName = space.id === 'all' ? 'All' : collectionId !== null ? (collection?.name ?? 'Collection') : (lib?.name ?? 'Library');

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
            quietNext={quietSwitch}
            onFocus={onFocus}
            onOpen={onOpen}
            onPlay={startPlay}
            onAccessory={setAccessory}
            onScrolled={onScrolled}
            onIntent={onIntent}
            sectionOf={sectionFor}
            onError={onError}
          />
          <Empty list={list} visible={visible.length} filtering={filtering} hasLibraries={libs.data ? libs.data.length > 0 : true} failed={failed} unreachable={libsFailed} onRetry={retry} />
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
          collections={collections.data?.collections}
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
        <Corner onLenses={openGallery} />
      </header>

      <nav
        className={styles.bottom}
        aria-label="Lenses"
        data-mini={mini || undefined}
        inert={covered}
        onPointerEnter={() => {
          barHot.current = true;
          showBar();
        }}
        onPointerLeave={() => {
          barHot.current = false;
        }}
        onFocus={() => {
          barHot.current = true;
          showBar();
        }}
        onBlur={(e) => {
          if (!e.currentTarget.contains(e.relatedTarget)) barHot.current = false;
        }}
      >
        <LensBar lenses={barLenses} current={def.id} mini={mini} accessory={def.id === 'folders' ? null : accessory} onPick={pickLens} onMore={openGallery} />
      </nav>

      {galleryState && (
        <Suspense fallback={null}>
        <LensGallery
          key={galleryKey}
          lenses={lenses}
          current={def.id}
          pins={pins}
          items={visible}
          total={total}
          theme={theme}
          phone={phone}
          safe={safe}
          sectionOf={sectionFor}
          onChoose={(id) => {
            // It grows the lens into the window as the library behind switches to it and steps forward.
            quietSwitch.current = id !== def.id;
            pickLens(id);
            setGallery('closing');
          }}
          onPin={pin}
          onLensesChanged={() => setLensVersion((v) => v + 1)}
          onClose={closeGallery}
          closing={galleryState === 'closing'}
          onGone={galleryGone}
        />
        </Suspense>
      )}

      {[...exits, ...(active ? [active] : [])].map((e) => (
        <OverlayFrame
          key={e.key}
          entry={e}
          leaving={e.key !== active?.key}
          toLibrary={!active}
          restored={e.visit === firstVisit}
          onClose={closeOverlay}
          onDone={() => setExits((x) => x.filter((y) => y.key !== e.key))}
        />
      ))}
      {!m && <NotFound />}
      <div className={styles.notices}>
        <UpdateNote />
        <ReleaseNote />
        <Coach />
      </div>
    </div>
  );
}

/** A page open over the library, or leaving it (it says when it has gone; at worst it goes anyway). */
function OverlayFrame({ entry, leaving, toLibrary, restored, onClose, onDone }: { entry: OverlayEntry; leaving: boolean; toLibrary: boolean; restored: boolean; onClose: () => void; onDone: () => void }) {
  const { route, params } = entry.match;
  // The component it started with, for good: the router hands out the loaded component
  // once its code is in, and a different component type would rebuild the page (its
  // entrance would play again under the next page coming in).
  const [Component] = useState(() => route.Component);
  const query = useMemo(() => new URLSearchParams(entry.search), [entry.search]);
  const last = useRef(onDone);
  useEffect(() => {
    last.current = onDone;
  });
  const done = useCallback(() => last.current(), []);
  const exit = useMemo<OverlayExit>(() => ({ leaving, toLibrary, restored, done }), [leaving, toLibrary, restored, done]);
  useEffect(() => {
    if (!leaving) return;
    const t = setTimeout(done, 1600);
    return () => clearTimeout(t);
  }, [leaving, done]);
  const label = SHEETS[route.name];
  return (
    <div className={styles.overlay} data-kind={route.name} data-leaving={leaving || undefined} data-to={leaving ? (toLibrary ? 'library' : 'page') : undefined} inert={leaving}>
      <OverlayExitContext.Provider value={exit}>
        <CrashBoundary place="overlay" onClose={onClose}>
          <Suspense fallback={<div className={styles.overlayFallback} aria-busy="true" aria-label="Loading" />}>
            {label ? (
              <Sheet label={label} onClose={onClose}>
                <Component params={params} query={query} />
              </Sheet>
            ) : (
              <Component params={params} query={query} />
            )}
          </Suspense>
        </CrashBoundary>
      </OverlayExitContext.Provider>
    </div>
  );
}

function registerRoom(node: HTMLDivElement | null) {
  registerAmbientElement(node);
}

function Empty({
  list,
  visible,
  filtering,
  hasLibraries,
  failed,
  unreachable,
  onRetry,
}: {
  list: ReturnType<typeof useSpaceList>;
  visible: number;
  filtering: boolean;
  hasLibraries: boolean;
  failed: string | null;
  /** Why the server's libraries didn't arrive, when they didn't. */
  unreachable: unknown;
  onRetry: () => void;
}) {
  let body: ReactNode = null;
  // Only over an empty window: titles already showing stay, whatever failed beside them.
  const error = list.items.length === 0 ? (unreachable ?? list.error) : null;
  if (failed) body = <p>The {failed} lens couldn’t start. Pick another lens below.</p>;
  else if (error !== null && error !== undefined)
    body = (
      <>
        <h1 className="display-l">{guardRefusal(error) ? 'flux refused this page' : 'Can’t reach the server'}</h1>
        <p>{guardRefusal(error) ?? 'flux didn’t answer. Check that it is running, then try again.'}</p>
        <button type="button" className="btn" onClick={onRetry}>
          Try again
        </button>
      </>
    );
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
