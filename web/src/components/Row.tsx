/**
 * A horizontal shelf of cards. Arrow keys move within the shelf; Up/Down jump
 * to the nearest card in the shelf above/below (TV remotes, keyboards).
 */
import { type ReactNode, useId, useRef } from 'react';
import { Link } from '../app/Link';
import { Icon } from './Icon';
import styles from './Row.module.css';
import { spatialKeyDown } from './spatial';

export interface RowProps {
  title: string;
  seeAllHref?: string;
  /** Shelf variant: posters (2:3) or wide cards (16:9). */
  variant?: 'poster' | 'wide';
  children: ReactNode;
}

export function Row({ title, seeAllHref, variant = 'poster', children }: RowProps) {
  const id = useId();
  const scroller = useRef<HTMLDivElement>(null);
  const page = (dir: 1 | -1) => {
    const el = scroller.current;
    if (el) el.scrollBy({ left: dir * el.clientWidth * 0.85, behavior: 'smooth' });
  };
  return (
    <section className={styles.row} aria-labelledby={id}>
      <header className={styles.header}>
        <h2 id={id} className="display-m">
          {title}
        </h2>
        {seeAllHref && (
          <Link to={seeAllHref} className={styles.seeAll}>
            See all <Icon name="forward" />
          </Link>
        )}
        <span className={styles.pager}>
          <button type="button" className={styles.pageBtn} onClick={() => page(-1)} aria-label={`Scroll ${title} back`} tabIndex={-1}>
            <Icon name="back" />
          </button>
          <button type="button" className={styles.pageBtn} onClick={() => page(1)} aria-label={`Scroll ${title} forward`} tabIndex={-1}>
            <Icon name="forward" />
          </button>
        </span>
      </header>
      <div ref={scroller} className={`${styles.scroller} ${variant === 'wide' ? styles.wide : styles.poster}`} role="list" data-nav-row onKeyDown={spatialKeyDown}>
        {children}
      </div>
    </section>
  );
}

/** A list item wrapper that marks its focusable child as a nav target. */
export function RowItem({ children }: { children: ReactNode }) {
  return (
    <div role="listitem" className={styles.item}>
      {children}
    </div>
  );
}
