/** A glass sheet over the library for the server's own screens (Libraries & scanning, Remote access): it rises in, and sinks away when it closes. */
import { type ReactNode, useEffect, useRef } from 'react';
import { Icon } from '../components/Icon';
import { reducedMotion } from '../lens/dom';
import { useOverlayExit } from './overlay-exit';
import styles from './Sheet.module.css';

export function Sheet({ label, onClose, children }: { label: string; onClose: () => void; children: ReactNode }) {
  const ref = useRef<HTMLDivElement>(null);
  const exit = useOverlayExit();
  useEffect(() => {
    ref.current?.focus({ preventScroll: true });
  }, []);
  useEffect(() => {
    if (!exit.leaving) return;
    const sheet = ref.current;
    if (!sheet || reducedMotion()) return exit.done();
    const end = (e: AnimationEvent) => {
      if (e.target === sheet) exit.done();
    };
    sheet.addEventListener('animationend', end);
    return () => sheet.removeEventListener('animationend', end);
  }, [exit]);
  return (
    <div className={styles.wrap} data-leaving={exit.leaving || undefined} data-restored={exit.restored || undefined}>
      <div className={styles.back} onClick={onClose} aria-hidden="true" />
      <div ref={ref} className={`${styles.sheet} glass`} role="dialog" aria-modal="true" aria-label={label} tabIndex={-1}>
        <button type="button" className={`${styles.close} glass glass-btn`} onClick={onClose} aria-label={`Close ${label}`} title="Close (esc)">
          <Icon name="close" />
        </button>
        <div className={styles.body}>{children}</div>
      </div>
    </div>
  );
}
