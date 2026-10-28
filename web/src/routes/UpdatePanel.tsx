/**
 * flux's own updates, on the Libraries screen (Owners and Admins): what is
 * out and what's in it, "Update now" for an install that updates itself (or
 * the command for one that doesn't), the steps of an update as it runs, and
 * the daily check's switch. The server does the work (flux-server's
 * `update`); `update` events keep this current.
 */
import { useEffect, useState } from 'react';
import { reloadNow, useUpdate } from '../app/update';
import { api } from '../api/client';
import { watchEventsState } from '../api/events';
import { formatAgo } from '../api/format';
import { keys, useUpdateInfo } from '../api/queries';
import { invalidateQueries, setQueryData } from '../api/query';
import type { UpdateInfo } from '../api/types';
import { Icon } from '../components/Icon';
import styles from './UpdatePanel.module.css';
import { checkedLine, downloaded, headline, stepLine, updating } from './update-state';

/** The version an update started from this page goes to (kept while the panel remounts). */
let updatingTo: string | null = null;

const message = (e: unknown, fallback: string) => (e instanceof Error ? e.message : fallback);

export function UpdatePanel() {
  const q = useUpdateInfo();
  const u = q.data;
  const page = useUpdate();
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

  // Started here: once flux is back as the new version, open it.
  useEffect(() => {
    if (!updatingTo) return;
    if (page.ready || (u?.phase === 'idle' && u.current === updatingTo)) {
      updatingTo = null;
      reloadNow();
    }
  }, [page.ready, u]);
  // While flux restarts, ask again each time the event stream comes back.
  useEffect(
    () =>
      watchEventsState((s) => {
        if (s === 'open' && updatingTo) invalidateQueries(keys.update);
      }),
    [],
  );

  if (!u) {
    return q.error ? (
      <section className={styles.panel}>
        <p className={styles.error}>{message(q.error, 'flux didn’t say how it updates.')}</p>
      </section>
    ) : null;
  }

  const take = (p: Promise<UpdateInfo>, fallback: string, then?: (info: UpdateInfo) => void) => {
    setBusy(true);
    setErr(null);
    p.then(
      (info) => {
        setQueryData(keys.update, info);
        then?.(info);
      },
      (e: unknown) => setErr(message(e, fallback)),
    ).finally(() => setBusy(false));
  };
  const check = () => take(api.checkForUpdate(), 'The check for updates failed.');
  const apply = () =>
    take(api.applyUpdate(), 'The update didn’t start.', (info) => {
      updatingTo = info.available?.version ?? null;
    });
  const toggle = () => take(api.setUpdateCheck(!u.check), 'Couldn’t change the setting.');
  const copy = (text: string) => {
    void navigator.clipboard?.writeText(text).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 1600);
    });
  };

  const running = updating(u.phase);
  const step = stepLine(u);
  const share = downloaded(u);
  const release = u.available;
  return (
    <section className={styles.panel} id="update" data-phase={u.phase} aria-labelledby="update-title">
      <header className={styles.head}>
        <div>
          <h2 id="update-title" className="display-m">
            {headline(u)}
          </h2>
          <p className="edge">{checkedLine(u, formatAgo)}</p>
        </div>
        <button type="button" className="btn btn-sm" onClick={check} disabled={busy || running || u.phase === 'checking'}>
          <Icon name="refresh" />
          {u.phase === 'checking' ? 'Checking…' : 'Check now'}
        </button>
      </header>

      {release && (
        <div className={styles.release}>
          <p className={styles.meta}>
            {release.published_at ? `Released ${formatAgo(release.published_at)} · ` : ''}
            <a className={styles.link} href={release.url} target="_blank" rel="noreferrer noopener">
              Release notes
            </a>
          </p>
          {release.highlights.length > 0 && (
            <ul className={styles.highlights}>
              {release.highlights.map((h) => (
                <li key={h}>{h}</li>
              ))}
            </ul>
          )}
        </div>
      )}

      {running && (
        <div className={styles.progress} role="status">
          <div className={styles.bar} aria-hidden="true">
            <span style={{ width: share === null ? '100%' : `${share * 100}%` }} data-indeterminate={share === null || undefined} />
          </div>
          <p>{step}</p>
        </div>
      )}

      {!running && release && u.install.can_apply && (
        <div className={styles.actions}>
          <button type="button" className="btn btn-primary" onClick={apply} disabled={busy}>
            Update now
          </button>
          <p className={styles.hint}>flux restarts as {release.version}; anything playing stops for a moment.</p>
        </div>
      )}

      {!running && release && !u.install.can_apply && (
        <div className={styles.manual}>
          {u.install.why_not && <p>{u.install.why_not}</p>}
          {u.install.command && (
            <div className={styles.command}>
              <code className="mono">{u.install.command}</code>
              <button type="button" className="btn btn-sm" onClick={() => copy(u.install.command ?? '')}>
                <Icon name={copied ? 'check' : 'copy'} />
                {copied ? 'Copied' : 'Copy'}
              </button>
            </div>
          )}
        </div>
      )}

      {u.error && (
        <p className={u.phase === 'failed' ? styles.error : styles.warn} role={u.phase === 'failed' ? 'alert' : undefined}>
          {u.phase === 'failed' ? `The update stopped, and flux carries on as it was: ${u.error}` : u.error}
        </p>
      )}
      {err && (
        <p className={styles.error} role="alert">
          {err}
        </p>
      )}

      <div className={styles.setting}>
        <div>
          <p className={styles.settingTitle} id="update-check-title">
            Check for updates daily
          </p>
          <p className={styles.hint}>
            {u.check_fixed ? (
              <>
                Set by the server’s configuration (<code className="mono">--update-check</code>).
              </>
            ) : (
              'flux asks GitHub once a day. GitHub sees this server’s address and flux’s version, nothing about your library.'
            )}
          </p>
        </div>
        <button
          type="button"
          role="switch"
          aria-checked={u.check}
          aria-labelledby="update-check-title"
          className={styles.switch}
          disabled={u.check_fixed || busy}
          onClick={toggle}
        />
      </div>

      {u.previous && !running && (
        <p className={styles.hint}>
          flux {u.previous} is kept: <code className="mono">flux-server update --rollback</code> goes back to it.
        </p>
      )}
    </section>
  );
}
