/**
 * "flux was updated": the server now serves a newer client than this page runs
 * (app/update.ts). Reloading opens the new version; "Later" hides the note for
 * this version, and the corner menu keeps offering the reload.
 */
import { useState } from 'react';
import { reloadNow, useUpdate } from '../app/update';
import { Icon } from '../components/Icon';
import styles from './UpdateNote.module.css';

export function UpdateNote() {
  const update = useUpdate();
  const [later, setLater] = useState<string | null>(null);
  if (!update.ready || later === update.build) return null;
  return (
    <aside className={`${styles.note} glass`} role="status" aria-label="flux was updated">
      <span className={styles.mark} aria-hidden="true">
        <Icon name="sparkle" />
      </span>
      <span className={styles.text}>
        <b>flux was updated</b>
        <small>Reload to use the new version.</small>
      </span>
      <button type="button" className="btn btn-primary btn-sm" onClick={() => reloadNow()}>
        Reload
      </button>
      <button type="button" className={styles.later} onClick={() => setLater(update.build)} aria-label="Later" title="Later">
        <Icon name="close" />
      </button>
    </aside>
  );
}
