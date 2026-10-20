import { Suspense, useEffect, useLayoutEffect, useState } from 'react';
import { MOCK } from '../config';
import { formatCount } from '../api/format';
import { useLibraries, useTailnet } from '../api/queries';
import type { Library } from '../api/types';
import { Icon, type IconName } from '../components/Icon';
import { SearchOverlay } from '../components/SearchOverlay';
import { registerAmbientElement } from './ambient';
import styles from './App.module.css';
import { consumeScrollRestore, matchRoute, useLocation } from './router';
import { Link } from './Link';
import './routes';
import { setTheme, useTheme, type ThemeSetting } from './theme';

function libraryIcon(l: Library): IconName {
  if (l.kind === 'shows') return 'tv';
  return l.scan.state === 'unavailable' || l.name.toLowerCase().includes('archive') ? 'archive' : 'film';
}

function Wordmark() {
  return (
    <Link to="/" className={styles.wordmark} aria-label="flux home">
      <svg viewBox="0 0 40 28" aria-hidden="true" className={styles.mark}>
        <path d="M2 26 L20 2 L27 2 L9 26 Z" />
        <path d="M16 26 L28 10 L33 10 L21 26 Z" opacity=".45" />
      </svg>
      <span>flux</span>
    </Link>
  );
}

function ThemeSwitch() {
  const { setting } = useTheme();
  const options: [ThemeSetting, IconName, string][] = [
    ['system', 'monitor', 'Match system'],
    ['dark', 'moon', 'Dark'],
    ['light', 'sun', 'Light'],
  ];
  return (
    <div className={styles.themeSwitch} role="radiogroup" aria-label="Theme">
      {options.map(([value, icon, label]) => (
        <button key={value} type="button" role="radio" aria-checked={setting === value} aria-label={label} title={label} onClick={() => setTheme(value)}>
          <Icon name={icon} />
        </button>
      ))}
    </div>
  );
}

function NavLink({ to, icon, label, meta, active, dot }: { to: string; icon: IconName; label: string; meta?: string; active: boolean; dot?: 'warn' | 'ok' }) {
  return (
    <Link to={to} className={styles.navLink} aria-current={active ? 'page' : undefined} title={label}>
      <Icon name={icon} />
      <span className={styles.navLabel}>{label}</span>
      {meta && <span className={`edge ${styles.navMeta}`}>{meta}</span>}
      {dot && <span className={`${styles.dot} ${dot === 'warn' ? styles.dotWarn : styles.dotOk}`} aria-hidden="true" />}
    </Link>
  );
}

function Sidebar({ pathname, onSearch }: { pathname: string; onSearch: () => void }) {
  const libs = useLibraries();
  const tailnet = useTailnet();
  const connected = tailnet.data?.state === 'running';
  return (
    <nav className={styles.sidebar} aria-label="Main">
      <Wordmark />
      <div className={styles.navGroup}>
        <NavLink to="/" icon="home" label="Home" active={pathname === '/'} />
        <button type="button" className={styles.navLink} onClick={onSearch} title="Search">
          <Icon name="search" />
          <span className={styles.navLabel}>Search</span>
          <kbd className={styles.kbd}>/</kbd>
        </button>
      </div>
      <div className={styles.navGroup}>
        <h2 className={`edge ${styles.navHeading}`}>Libraries</h2>
        {libs.data?.map((l) => (
          <NavLink
            key={l.id}
            to={`/library/${l.id}`}
            icon={libraryIcon(l)}
            label={l.name}
            meta={formatCount(l.item_count)}
            active={pathname === `/library/${l.id}`}
            dot={l.scan.state === 'unavailable' || l.scan.state === 'error' ? 'warn' : undefined}
          />
        ))}
        {!libs.data && [0, 1, 2].map((i) => <span key={i} className={`skeleton ${styles.navSkeleton}`} />)}
      </div>
      <div className={`${styles.navGroup} ${styles.navBottom}`}>
        <NavLink to="/admin" icon="settings" label="Libraries & scanning" active={pathname === '/admin'} />
        <NavLink to="/connect" icon="link" label={connected ? 'Remote access' : 'Set up remote access'} active={pathname === '/connect'} dot={connected ? 'ok' : 'warn'} />
        <ThemeSwitch />
      </div>
    </nav>
  );
}

