/**
 * Home: pick up where you left off, then what's new, then each library.
 * No promoted or "discover" content — only the household's own library
 * (research/01 §8.2.7).
 */
import { useEffect } from 'react';
import { setAmbientFromHash } from '../app/ambient';
import { Link } from '../app/Link';
import { useTitle } from '../app/useTitle';
import { guardRefusal } from '../api/client';
import { formatRemaining, formatRuntime } from '../api/format';
import { parentTitle, prefetchItem, rememberSummaries, useHome, useTailnet } from '../api/queries';
import type { ItemSummary } from '../api/types';
import { Art } from '../components/Art';
import { PosterCard, WideCard } from '../components/Cards';
import { itemHref } from '../components/card-utils';
import { Icon } from '../components/Icon';
import { Row, RowItem } from '../components/Row';
import { preloadPlayer, startPlay } from '../player/start';
import styles from './Home.module.css';

function Hero({ item, resume }: { item: ItemSummary; resume: boolean }) {
  const p = item.progress;
  const art = item.backdrop ?? item.poster;
  const show = parentTitle(item);
  useEffect(() => setAmbientFromHash(art?.blurhash, true), [art?.blurhash]);
  const frac = p && p.duration_ms ? p.position_ms / p.duration_ms : 0;
  const meta: string[] = [];
  if (item.kind === 'episode' && item.subtitle) meta.push(item.subtitle);
  else if (item.year) meta.push(String(item.year));
  if (resume && p) meta.push(formatRemaining(p.position_ms, p.duration_ms));
  else if (p?.duration_ms) meta.push(formatRuntime(p.duration_ms));
  meta.push(...item.badges);
  return (
    <section className={styles.hero} aria-labelledby="hero-title">
      {/* Decorative: the hero's own heading carries the title (no typeset duplicate behind it). */}
      <Art art={art} width={1400} aspect="wide" title="" priority className={styles.heroArt} showSourceLabel={false} />
      <div className={styles.heroShade} aria-hidden="true" />
      <div className={styles.heroBody}>
        <p className="edge">{resume ? 'Continue watching' : 'Just added'}</p>
        <h1 id="hero-title" className="display-xl">
          {show ?? item.title}
        </h1>
        <p className={`edge ${styles.heroMeta}`}>{meta.join(' · ')}</p>
        {resume && p && (
          <div className={styles.heroProgress} aria-label={`${Math.round(frac * 100)}% watched`} role="img">
            <span style={{ width: `${frac * 100}%` }} />
          </div>
        )}
        <div className={styles.heroActions}>
          <button type="button" className="btn btn-primary btn-lg" onClick={() => startPlay(item.id)} onPointerEnter={preloadPlayer} onFocus={preloadPlayer}>
            <Icon name="play" />
            {resume ? 'Resume' : 'Play'}
          </button>
          <Link to={itemHref(item)} className="btn btn-lg" onIntent={() => prefetchItem(item)}>
            Details
          </Link>
        </div>
      </div>
    </section>
  );
}

function ConnectBanner() {
  const t = useTailnet();
  if (!t.data || t.data.state === 'running') return null;
  return (
    <aside className={styles.banner}>
      <div>
        <p className={styles.bannerTitle}>Watch away from home</p>
        <p className="muted">Sign this server in to Tailscale and your family can stream from anywhere — no ports to open, no passwords to share.</p>
      </div>
      <Link to="/connect" className="btn">
        Set up remote access
      </Link>
    </aside>
  );
}

function HomeSkeleton() {
  return (
    <div aria-busy="true" aria-label="Loading home">
      <div className={`skeleton ${styles.heroSkeleton}`} />
      <div className={styles.rows}>
        {[0, 1].map((r) => (
          <div key={r} className={styles.skeletonRow}>
            {Array.from({ length: 8 }, (_, i) => (
              <span key={i} className="skeleton" />
            ))}
          </div>
        ))}
      </div>
    </div>
  );
}

export default function Home() {
  useTitle(null);
  const home = useHome();
  const data = home.data;
  useEffect(() => {
    if (data) rememberSummaries([...data.continue_watching, ...data.recently_added, ...data.rows.flatMap((r) => r.items)]);
  }, [data]);

  if (!data) {
    if (home.status === 'error') {
      const refused = guardRefusal(home.error);
      return (
        <div className={styles.error}>
          <h1 className="display-l">{refused ? 'flux refused this page' : 'Can’t reach the server'}</h1>
          <p className="muted">{refused ?? 'flux didn’t answer. Check that it is running, then try again.'}</p>
          <button type="button" className="btn" onClick={() => void home.refetch()}>
            Try again
          </button>
        </div>
      );
    }
    return <HomeSkeleton />;
  }

  const [lead, ...rest] = data.continue_watching;
  const heroItem = lead ?? data.recently_added[0];
  const empty = !heroItem && data.rows.every((r) => r.items.length === 0);

  if (empty) {
    return (
      <div className={styles.error}>
        <h1 className="display-l">Your library is empty</h1>
        <p className="muted">Add a folder of films or shows and flux will have it browsable in seconds.</p>
        <Link to="/admin" className="btn btn-primary">
          Add a library
        </Link>
      </div>
    );
  }

  return (
    <div className={styles.home}>
      {heroItem && <Hero item={heroItem} resume={Boolean(lead)} />}
      <div className={styles.rows}>
        <ConnectBanner />
        {rest.length > 0 && (
          <Row title="Continue watching" variant="wide">
            {rest.map((it) => (
              <RowItem key={it.id}>
                <WideCard item={it} width={360} />
              </RowItem>
            ))}
          </Row>
        )}
        {data.recently_added.length > 0 && (
          <Row title="Recently added">
            {data.recently_added.map((it) => (
              <RowItem key={it.id}>
                <PosterCard item={it} width={184} />
              </RowItem>
            ))}
          </Row>
        )}
        {data.rows.map((row) =>
          row.items.length === 0 ? null : (
            <Row key={row.title} title={row.title} seeAllHref={row.library_id !== null ? `/library/${row.library_id}` : undefined}>
              {row.items.map((it) => (
                <RowItem key={it.id}>
                  <PosterCard item={it} width={184} />
                </RowItem>
              ))}
            </Row>
          ),
        )}
      </div>
    </div>
  );
}
