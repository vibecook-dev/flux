/** A glass sheet over the library for the server's own screens (Libraries & scanning, Remote access). */
import { type ReactNode, useEffect, useRef } from 'react';
import { Icon } from '../components/Icon';
import styles from './Sheet.module.css';

export function Sheet({ label, onClose, children }: { label: string; onClose: () => void; children: ReactNode }) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    ref.current?.focus({ preventScroll: true });
  }, []);
  return (
    <div className={styles.wrap}>
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
