/**
 * Owner devices, on the Libraries screen (Owners and Admins): browsers an
 * owner link made owners, known by the key each sends (flux-server's
 * `devices`). Each can be removed, which signs it out at once; "Add a
 * device" makes a link, with a QR code for a phone on this network.
 */
import { useState } from 'react';
import { api } from '../api/client';
import { setDeviceKey } from '../api/device-key';
import { formatAgo } from '../api/format';
import { keys, useDevices } from '../api/queries';
import { invalidateQueries, setQueryData } from '../api/query';
import type { DeviceInfo, OwnerLink } from '../api/types';
import { Icon } from '../components/Icon';
import { QrCode } from '../components/QrCode';
import styles from './OwnerDevices.module.css';
import { isLoopback, linkUrl } from './setup-state';

export function OwnerDevices() {
  const devices = useDevices();
  const [link, setLink] = useState<OwnerLink | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [copied, setCopied] = useState(false);

  const make = () => {
    setBusy(true);
    setErr(null);
    api
      .ownerLink()
      .then(setLink, (e: unknown) => setErr(e instanceof Error ? e.message : 'Couldn’t make a link.'))
      .finally(() => setBusy(false));
  };
  const remove = (d: DeviceInfo) => {
    setErr(null);
    api.removeDevice(d.id).then(
      () => {
        setQueryData<DeviceInfo[]>(keys.devices, (prev) => (prev ?? []).filter((x) => x.id !== d.id));
        // Removing this very browser signs it out.
        if (d.this_device) {
          setDeviceKey(null);
          invalidateQueries(keys.server);
          invalidateQueries(keys.update);
        }
      },
      (e: unknown) => setErr(e instanceof Error ? e.message : 'Couldn’t remove it.'),
    );
  };
  const url = link ? linkUrl(link, location.origin, location.hostname) : null;
  const onlyHere = url !== null && isLoopback(new URL(url).hostname);

  return (
    <section className={styles.panel} aria-labelledby="devices-title">
      <header className={styles.head}>
        <div>
          <h2 id="devices-title" className="display-m">
            Owner devices
          </h2>
          <p className="edge">Browsers an owner link made owners</p>
        </div>
        <button type="button" className="btn btn-sm" onClick={make} disabled={busy}>
          <Icon name="plus" /> Add a device
        </button>
      </header>
      <p className={styles.hint}>
        flux knows its owner by where a call comes from: this computer, or a Tailscale login. Elsewhere on the network (or from a Docker host) it can’t tell, so an owner link makes a browser an owner. Over
        plain HTTP, someone watching the network could copy that browser’s key: remove devices you don’t use.
      </p>
      {link && url && (
        <div className={styles.link}>
          <div className={styles.qr}>
            <QrCode value={url} size={140} label="QR code for the owner link" />
          </div>
          <div className={styles.linkText}>
            <p>
              Open this on the device that should be an owner. It works once, until {new Date(link.expires_at).toLocaleString()}.
              {onlyHere && ' On another device, put this computer’s address in place of localhost.'}
            </p>
            <code className="mono">{url}</code>
            <button
              type="button"
              className="btn btn-sm"
              onClick={() => {
                void navigator.clipboard?.writeText(url).then(() => {
                  setCopied(true);
                  setTimeout(() => setCopied(false), 1600);
                });
              }}
            >
              <Icon name={copied ? 'check' : 'copy'} />
              {copied ? 'Copied' : 'Copy link'}
            </button>
          </div>
        </div>
      )}
      {devices.data && devices.data.length > 0 && (
        <ul className={styles.list}>
          {devices.data.map((d) => (
            <li key={d.id}>
              <span>
                <span className={styles.name}>
                  <b>{d.name}</b>
                  {d.this_device && <span className={styles.this}>this browser</span>}
                </span>
                <small>
                  {d.role === 'admin' ? 'an admin · ' : ''}added {formatAgo(d.created_at)} with a link from {d.added_by} · last seen {formatAgo(d.last_seen_at)}
                </small>
              </span>
              <button type="button" className="btn btn-ghost btn-sm" onClick={() => remove(d)}>
                Remove
              </button>
            </li>
          ))}
        </ul>
      )}
      {err && (
        <p className={styles.error} role="alert">
          {err}
        </p>
      )}
    </section>
  );
}
