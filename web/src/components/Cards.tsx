/**
 * Poster cards (folders, more episodes). Hover or focus does three things:
 * preloads the detail route's code, prefetches the item's data, and lights the
 * room with the artwork's colour (bias lighting).
 */
import { memo, type KeyboardEvent, type Ref } from 'react';
import { setAmbientFromHash } from '../app/ambient';
import { Link } from '../app/Link';
import { prefetchItem } from '../api/queries';
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
