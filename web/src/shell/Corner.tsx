/**
 * The corner button: who's watching (your Tailscale identity), and the doors to
 * the server's own screens. The menu grows out of the button that opened it.
 */
import { useEffect, useRef, useState } from 'react';
import { install, useInstallWay } from '../app/install';
import { Link } from '../app/Link';
import { setTheme, useTheme, type ThemeSetting } from '../app/theme';
import { reloadNow, useUpdate } from '../app/update';
import { useChanges, useServer, useTailnet } from '../api/queries';
import { Icon, type IconName } from '../components/Icon';
import { MOCK } from '../config';
import styles from './Corner.module.css';
import { refreshGlass } from './glass-light';

export function Corner({ onLenses }: { onLenses: () => void }) {
  const [open, setOpen] = useState(false);
  const btn = useRef<HTMLButtonElement>(null);
  const menu = useRef<HTMLDivElement>(null);
  const server = useServer();
  const tailnet = useTailnet();
  // Proposals waiting for a decision (Owners and Admins decide them).
  const admin = server.data?.you?.role === 'owner' || server.data?.you?.role === 'admin';
  const changes = useChanges(admin);
  const { setting } = useTheme();
  const installWay = useInstallWay();
  const [installHint, setInstallHint] = useState(false);
  const update = useUpdate();
  const you = server.data?.you ?? null;
  const name = you?.display_name || you?.login_name || 'This device';
  const initial = (you?.display_name || you?.login_name || '').trim().charAt(0).toUpperCase();
  const connected = tailnet.data?.state === 'running';

  useEffect(() => {
    if (!open) return;
    refreshGlass();
    const away = (e: PointerEvent) => {
      if (!menu.current?.contains(e.target as Node) && e.target !== btn.current && !btn.current?.contains(e.target as Node)) setOpen(false);
    };
    const esc = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.stopPropagation();
        setOpen(false);
        btn.current?.focus();
      }
    };
    window.addEventListener('pointerdown', away, true);
    window.addEventListener('keydown', esc, true);
    const t = setTimeout(() => menu.current?.querySelector<HTMLElement>('a, button')?.focus({ preventScroll: true }), 120);
    return () => {
      window.removeEventListener('pointerdown', away, true);
      window.removeEventListener('keydown', esc, true);
      clearTimeout(t);
    };
  }, [open]);

  const themes: [ThemeSetting, IconName, string][] = [
    ['system', 'monitor', 'Match system'],
    ['dark', 'moon', 'Dark'],
    ['light', 'sun', 'Light'],
  ];

  return (
    <div className={styles.corner}>
      <button
        ref={btn}
        type="button"
        className={`${styles.btn} glass glass-btn`}
        aria-haspopup="menu"
        aria-expanded={open}
        aria-label={`${name}: settings and server`}
        title="Settings and server"
        onClick={() => setOpen((o) => !o)}
      >
        {you?.profile_pic_url ? (
          <img className={styles.avatar} src={you.profile_pic_url} alt="" />
        ) : (
          <span className={styles.avatar}>{initial || <Icon name="user" />}</span>
        )}
        {tailnet.data && !connected && <span className={styles.dot} aria-hidden="true" />}
      </button>
      <div ref={menu} className={`${styles.menu} glass`} role="menu" aria-label="Settings and server" data-open={open || undefined} inert={!open}>
        <div className={styles.head}>
          <b>{name}</b>
          <small>
            {you && <span className={styles.role}>{you.role}</span>}
            {you ? ' · ' : ''}
            {server.data?.name ?? 'flux'}
            {server.data ? ` · ${server.data.version}` : ''}
          </small>
        </div>
        <Link to="/admin" role="menuitem" className={styles.item} onClick={() => setOpen(false)}>
          <Icon name="settings" />
          <span>Libraries &amp; scanning</span>
        </Link>
        <Link to="/changes" role="menuitem" className={styles.item} onClick={() => setOpen(false)}>
          <Icon name="agent" />
          <span>Agents &amp; changes</span>
          {(changes.data?.proposed ?? 0) > 0 && (
            <span className={styles.meta} data-tone="warn">
              {changes.data?.proposed} waiting
            </span>
          )}
        </Link>
        <Link to="/connect" role="menuitem" className={styles.item} onClick={() => setOpen(false)}>
          <Icon name="tailscale" />
          <span>Remote access</span>
          <span className={styles.meta} data-tone={connected ? 'ok' : 'warn'}>
            {tailnet.data ? (connected ? 'On' : 'Off') : ''}
          </span>
        </Link>
        <button
          type="button"
          role="menuitem"
          className={styles.item}
          onClick={() => {
            setOpen(false);
            onLenses();
          }}
        >
          <Icon name="sparkle" />
          <span>Lenses</span>
          <span className={styles.meta}>\</span>
        </button>
        {update.ready && (
          <button type="button" role="menuitem" className={styles.item} onClick={() => reloadNow()}>
            <Icon name="refresh" />
            <span>Reload for the new version</span>
          </button>
        )}
        {installWay === 'prompt' && (
          <button
            type="button"
            role="menuitem"
            className={styles.item}
            onClick={() => {
              setOpen(false);
              void install();
            }}
          >
            <Icon name="install" />
            <span>Install flux</span>
          </button>
        )}
        {(installWay === 'ios' || installWay === 'mac-safari') && (
          <>
            <button type="button" role="menuitem" className={styles.item} aria-expanded={installHint} onClick={() => setInstallHint((h) => !h)}>
              <Icon name="install" />
              <span>{installWay === 'ios' ? 'Add to Home Screen' : 'Add to Dock'}</span>
            </button>
            {installHint && (
              <p className={styles.hint}>
                {installWay === 'ios' ? (
                  <>
                    Tap <Icon name="share" /> Share, then Add to Home Screen.
                  </>
                ) : (
                  'In the menu bar, choose File, then Add to Dock.'
                )}
              </p>
            )}
          </>
        )}
        <div className={styles.sep} />
        <div className={styles.theme} role="radiogroup" aria-label="Theme">
          {themes.map(([value, icon, label]) => (
            <button key={value} type="button" role="radio" aria-checked={setting === value} aria-label={label} title={label} onClick={() => setTheme(value)}>
              <Icon name={icon} />
            </button>
          ))}
        </div>
        {MOCK && <p className={`edge ${styles.mock}`}>Mock server · sample data · set VITE_FLUX_API to use a real server</p>}
      </div>
    </div>
  );
}
