/**
 * Poster and wide (continue-watching) cards. Hover or focus does three things:
 * preloads the detail route's code, prefetches the item's data, and lights the
 * room with the artwork's colour (bias lighting).
 */
import { memo, type KeyboardEvent, type Ref } from 'react';
import { setAmbientFromHash } from '../app/ambient';
import { Link } from '../app/Link';
import { formatRemaining } from '../api/format';
import { parentTitle, prefetchItem } from '../api/queries';
import type { ItemSummary } from '../api/types';
import { Art } from './Art';
import { cardEdge, itemHref } from './card-utils';
import styles from './Cards.module.css';

function intent(item: ItemSummary) {
  prefetchItem(item);
  setAmbientFromHash(item.backdrop?.blurhash ?? item.poster?.blurhash);
}

export interface PosterCardProps {
  item: ItemSummary;
  width: number;
  tabIndex?: number;
  deferImage?: boolean;
  onKeyDown?: (e: KeyboardEvent<HTMLAnchorElement>) => void;
  linkRef?: Ref<HTMLAnchorElement>;
  'data-index'?: number;
}

export const PosterCard = memo(function PosterCard({ item, width, tabIndex, deferImage, onKeyDown, linkRef, ...rest }: PosterCardProps) {
  const progress = item.progress && !item.progress.watched && item.progress.duration_ms > 0 ? item.progress.position_ms / item.progress.duration_ms : null;
  const isEpisode = item.kind === 'episode';
  return (
    <Link
      to={itemHref(item)}
      className={styles.card}
      tabIndex={tabIndex}
      onIntent={() => intent(item)}
      onKeyDown={onKeyDown}
      ref={linkRef}
      data-nav-item
      data-index={rest['data-index']}
      aria-label={`${item.title}${item.year ? `, ${item.year}` : ''}${item.progress?.watched ? ', watched' : ''}`}
    >
      <div className={styles.frame}>
        <Art art={item.poster} width={width} aspect={isEpisode ? 'wide' : 'poster'} title={item.title} deferImage={deferImage} />
        {progress !== null && (
          <span className={styles.progress} aria-hidden="true">
            <span style={{ width: `${Math.max(3, progress * 100)}%` }} />
          </span>
        )}
        {item.progress?.watched && (
          <span className={styles.watched} aria-hidden="true" title="Watched">
            <svg viewBox="0 0 16 16">
              <path d="m3.5 8.5 3 3 6-7" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" />
            </svg>
          </span>
        )}
      </div>
      <span className={styles.title}>{item.title}</span>
      <span className={`edge ${styles.edge}`}>{cardEdge(item) || ' '}</span>
    </Link>
  );
});

/** Landscape card with resume progress, for Continue watching. */
export const WideCard = memo(function WideCard({ item, width, onKeyDown, tabIndex }: { item: ItemSummary; width: number; onKeyDown?: (e: KeyboardEvent<HTMLAnchorElement>) => void; tabIndex?: number }) {
  const p = item.progress;
  const frac = p && p.duration_ms > 0 ? p.position_ms / p.duration_ms : 0;
  const art = item.kind === 'episode' ? item.poster : item.backdrop ?? item.poster;
  const show = parentTitle(item);
  return (
    <Link to={itemHref(item)} className={`${styles.card} ${styles.wideCard}`} onIntent={() => intent(item)} onKeyDown={onKeyDown} tabIndex={tabIndex} data-nav-item>
      <div className={styles.frame}>
        <Art art={art} width={width} aspect="wide" title={item.title} showSourceLabel={item.kind !== 'episode'} />
        <span className={styles.wideShade} aria-hidden="true" />
        <span className={styles.wideText}>
          <span className={styles.wideTitle}>{item.kind === 'episode' ? (show ?? item.title) : item.title}</span>
          {p && (
            <span className="edge">
              {item.kind === 'episode' && item.subtitle ? `${item.subtitle} · ` : ''}
              {formatRemaining(p.position_ms, p.duration_ms)}
            </span>
          )}
        </span>
        <span className={styles.progress} aria-hidden="true">
          <span style={{ width: `${Math.max(3, frac * 100)}%` }} />
        </span>
      </div>
    </Link>
  );
});
