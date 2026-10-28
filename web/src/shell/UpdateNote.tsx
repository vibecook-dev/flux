/**
 * Notes about flux's versions, over the library:
 *
 * - "flux was updated": the server now serves a newer client than this page
 *   runs (app/update.ts). Reloading opens the new version; "Later" hides the
 *   note for this version, and the corner menu keeps offering the reload.
 * - "flux X is out" (Owners and Admins): a newer release of the server; the
 *   Libraries screen's update panel has the rest.
 */
import { useState } from 'react';
import { Link } from '../app/Link';
import { reloadNow, useUpdate } from '../app/update';
import { useServer, useUpdateInfo } from '../api/queries';
import { Icon } from '../components/Icon';
import { noteFor } from '../routes/update-state';
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

/** A release dismissed with "Later" stays quiet (this browser remembers which). */
const LATER = 'flux.update.later';

function laterRead(): string | null {
  try {
    return localStorage.getItem(LATER);
  } catch {
    return null;
  }
}

/**
 * "flux X is out", for the Owner and Admins: a newer release of the server
 * itself (flux-server's update check). Opens the Libraries screen, where the
 * update panel has the rest.
 */
export function ReleaseNote() {
  const server = useServer();
  const admin = server.data?.you?.role === 'owner' || server.data?.you?.role === 'admin';
  const info = useUpdateInfo(admin);
  const [later, setLater] = useState(laterRead);
  const version = admin ? noteFor(info.data, later) : null;
  if (!version) return null;
  const dismiss = () => {
    try {
      localStorage.setItem(LATER, version);
    } catch {
      // private mode: quiet for this page only
    }
    setLater(version);
  };
  return (
    <aside className={`${styles.note} glass`} role="status" aria-label={`flux ${version} is out`}>
      <span className={styles.mark} aria-hidden="true">
        <Icon name="install" />
      </span>
      <span className={styles.text}>
        <b>flux {version} is out</b>
        <small>See what’s new, and update.</small>
      </span>
      <Link to="/admin" className="btn btn-sm">
        Open
      </Link>
      <button type="button" className={styles.later} onClick={dismiss} aria-label="Later" title="Later">
        <Icon name="close" />
      </button>
    </aside>
  );
}
