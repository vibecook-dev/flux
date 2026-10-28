/**
 * Remote access onboarding (research/04 §9): connect this server to Tailscale
 * (sign-in link + QR), keep it signed in, and invite people — friends via node
 * sharing, household members via tailnet invites. No ports, no passwords.
 */
import { useCallback, useEffect, useState } from 'react';
import { useTitle } from '../app/useTitle';
import { api } from '../api/client';
import { subscribeEvents } from '../api/events';
import { formatAgo } from '../api/format';
import { keys, useInvites, useServer, useTailnet } from '../api/queries';
import { invalidateQueries, setQueryData } from '../api/query';
import type { Invite, InviteKind, Role, TailnetInfo } from '../api/types';
import { MOCK } from '../config';
import { Icon } from '../components/Icon';
import { QrCode } from '../components/QrCode';
import styles from './Connect.module.css';

type StepState = 'done' | 'active' | 'todo';

function CopyButton({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      type="button"
      className="btn btn-sm"
      onClick={() => {
        void navigator.clipboard?.writeText(text).then(() => {
          setCopied(true);
          setTimeout(() => setCopied(false), 1600);
        });
      }}
    >
      <Icon name={copied ? 'check' : 'copy'} />
      {copied ? 'Copied' : 'Copy link'}
    </button>
  );
}

function Steps({ t, invites }: { t: TailnetInfo | undefined; invites: number }) {
  const running = t?.state === 'running';
  const steps: [string, string, StepState][] = [
    ['Connect this server', running ? `On your tailnet as ${t?.dns_name ?? ''}` : 'Sign in with your Tailscale account', running ? 'done' : 'active'],
    [
      'Keep it signed in',
      t?.key_expiry ? `Key expires ${formatAgo(t.key_expiry)} — turn off key expiry` : running ? 'Key expiry is off' : 'After connecting',
      running ? (t?.key_expiry ? 'active' : 'done') : 'todo',
    ],
    ['Invite people', invites > 0 ? `${invites} invite${invites === 1 ? '' : 's'} sent` : 'Family and friends watch from anywhere', running ? (invites > 0 ? 'done' : 'active') : 'todo'],
  ];
  return (
    <ol className={styles.steps}>
      {steps.map(([title, note, state], i) => (
        <li key={title} className={styles.step} data-state={state}>
          <span className={styles.stepMark} aria-hidden="true">
            {state === 'done' ? <Icon name="check" /> : i + 1}
          </span>
          <span>
            <span className={styles.stepTitle}>{title}</span>
            <span className={styles.stepNote}>{note}</span>
          </span>
          <span className="visually-hidden">{state === 'done' ? '(done)' : state === 'active' ? '(next)' : '(later)'}</span>
        </li>
      ))}
    </ol>
  );
}

