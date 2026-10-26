/**
 * The lens bar: your pinned lenses, a glass drop on the current one that flows to
 * the next, the door to all lenses, and one control the current lens lends it.
 * It shrinks to the current lens while you scroll down and comes back when you
 * scroll up.
 */
import { useLayoutEffect, useRef } from 'react';
import { LENS_ICONS } from '../lens/registry';
import type { LensDef } from '../lens/types';
import { refreshGlass } from './glass-light';
import styles from './LensBar.module.css';

export interface LensBarProps {
  lenses: readonly LensDef[];
  current: string;
  mini: boolean;
  accessory: HTMLElement | null;
  onPick: (id: string) => void;
  onMore: () => void;
}

function Svg({ markup }: { markup: string }) {
  // Lens icons are constant SVG strings from the registry.
  return <span className={styles.icon} dangerouslySetInnerHTML={{ __html: markup }} />;
}

export function LensBar({ lenses, current, mini, accessory, onPick, onMore }: LensBarProps) {
  const bar = useRef<HTMLDivElement>(null);
  const drop = useRef<HTMLSpanElement>(null);
  const acc = useRef<HTMLDivElement>(null);
  const lastX = useRef<number | null>(null);

  // The drop follows the current lens (after labels change width in mini mode too).
  useLayoutEffect(() => {
    const place = () => {
      const b = bar.current?.querySelector<HTMLElement>(`[data-lens="${CSS.escape(current)}"]`);
      const d = drop.current;
      if (!d) return;
      if (!b) {
        d.style.opacity = '0';
        return;
      }
      d.style.opacity = '1';
      const x = b.offsetLeft;
      if (lastX.current !== null && Math.abs(lastX.current - x) > 4 && !matchMedia('(prefers-reduced-motion: reduce)').matches) {
        // A drop of liquid stretches as it moves, then settles.
        d.animate([{ scale: '1 1' }, { scale: '1.22 0.92', offset: 0.35 }, { scale: '1 1' }], { duration: 560, easing: 'ease-out' });
      }
      lastX.current = x;
      d.style.setProperty('--dx', `${x}px`);
      d.style.setProperty('--dw', `${b.offsetWidth}px`);
      refreshGlass();
    };
    place();
    const t = setTimeout(place, 640);
    return () => clearTimeout(t);
  }, [current, mini, lenses]);

  // The accessory is a DOM node the lens built; mount it into its glass slot.
  useLayoutEffect(() => {
    const slot = acc.current;
    if (!slot) return;
    slot.replaceChildren(...(accessory ? [accessory] : []));
    refreshGlass();
  }, [accessory]);

  // When the bar and the control don't fit side by side, the control sits above the bar (as on iOS).
  useLayoutEffect(() => {
    const row = bar.current?.parentElement;
    if (!row || !bar.current) return;
    const fit = () => {
      row.classList.remove(styles.stack);
      const need = (bar.current?.offsetWidth ?? 0) + (accessory ? (acc.current?.offsetWidth ?? 0) + 10 : 0);
      row.classList.toggle(styles.stack, need > window.innerWidth - 32);
    };
    fit();
    window.addEventListener('resize', fit);
    return () => window.removeEventListener('resize', fit);
  }, [accessory, lenses, mini]);

  return (
    <>
      <div ref={bar} className={`${styles.bar} glass`} role="tablist" aria-label="Lenses" data-mini={mini || undefined}>
        <span ref={drop} className={styles.drop} aria-hidden="true" />
        {lenses.map((l) => (
          <button key={l.id} type="button" role="tab" className={styles.item} data-lens={l.id} aria-selected={l.id === current} title={l.name} onClick={() => onPick(l.id)}>
            <Svg markup={l.icon} />
            <span className={styles.name}>{l.name}</span>
          </button>
        ))}
        <span className={styles.sep} aria-hidden="true" />
        <button type="button" className={styles.more} onClick={onMore} aria-label="All lenses" title="All lenses (\)">
          <Svg markup={LENS_ICONS.more} />
        </button>
      </div>
      <div ref={acc} className={`${styles.accessory} glass`} hidden={!accessory} />
    </>
  );
}
