/**
 * Artwork with a blurhash placeholder, width-hinted `srcset`, async decode and
 * a short fade-in. Art the server didn't fetch from a provider is labelled:
 * `frame` (picked from the video, no AI) and `generated` (AI) — the UI must
 * never pass either off as official art (research/01 §8.2.6, api.rs ArtSource).
 */
import { memo, useState } from 'react';
import { artSrcSet, artUrl, artWidthBucket } from '../api/client';
import type { ArtRef } from '../api/types';
import { sourceLabel } from './art-label';
import styles from './Art.module.css';
import { blurhashDataUrl } from './blurhash';

export interface ArtProps {
  art: ArtRef | null;
  /** CSS width the image is displayed at (for `sizes` and the `?w=` hint). */
  width: number;
  aspect: 'poster' | 'wide';
  /** Title for the typeset fallback and alt text. */
  title: string;
  /** Load eagerly with high priority (above-the-fold hero). */
  priority?: boolean;
  /** Skip loading the image (fast scrolling): placeholder only. */
  deferImage?: boolean;
  /** Show the source label for frame/generated art. */
  showSourceLabel?: boolean;
  className?: string;
}

export const Art = memo(function Art({ art, width, aspect, title, priority, deferImage, showSourceLabel = true, className }: ArtProps) {
  // Track which URL finished loading, so a reused instance never shows a stale image.
  const [loadedUrl, setLoadedUrl] = useState<string | null>(null);
  const placeholder = art?.blurhash ? blurhashDataUrl(art.blurhash, aspect === 'poster' ? 8 : 16, aspect === 'poster' ? 12 : 9) : null;
  const label = showSourceLabel ? sourceLabel(art) : null;
  const cls = [styles.art, aspect === 'poster' ? styles.poster : styles.wide, className].filter(Boolean).join(' ');

  if (!art) {
    // No art at all: typeset the title on a quiet field instead of a broken image.
    // An empty title is a decorative backdrop (the page shows the title itself).
    if (!title) return <div className={`${cls} ${styles.typeset}`} aria-hidden="true" />;
    return (
      <div className={`${cls} ${styles.typeset}`} role="img" aria-label={title}>
        <span>{title}</span>
      </div>
    );
  }

  return (
    <div className={cls} style={placeholder ? { backgroundImage: `url(${placeholder})` } : undefined}>
      {!deferImage && (
        <img
          key={art.url}
          src={artUrl(art, artWidthBucket(width, 1))}
          srcSet={artSrcSet(art, width)}
          sizes={`${Math.round(width)}px`}
          alt=""
          loading={priority ? 'eager' : 'lazy'}
          decoding="async"
          fetchPriority={priority ? 'high' : 'auto'}
          className={loadedUrl === art.url ? styles.loaded : undefined}
          onLoad={() => setLoadedUrl(art.url)}
        />
      )}
      {label && (
        <span className={`${styles.label} ${label.tone === 'ai' ? styles.labelAi : ''}`} title={label.long}>
          {label.tone === 'ai' && (
            <svg viewBox="0 0 12 12" aria-hidden="true">
              <path d="M6 1l1.1 3.4L10.5 5.5 7.1 6.6 6 10 4.9 6.6 1.5 5.5l3.4-1.1z" fill="currentColor" />
            </svg>
          )}
          {label.short}
        </span>
      )}
    </div>
  );
});