function ConnectPanel({ t, onChange }: { t: TailnetInfo; onChange: (s: TailnetInfo) => void }) {
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  // While waiting for sign-in, poll so the page moves on by itself (the server
  // also pushes `tailnet` events; polling covers a dropped event stream).
  useEffect(() => {
    if (t.state !== 'needs_login' && t.state !== 'starting' && t.state !== 'needs_approval') return;
    const id = setInterval(() => void api.tailnet().then(onChange).catch(() => undefined), 2000);
    return () => clearInterval(id);
  }, [t.state, onChange]);

  if (t.state === 'off' || t.state === 'error' || t.state === 'unavailable') {
    return (
      <div className={styles.panelBody}>
        <h2 className="display-m">Connect with Tailscale</h2>
        <p className="muted">
          flux joins your tailnet as its own device. Anyone you invite reaches it over an encrypted, direct connection — nothing is opened on your router.
        </p>
        {t.last_error && <p className={styles.error}>{t.last_error}</p>}
        {err && <p className={styles.error}>{err}</p>}
        <button
          type="button"
          className="btn btn-primary btn-lg"
          disabled={busy}
          onClick={() => {
            setBusy(true);
            setErr(null);
            api.connectTailnet().then(onChange, (e: unknown) => setErr(e instanceof Error ? e.message : 'Couldn’t start Tailscale.')).finally(() => setBusy(false));
          }}
        >
          {busy ? 'Starting…' : 'Connect with Tailscale'}
        </button>
        {t.system_tailscale && <p className={styles.hint}>Tailscale is already running on this machine. flux will still get its own identity, so you can share it without sharing the whole computer.</p>}
        <p className={styles.hint}>You’ll need a free Tailscale account (Google, Microsoft, GitHub or Apple sign-in). Personal plans cover 6 people.</p>
      </div>
    );
  }

  if ((t.state === 'needs_login' || t.state === 'starting') && t.auth_url) {
    return (
      <div className={`${styles.panelBody} ${styles.signin}`}>
        <div className={styles.qrWrap}>
          <QrCode value={t.auth_url} size={232} label="QR code for the Tailscale sign-in page" />
        </div>
        <div className={styles.signinText}>
          <h2 className="display-m">Sign in to connect</h2>
          <p className="muted">Scan with your phone’s camera, or open the sign-in page here. Use the account you want to own this server.</p>
          <div className={styles.urlRow}>
            <code className={styles.url}>{t.auth_url}</code>
          </div>
          <div className={styles.row}>
            <a className="btn btn-primary" href={t.auth_url} target="_blank" rel="noreferrer noopener">
              Open sign-in page
            </a>
            <CopyButton text={t.auth_url} />
          </div>
          <p className={styles.waiting} role="status">
            <span className={styles.pulse} aria-hidden="true" /> Waiting for you to sign in…
          </p>
          {MOCK && (
            <button type="button" className="btn btn-sm btn-ghost" onClick={() => void fetch('/__mock/tailnet/sign-in', { method: 'POST' }).then((r) => r.json() as Promise<TailnetInfo>).then(onChange)}>
              Simulate sign-in (mock server)
            </button>
          )}
        </div>
      </div>
    );
  }

  if (t.state === 'needs_approval') {
    return (
      <div className={styles.panelBody}>
        <h2 className="display-m">Approve flux in Tailscale</h2>
        <p className="muted">Your tailnet requires new devices to be approved. Approve “flux” in the admin console and this page will continue.</p>
        {t.approve_url && (
          <a className="btn btn-primary" href={t.approve_url} target="_blank" rel="noreferrer noopener">
            Open the admin console
          </a>
        )}
      </div>
    );
  }

  return null;
}

/**
 * HTTPS on the tailnet: browsers keep some features for secure pages (WebGPU,
 * so the GPU-drawn lenses), and plain HTTP from another device isn't one.
 */
function HttpsRow({ t, onChange, onRestart }: { t: TailnetInfo; onChange: (s: TailnetInfo) => void; onRestart: (running: TailnetInfo) => void }) {
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const serving = t.https && !!t.url?.startsWith('https://');
  const toggle = () => {
    setBusy(true);
    setErr(null);
    onRestart(t);
    api
      .connectTailnet(t.mode, !t.https)
      .then(onChange, (e: unknown) => setErr(e instanceof Error ? e.message : 'Couldn’t change HTTPS.'))
      .finally(() => setBusy(false));
  };
  return (
    <div className={styles.https}>
      <div className={styles.httpsText}>
        <p className={styles.httpsTitle} id="https-title">
          <Icon name="lock" /> Secure link (HTTPS)
        </p>
        <p className={styles.hint}>
          Browsers save some features for secure pages, the GPU-drawn lenses among them. flux uses your tailnet’s own certificate, so its name appears in public certificate logs. http:// links keep working.
        </p>
        {t.https && !serving && !t.https_error && (
          <p className={styles.waiting} role="status">
            <span className={styles.pulse} aria-hidden="true" /> Getting the certificate…
          </p>
        )}
        {t.https_error && (
          <p className={styles.notice}>
            <Icon name="warning" />
            <span>{t.https_error}</span>
          </p>
        )}
        {err && <p className={styles.error}>{err}</p>}
      </div>
      <button type="button" role="switch" aria-checked={t.https} aria-labelledby="https-title" className={styles.switch} disabled={busy} onClick={toggle} />
    </div>
  );
}

