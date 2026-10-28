/**
 * An owner link (`/own#<token>`; flux-server's `devices`): this browser
 * becomes an owner device. The token comes out of the address bar (and the
 * history) at once, and the link is used only when the person asks: a link
 * opened by something that only looks (a preview, a scanner) stays good.
 * Another link pasted into the address bar changes only the fragment, so the
 * page takes it from `hashchange`.
 *
 * flux answers 409 for a browser that can already set it up (the link stays
 * unused), 403 for one whose role comes from elsewhere (a Tailscale login,
 * `--localhost-role`), and 410 for a used or expired link. The device's key
 * goes to the page's own storage (`api/device-key`).
 */
import { useEffect, useState } from 'react';
import { Link } from '../app/Link';
import { useTitle } from '../app/useTitle';
import { api, FluxApiError } from '../api/client';
import { setDeviceKey } from '../api/device-key';
import { keys, useLibraries, useServer } from '../api/queries';
import { invalidateQueries } from '../api/query';
import type { ClaimedOwner, DeviceInfo } from '../api/types';
import { Icon } from '../components/Icon';
import { OwnerLinkHelp } from './OwnerLinkHelp';
import styles from './Setup.module.css';
import { deviceName } from './setup-state';

/** Each token's claim: one request at a time, however often React renders. */
const claims = new Map<string, Promise<ClaimedOwner>>();

function claim(token: string): Promise<ClaimedOwner> {
  let p = claims.get(token);
  if (!p) {
    p = api.claimOwner({ token, name: thisDevice() });
    claims.set(token, p);
    // A failure the person can retry (the network, a full disk) isn't kept.
    p.catch(() => claims.delete(token));
  }
  return p;
}

function thisDevice(): string {
  return deviceName(navigator.userAgent, navigator.maxTouchPoints ?? 0);
}

/** The token in the address bar, taken out of it and the history. */
function takeToken(): string | null {
  const token = location.hash.replace(/^#/, '');
  if (location.hash) history.replaceState(history.state, '', location.pathname + location.search);
  return token || null;
}

/** The token this page has (kept: taking it empties the address bar, and React may render twice). */
let current: string | null = null;

function useToken(): string | null {
  const [token, setToken] = useState(() => {
    current = takeToken() ?? current;
    return current;
  });
  useEffect(() => {
    const onHash = () => {
      const next = takeToken();
      if (next) {
        current = next;
        setToken(next);
      }
    };
    window.addEventListener('hashchange', onHash);
    return () => window.removeEventListener('hashchange', onHash);
  }, []);
  return token;
}

type State = 'ready' | 'claiming' | 'done' | 'already' | 'elsewhere' | 'used' | 'failed' | 'missing';

interface Result {
  token: string;
  state: State;
  /** The server's sentence, for the states that show one. */
  said: string | null;
  device: DeviceInfo | null;
}

const TITLES: Record<State, string> = {
  ready: 'Make this browser an owner?',
  claiming: 'One moment…',
  done: 'This browser is an owner',
  already: 'This browser can already set flux up',
  elsewhere: 'An owner link can’t change this',
  used: 'This owner link doesn’t work',
  failed: 'That didn’t work',
  missing: 'This owner link doesn’t work',
};

/** What a failed claim means for this page. */
function resultOf(token: string, e: unknown): Result {
  const status = e instanceof FluxApiError ? e.status : 0;
  const state: State = status === 409 ? 'already' : status === 410 ? 'used' : status === 403 && e instanceof FluxApiError && e.said ? 'elsewhere' : 'failed';
  const said = state === 'elsewhere' && e instanceof FluxApiError ? e.said : e instanceof Error ? e.message : 'The owner link didn’t work.';
  return { token, state, said, device: null };
}

export default function Own() {
  useTitle('Owner link');
  const libs = useLibraries();
  const server = useServer();
  const token = useToken();
  const [result, setResult] = useState<Result | null>(null);
  const [asked, setAsked] = useState<string | null>(null);

  const go = () => {
    if (!token) return;
    const t = token;
    setAsked(t);
    claim(t).then(
      ({ device, key }) => {
        setDeviceKey(key);
        setResult({ token: t, state: 'done', said: null, device });
        // Who this browser is changed: everything that depends on it.
        invalidateQueries(keys.server);
        invalidateQueries(keys.libraries);
        invalidateQueries(keys.update);
        invalidateQueries(keys.devices);
      },
      (e: unknown) => setResult(resultOf(t, e)),
    );
  };

  // A browser that is the owner (or an admin) already needs no link: say so
  // without using it.
  const role = server.data?.you?.role;
  const placed = role === 'owner' || role === 'admin';
  const knowing = server.data === undefined && server.status !== 'error';
  const state: State = !token
    ? 'missing'
    : result?.token === token
      ? result.state
      : asked === token || knowing
        ? 'claiming'
        : placed
          ? 'already'
          : 'ready';
  const said = result?.token === token ? result.said : null;
  const empty = libs.data !== undefined && libs.data.length === 0;
  const lede: Partial<Record<State, string | null>> = {
    ready: `It can then set flux up and change its settings, as “${thisDevice()}”, until it’s removed on the Libraries screen. The link works once.`,
    elsewhere: said,
    used: 'It has been used already, or it’s more than a day old.',
    failed: said,
    missing: 'This address has no owner link in it.',
  };
  const onward = (
    <Link to={empty ? '/setup' : '/'} className="btn btn-primary">
      {empty ? 'Set up flux' : 'Go to your library'}
    </Link>
  );
  return (
    <div className={styles.page}>
      <header className={styles.header}>
        <h1 className="display-l">{TITLES[state]}</h1>
        {lede[state] && <p className={styles.lede}>{lede[state]}</p>}
      </header>
      {(state === 'ready' || state === 'claiming') && (
        <div className={styles.actions}>
          <button type="button" className="btn btn-primary" onClick={go} disabled={state === 'claiming'}>
            {state === 'claiming' ? 'One moment…' : 'Make this browser an owner'}
          </button>
        </div>
      )}
      {(state === 'done' || state === 'already') && (
        <div className={styles.panel}>
          <div className={styles.own}>
            <p className={styles.ok}>
              <Icon name="check" />{' '}
              {state === 'done'
                ? `flux knows this browser as ${result?.device?.name ?? 'an owner device'}, until it’s removed on the Libraries screen.`
                : 'The link wasn’t used, so it still works on another device.'}
            </p>
            {onward}
          </div>
        </div>
      )}
      {state === 'failed' && (
        <div className={styles.actions}>
          <button type="button" className="btn" onClick={go}>
            <Icon name="refresh" /> Try again
          </button>
        </div>
      )}
      {(state === 'used' || state === 'missing') && <OwnerLinkHelp />}
    </div>
  );
}