function TabBar({ pathname, onSearch }: { pathname: string; onSearch: () => void }) {
  const libs = useLibraries();
  const shown = (libs.data ?? []).slice(0, 2);
  return (
    <nav className={styles.tabbar} aria-label="Main">
      <Link to="/" className={styles.tab} aria-current={pathname === '/' ? 'page' : undefined}>
        <Icon name="home" />
        <span>Home</span>
      </Link>
      {shown.map((l) => (
        <Link key={l.id} to={`/library/${l.id}`} className={styles.tab} aria-current={pathname === `/library/${l.id}` ? 'page' : undefined}>
          <Icon name={libraryIcon(l)} />
          <span>{l.name.split(' ')[0]}</span>
        </Link>
      ))}
      <button type="button" className={styles.tab} onClick={onSearch}>
        <Icon name="search" />
        <span>Search</span>
      </button>
      <Link to="/admin" className={styles.tab} aria-current={pathname === '/admin' || pathname === '/connect' ? 'page' : undefined}>
        <Icon name="settings" />
        <span>Server</span>
      </Link>
    </nav>
  );
}

function RouteFallback() {
  return (
    <div className={styles.fallback} aria-busy="true" aria-label="Loading">
      <span className={`skeleton ${styles.fallbackTitle}`} />
      <div className={styles.fallbackRow}>
        {Array.from({ length: 7 }, (_, i) => (
          <span key={i} className="skeleton" />
        ))}
      </div>
    </div>
  );
}

function Outlet({ pathname, search }: { pathname: string; search: string }) {
  const m = matchRoute(pathname);
  useLayoutEffect(() => {
    consumeScrollRestore();
  }, [pathname, search]);
  if (!m) {
    return (
      <div className={styles.notFound}>
        <h1 className="display-l">Nothing here</h1>
        <p className="muted">This address doesn’t match a screen in flux.</p>
        <Link to="/" className="btn">
          Go home
        </Link>
      </div>
    );
  }
  const { Component } = m.route;
  return <Component params={m.params} query={new URLSearchParams(search)} />;
}

export function App() {
  const loc = useLocation();
  const [searchOpen, setSearchOpen] = useState(false);
  const bare = matchRoute(loc.pathname)?.route.bare ?? false;

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const t = e.target as HTMLElement | null;
      const typing = t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.tagName === 'SELECT' || t.isContentEditable);
      if ((e.key === '/' && !typing) || (e.key.toLowerCase() === 'k' && (e.metaKey || e.ctrlKey))) {
        if (bare) return;
        e.preventDefault();
        setSearchOpen(true);
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [bare]);

  if (bare) {
    return (
      <Suspense fallback={<div className={styles.bareFallback} />}>
        <Outlet pathname={loc.pathname} search={loc.search} />
      </Suspense>
    );
  }

  return (
    <div className={styles.app}>
      <div className={styles.ambient} ref={registerAmbientElement} aria-hidden="true" />
      <a href="#main" className={styles.skip}>
        Skip to content
      </a>
      <Sidebar pathname={loc.pathname} onSearch={() => setSearchOpen(true)} />
      <header className={styles.topbar}>
        <Wordmark />
        <button type="button" className={styles.topSearch} onClick={() => setSearchOpen(true)} aria-label="Search">
          <Icon name="search" />
        </button>
      </header>
      <main id="main" className={styles.main}>
        <Suspense fallback={<RouteFallback />}>
          <Outlet pathname={loc.pathname} search={loc.search} />
        </Suspense>
        {MOCK && (
          <p className={`edge ${styles.mockNote}`}>
            Mock server · sample data built from the flux test library · set VITE_FLUX_API to use a real server
          </p>
        )}
      </main>
      <TabBar pathname={loc.pathname} onSearch={() => setSearchOpen(true)} />
      <SearchOverlay open={searchOpen} onClose={() => setSearchOpen(false)} />
    </div>
  );
}