function ConnectedPanel({ t, onChange, onRestart }: { t: TailnetInfo; onChange: (s: TailnetInfo) => void; onRestart: (running: TailnetInfo) => void }) {
  const url = t.url ?? `http://${t.dns_name ?? 'flux'}`;
  return (
    <div className={`${styles.panelBody} ${styles.connected}`}>
      <div className={styles.connectedText}>
        <p className={styles.okLine}>
          <span className={styles.okDot} aria-hidden="true" /> Connected
        </p>
        <h2 className="display-m">{t.dns_name}</h2>
        <p className="muted">
          Signed in by <strong>{t.owner_login}</strong> — that account owns this server. Open flux on any device signed in to the same tailnet:
        </p>
        <div className={styles.row}>
          <code className={styles.url}>{url}</code>
          <CopyButton text={url} />
        </div>
        {t.key_expiry && (
          <p className={styles.notice}>
            <Icon name="warning" />
            <span>
              Tailscale signs devices out after 180 days by default. The key for flux expires {formatAgo(t.key_expiry)}.{' '}
              <a href="https://login.tailscale.com/admin/machines" target="_blank" rel="noreferrer noopener">
                Turn off key expiry for flux
              </a>{' '}
              so remote viewers never lose access.
            </span>
          </p>
        )}
      </div>
      <div className={styles.qrSmall}>
        <QrCode value={url} size={148} label={`QR code to open ${url}`} />
        <span className="edge">Open on your phone</span>
      </div>
      <HttpsRow t={t} onChange={onChange} onRestart={onRestart} />
    </div>
  );
}

const ROLE_HELP: Record<Exclude<Role, 'owner'>, string> = {
  viewer: 'Watches everything, keeps their own progress',
  kid: 'Only libraries you mark for kids',
  guest: 'Watches; can’t see other people’s activity',
  admin: 'Can manage libraries and invites',
};

function InvitePanel({ invites }: { invites: Invite[] }) {
  const [kind, setKind] = useState<InviteKind>('share');
  const [role, setRole] = useState<Exclude<Role, 'owner'>>('viewer');
  const [note, setNote] = useState('');
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [created, setCreated] = useState<Invite | null>(null);

  const create = () => {
    setBusy(true);
    setErr(null);
    api
      .createInvite({ kind, role, note: note.trim() || null })
      .then(
        (inv) => {
          setCreated(inv);
          setNote('');
          setQueryData<Invite[]>(keys.invites, (prev) => [inv, ...(prev ?? [])]);
        },
        (e: unknown) => setErr(e instanceof Error ? e.message : 'Couldn’t create the invite.'),
      )
      .finally(() => setBusy(false));
  };

  return (
    <section className={styles.panel} aria-labelledby="invite">
      <div className={styles.panelBody}>
        <h2 id="invite" className="display-m">
          Invite family
        </h2>
        <fieldset className={styles.kinds}>
          <legend className="visually-hidden">Who are you inviting?</legend>
          <label className={styles.kind}>
            <input type="radio" name="kind" checked={kind === 'share'} onChange={() => setKind('share')} />
            <span className={styles.kindTitle}>Someone in another home</span>
            <span className={styles.kindNote}>Shares only flux. They sign in with their own free Tailscale account; nothing else on your network is visible to them.</span>
          </label>
          <label className={styles.kind}>
            <input type="radio" name="kind" checked={kind === 'household'} onChange={() => setKind('household')} />
            <span className={styles.kindTitle}>Someone in your household</span>
            <span className={styles.kindNote}>Joins your tailnet, so their devices can reach flux at home and away. Up to 6 people on the free plan.</span>
          </label>
        </fieldset>
        <div className={styles.formRow}>
          <label className="field">
            <span>What they can do</span>
            <select className="select" value={role} onChange={(e) => setRole(e.target.value as Exclude<Role, 'owner'>)}>
              <option value="viewer">Viewer</option>
              <option value="kid">Kid</option>
              <option value="guest">Guest</option>
              <option value="admin">Admin</option>
            </select>
          </label>
          <label className="field">
            <span>Note (optional)</span>
            <input className="input" value={note} onChange={(e) => setNote(e.target.value)} placeholder="For Mum’s iPad" maxLength={80} />
          </label>
        </div>
        <p className={styles.hint}>{ROLE_HELP[role]}.</p>
        {err && <p className={styles.error}>{err}</p>}
        <button type="button" className="btn btn-primary" onClick={create} disabled={busy}>
          <Icon name="link" />
          {busy ? 'Creating…' : 'Create invite link'}
        </button>

        {created && (
          <div className={styles.created} role="status">
            <QrCode value={created.url} size={132} label="QR code for the invite link" />
            <div className={styles.createdText}>
              <p className={styles.createdTitle}>Invite link ready</p>
              <code className={styles.url}>{created.url}</code>
              <p className={styles.hint}>
                Send it however you like. They install Tailscale, open the link, then open flux — no password from you. Expires {created.expires_at ? formatAgo(created.expires_at) : 'never'}.
              </p>
              <CopyButton text={created.url} />
            </div>
          </div>
        )}

        {invites.length > 0 && (
          <>
            <h3 className={styles.subhead}>Invites</h3>
            <ul className={styles.inviteList}>
              {invites.map((inv) => (
                <li key={inv.id}>
                  <span>
                    <strong>{inv.role.charAt(0).toUpperCase() + inv.role.slice(1)}</strong> · {inv.kind === 'share' ? 'another home' : 'household'}
                    {inv.note ? ` · ${inv.note}` : ''}
                  </span>
                  <span className="edge">
                    {inv.accepted > 0 ? `accepted ×${inv.accepted}` : 'not used yet'} · {formatAgo(inv.created_at)}
                  </span>
                </li>
              ))}
            </ul>
          </>
        )}
      </div>
    </section>
  );
}

