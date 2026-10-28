import { Suspense, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { Shell } from '../shell/Shell';
import styles from './App.module.css';
import { CrashBoundary } from './Crash';
import { back, consumeScrollRestore, type Location, matchRoute, useLocation } from './router';
import './routes';
import { ScreenLeavingContext } from './screen';

/** How long the player takes to fade in over the page you played from, or away over the page you go back to. */
const FADE_MS = 280;

/**
 * The player is a bare route; everything else lives in the shell (the library, with
 * overlays over it). Between the two, the one going stays a moment under (or over)
 * the one coming, so the player fades in like the lights going down, and away again.
 */
export function App() {
  const loc = useLocation();
  const bare = Boolean(matchRoute(loc.pathname)?.route.bare);
  useLayoutEffect(() => {
    consumeScrollRestore();
  }, [loc.pathname, loc.search]);

  // The screen on its way out when the player opens or closes.
  const [last, setLast] = useState({ loc, bare });
  const [held, setHeld] = useState<{ loc: Location; bare: boolean } | null>(null);
  if (last.loc !== loc) {
    setLast({ loc, bare });
    if (last.bare !== bare) setHeld(last);
  }
  useEffect(() => {
    if (!held) return;
    const t = setTimeout(() => setHeld(null), FADE_MS + 60);
    return () => clearTimeout(t);
  }, [held]);

  const shellLoc = !bare ? loc : held && !held.bare ? held.loc : null;
  const playerLoc = bare ? loc : held?.bare ? held.loc : null;
  return (
    <>
      {shellLoc && (
        <ScreenLeavingContext.Provider value={bare}>
          <Shell key="shell" loc={shellLoc} />
        </ScreenLeavingContext.Provider>
      )}
      {playerLoc && <Bare key="bare" loc={playerLoc} leaving={!bare} />}
    </>
  );
}

function Bare({ loc, leaving }: { loc: Location; leaving: boolean }) {
  const m = matchRoute(loc.pathname);
  const box = useRef<HTMLDivElement>(null);
  // The component it started with, for good: the router hands out the loaded component
  // once its code is in, and a different component type would start the player over.
  const [Component] = useState(() => m?.route.Component ?? null);
  // Leaving: the picture stops as the player fades.
  useEffect(() => {
    if (leaving) for (const v of box.current?.querySelectorAll('video') ?? []) v.pause();
  }, [leaving]);
  if (!m || !Component) return null;
  return (
    <div ref={box} className={styles.bare} data-leaving={leaving || undefined} inert={leaving}>
      <ScreenLeavingContext.Provider value={leaving}>
        <CrashBoundary place="player" onBack={() => back()}>
          <Suspense fallback={<div className={styles.bareFallback} />}>
            <Component params={m.params} query={new URLSearchParams(loc.search)} />
          </Suspense>
        </CrashBoundary>
      </ScreenLeavingContext.Provider>
    </div>
  );
}
