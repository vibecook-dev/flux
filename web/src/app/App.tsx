import { Suspense, useLayoutEffect } from 'react';
import { Shell } from '../shell/Shell';
import styles from './App.module.css';
import { consumeScrollRestore, matchRoute, useLocation } from './router';
import './routes';

/** The player is a bare route; everything else lives in the shell (the library, with overlays over it). */
export function App() {
  const loc = useLocation();
  const m = matchRoute(loc.pathname);
  useLayoutEffect(() => {
    consumeScrollRestore();
  }, [loc.pathname, loc.search]);

  if (m?.route.bare) {
    const { Component } = m.route;
    return (
      <Suspense fallback={<div className={styles.bareFallback} />}>
        <Component params={m.params} query={new URLSearchParams(loc.search)} />
      </Suspense>
    );
  }
  return <Shell loc={loc} />;
}