export default function Connect() {
  useTitle('Remote access');
  const tailnet = useTailnet();
  const invites = useInvites();
  const server = useServer();
  const t = tailnet.data;
  // Only the owner and admins change remote access (the server refuses others).
  const admin = server.data?.you?.role === 'owner' || server.data?.you?.role === 'admin';
  // Switching HTTPS restarts the tailnet listener, which reports `starting`
  // for a moment: keep showing the server as it was connected meanwhile.
  const [held, setHeld] = useState<TailnetInfo | null>(null);
  const connected = t?.state === 'running' ? t : t?.state === 'starting' && held ? { ...held, https: t.https, https_error: null } : null;
  const onChange = useCallback((s: TailnetInfo) => {
    setQueryData(keys.tailnet, s);
    if (s.state === 'running') invalidateQueries(keys.server);
  }, []);
  // The server pushes every state change (login URL ready, running, failed).
  useEffect(() => subscribeEvents((e) => e.type === 'tailnet' && onChange(e.info)), [onChange]);

  return (
    <div className={styles.page}>
      <header className={styles.header}>
        <h1 className="display-l">Remote access</h1>
        <p className={styles.lede}>Watch your library from anywhere you’re signed in to Tailscale. No ports to open, no relay to pay for, no passwords to share.</p>
      </header>
      <div className={styles.layout}>
        <aside>
          <Steps t={t} invites={invites.data?.length ?? 0} />
          {server.data && (
            <p className={`edge ${styles.serverLine}`}>
              {server.data.name} · {server.data.version}
            </p>
          )}
        </aside>
        <div className={styles.content}>
          {server.data && !admin ? (
            <section className={styles.panel} aria-label="Server connection">
              <div className={styles.panelBody}>
                <h2 className="display-m">{t?.state === 'running' ? 'Remote access is on' : 'Remote access is off'}</h2>
                <p className={styles.hint}>
                  {t?.state === 'running'
                    ? `Devices signed in to the tailnet reach flux at ${t.url ?? t.dns_name ?? 'its tailnet address'}.`
                    : 'flux’s owner turns it on, here, from the computer flux runs on or a browser an owner link made an owner.'}
                </p>
              </div>
            </section>
          ) : !t ? (
            <div className={`skeleton ${styles.panelSkeleton}`} />
          ) : (
            <section className={styles.panel} aria-label="Server connection">
              {connected ? <ConnectedPanel t={connected} onChange={onChange} onRestart={setHeld} /> : <ConnectPanel t={t} onChange={onChange} />}
            </section>
          )}
          {connected && admin && <InvitePanel invites={invites.data ?? []} />}
        </div>
      </div>
    </div>
  );
}
