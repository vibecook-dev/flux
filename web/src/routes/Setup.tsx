/**
 * Setting flux up (Owners and Admins): the films and shows (a folder browser
 * on the server, which suggests what kind of library a folder makes), then
 * playback (what ffmpeg can do there, and how to get it), then the other
 * devices (this network, anywhere with Tailscale, the installed app). The
 * empty library opens it; it works as well later, to add another folder.
 */
import { type FormEvent, useEffect, useState } from 'react';
import { installWay } from '../app/install';
import { Link } from '../app/Link';
import { navigate } from '../app/router';
import { useTitle } from '../app/useTitle';
import { api } from '../api/client';
import { formatCount } from '../api/format';
import { keys, useAddresses, useLibraries, useMediaTools, useServer, useTailnet, useUpdateInfo } from '../api/queries';
import { invalidateQueries, setQueryData } from '../api/query';
import type { BrowseResult, Library, LibraryKind } from '../api/types';
import { Icon } from '../components/Icon';
import { QrCode } from '../components/QrCode';
import styles from './Setup.module.css';
import { Command, OwnerLinkHelp } from './OwnerLinkHelp';
import { crumbs, encoderLine, ffmpegFix, KIND_CHOICES, nameFor, summaryLine, videosLabel } from './setup-state';

type Step = 'media' | 'playback' | 'devices';
const STEPS: [Step, string][] = [
  ['media', 'Your films and shows'],
  ['playback', 'Playback'],
  ['devices', 'Other devices'],
];

const message = (e: unknown, fallback: string) => (e instanceof Error ? e.message : fallback);

export default function Setup() {
  useTitle('Set up flux');
  const server = useServer();
  const admin = server.data?.you?.role === 'owner' || server.data?.you?.role === 'admin';
  const libs = useLibraries();
  const tools = useMediaTools(admin);
  const [step, setStep] = useState<Step>('media');

  if (server.data && !admin) {
    return (
      <div className={styles.page}>
        <header className={styles.header}>
          <h1 className="display-l">Set up flux</h1>
          <p className={styles.lede}>flux is set up by its owner. On the computer it runs on, open it there; from another computer, use an owner link.</p>
        </header>
        <OwnerLinkHelp />
      </div>
    );
  }

  const count = libs.data?.length ?? 0;
  const notes: Record<Step, string> = {
    media: count ? `${count} ${count === 1 ? 'library' : 'libraries'}` : 'Pick a folder on the server',
    playback: !tools.data ? 'ffmpeg, for what browsers can’t play' : tools.data.ok ? 'ffmpeg is ready' : 'ffmpeg is missing',
    devices: 'This network, and anywhere',
  };
  const at = STEPS.findIndex(([s]) => s === step);
  // A step is done when what it sets up is there, wherever you are in the list.
  const done: Record<Step, boolean> = { media: count > 0, playback: tools.data?.ok === true, devices: false };
  return (
    <div className={styles.page}>
      <header className={styles.header}>
        <h1 className="display-l">Set up flux</h1>
        <p className={styles.lede}>Three steps: your films and shows, playback, and the devices you watch on. You can come back here any time.</p>
      </header>
      <div className={styles.layout}>
        <ol className={styles.steps}>
          {STEPS.map(([s, title], i) => (
            <li key={s} className={styles.step} data-state={i === at ? 'active' : done[s] ? 'done' : 'todo'}>
              <button type="button" onClick={() => setStep(s)} aria-current={s === step ? 'step' : undefined}>
                <span className={styles.stepMark} aria-hidden="true">
                  {i !== at && done[s] ? <Icon name="check" /> : i + 1}
                </span>
                <span>
                  <span className={styles.stepTitle}>{title}</span>
                  <span className={styles.stepNote}>{notes[s]}</span>
                </span>
              </button>
            </li>
          ))}
        </ol>
        <section className={styles.panel} aria-label={STEPS[at][1]}>
          {step === 'media' && <MediaStep libraries={libs.data ?? []} onNext={() => setStep('playback')} />}
          {step === 'playback' && <PlaybackStep onNext={() => setStep('devices')} />}
          {step === 'devices' && <DevicesStep />}
        </section>
      </div>
    </div>
  );
}

function MediaStep({ libraries, onNext }: { libraries: Library[]; onNext: () => void }) {
  return (
    <div className={styles.body}>
      <h2 className="display-m">Your films and shows</h2>
      <p className={styles.hint}>Pick the folder that holds them, on the computer flux runs on. flux only reads the files; it never changes or moves them.</p>
      <FolderPicker />
      {libraries.length > 0 && (
        <div className={styles.added}>
          <h3>In flux</h3>
          <ul>
            {libraries.map((l) => (
              <li key={l.id}>
                <Icon name={l.kind === 'shows' ? 'tv' : 'film'} />
                <span>
                  <b>{l.name}</b>
                  <small>
                    {l.roots.map((r) => r.path).join(', ')} · {l.scan.state === 'scanning' ? `reading, ${formatCount(l.scan.files_seen)} files so far` : `${formatCount(l.item_count)} titles`}
                  </small>
                </span>
              </li>
            ))}
          </ul>
          <button type="button" className="btn btn-primary" onClick={onNext}>
            Next: playback
          </button>
        </div>
      )}
    </div>
  );
}

/** Folders on the server, one level at a time, and adding the one shown as a library. */
function FolderPicker() {
  const [path, setPath] = useState<string | null>(null);
  // What the server said, and for which folder (another folder's answer is still loading).
  const [shown, setShown] = useState<{ at: string | null; res: BrowseResult } | null>(null);
  const [failed, setFailed] = useState<{ at: string | null; why: string } | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [typed, setTyped] = useState('');
  const [name, setName] = useState('');
  const [kind, setKind] = useState<LibraryKind>('movies');
  const [adding, setAdding] = useState(false);
  const [added, setAdded] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    api.browse(path).then(
      (r) => {
        if (!live) return;
        setShown({ at: path, res: r });
        setName(r.path ? nameFor(r.path) : '');
        setKind(r.here?.kind ?? 'movies');
      },
      (e: unknown) => {
        if (live) setFailed({ at: path, why: message(e, 'flux couldn’t open that folder.') });
      },
    );
    return () => {
      live = false;
    };
  }, [path]);
  const res = shown?.res ?? null;
  const loading = shown?.at !== path && failed?.at !== path;
  const openError = failed?.at === path ? failed.why : null;

  const go = (p: string | null) => {
    setAdded(null);
    setErr(null);
    setFailed(null);
    setPath(p);
  };
  const add = (e: FormEvent) => {
    e.preventDefault();
    if (!res?.path || !name.trim()) return;
    setAdding(true);
    setErr(null);
    api
      .createLibrary({ name: name.trim(), kind, roots: [res.path] })
      .then(
        (lib) => {
          setQueryData<Library[]>(keys.libraries, (prev) => [...(prev ?? []), lib]);
          invalidateQueries(keys.home);
          setAdded(lib.name);
          // Back up a level: the next folder (TV beside Films) is usually there.
          setPath(res.parent);
        },
        (e2: unknown) => setErr(message(e2, 'Couldn’t add the library.')),
      )
      .finally(() => setAdding(false));
  };

  const here = res?.here ?? null;
  return (
    <div className={styles.picker}>
      <nav className={styles.crumbs} aria-label="Folder">
        <button type="button" onClick={() => go(null)} data-current={res?.path === null || undefined}>
          <Icon name="home" /> Places
        </button>
        {res?.path &&
          crumbs(res.path).map((c) => (
            <button key={c.path} type="button" onClick={() => go(c.path)} data-current={c.path === res.path || undefined}>
              {c.name}
            </button>
          ))}
      </nav>
      {added && (
        <p className={styles.ok} role="status">
          <Icon name="check" /> {added} is in flux: it’s reading the files now. Add another folder, or go on.
        </p>
      )}
      <ul className={styles.folders} aria-busy={loading || undefined}>
        {res?.parent && (
          <li>
            <button type="button" onClick={() => go(res.parent)}>
              <Icon name="back" />
              <span>Up</span>
            </button>
          </li>
        )}
        {res?.folders.map((f) => (
          <li key={f.path}>
            <button type="button" onClick={() => go(f.path)}>
              <Icon name="folder" />
              <span>{f.name}</span>
              <small>{videosLabel(f.videos)}</small>
            </button>
          </li>
        ))}
        {res && res.folders.length === 0 && <li className={styles.none}>No folders in here.</li>}
      </ul>
      {res?.path && here && (
        <form className={styles.here} onSubmit={add}>
          <p>
            <b>{nameFor(res.path)}</b>: {summaryLine(here)}
          </p>
          {here.taken_by ? (
            <p className={styles.warn}>It’s already part of {here.taken_by}: a folder can be in one library only.</p>
          ) : (
            <>
              <div className={styles.row}>
                <label className="field">
                  <span>Name</span>
                  <input className="input" value={name} onChange={(e) => setName(e.target.value)} />
                </label>
              </div>
              <fieldset className={styles.kinds}>
                <legend>What’s in it</legend>
                {KIND_CHOICES.map(([k, label]) => (
                  <label key={k} data-checked={kind === k || undefined}>
                    <input type="radio" name="kind" checked={kind === k} onChange={() => setKind(k)} />
                    <span>{label}</span>
                  </label>
                ))}
              </fieldset>
              <button type="submit" className="btn btn-primary" disabled={adding || !name.trim()}>
                {adding ? 'Adding…' : `Add ${name.trim() || 'this folder'}`}
              </button>
            </>
          )}
        </form>
      )}
      {(err ?? openError) && (
        <p className={styles.error} role="alert">
          {err ?? openError}
        </p>
      )}
      <form
        className={styles.typed}
        onSubmit={(e) => {
          e.preventDefault();
          if (typed.trim()) go(typed.trim());
        }}
      >
        <label className="field">
          <span>Or type a folder’s path</span>
          <input className="input" value={typed} onChange={(e) => setTyped(e.target.value)} placeholder="/Volumes/Media/Films" />
        </label>
        <button type="submit" className="btn" disabled={!typed.trim()}>
          Open
        </button>
      </form>
    </div>
  );
}

function PlaybackStep({ onNext }: { onNext: () => void }) {
  const tools = useMediaTools();
  const t = tools.data;
  const [checking, setChecking] = useState(false);
  const recheck = () => {
    setChecking(true);
    void tools.refetch().finally(() => setChecking(false));
  };
  return (
    <div className={styles.body}>
      <h2 className="display-m">Playback</h2>
      <p className={styles.hint}>
        Most files play as they are. For the rest (a codec a browser can’t play, picture subtitles, HDR on an SDR screen), flux converts on the fly with ffmpeg.
      </p>
      {!t && tools.error ? <p className={styles.error}>{message(tools.error, 'flux didn’t say what ffmpeg can do.')}</p> : null}
      {!t && !tools.error && <div className={`skeleton ${styles.skeleton}`} />}
      {t && t.ok && (
        <dl className={styles.facts}>
          <div>
            <dt>ffmpeg</dt>
            <dd>{t.ffmpeg.version ?? t.ffmpeg.path}</dd>
          </div>
          <div>
            <dt>Video conversion</dt>
            <dd>{encoderLine(t.video)}</dd>
          </div>
        </dl>
      )}
      {t && !t.ok && <FfmpegMissing platform={t.platform} missing={t.missing} />}
      {t && t.ok && <Gaps lines={t.missing} />}
      <div className={styles.actions}>
        <button type="button" className="btn" onClick={recheck} disabled={checking}>
          <Icon name="refresh" /> {checking ? 'Checking…' : 'Check again'}
        </button>
        <button type="button" className="btn btn-primary" onClick={onNext}>
          Next: other devices
        </button>
      </div>
    </div>
  );
}

/** The server's gaps (doctor's lines), with flags like `--ffmpeg` set as code. */
function Gaps({ lines }: { lines: string[] }) {
  if (lines.length === 0) return null;
  return (
    <ul className={styles.missing}>
      {lines.map((m) => (
        <li key={m}>{m.split(/(--[a-z][a-z-]*)/).map((part, i) => (i % 2 ? <code key={i}>{part}</code> : part))}</li>
      ))}
    </ul>
  );
}

function FfmpegMissing({ platform, missing }: { platform: string; missing: string[] }) {
  const fix = ffmpegFix(platform);
  return (
    <div className={styles.fix}>
      <p className={styles.warn}>flux didn’t find ffmpeg, so it plays only the files your devices can play as they are.</p>
      <Gaps lines={missing} />
      <p>{fix.text}</p>
      {fix.command && <Command text={fix.command} />}
      <a className={styles.link} href={fix.link} target="_blank" rel="noreferrer noopener">
        The steps for every system
      </a>
    </div>
  );
}

function DevicesStep() {
  const addresses = useAddresses();
  const tailnet = useTailnet();
  const update = useUpdateInfo();
  const docker = update.data?.install.kind === 'docker';
  const lan = addresses.data?.lan ?? [];
  const remote = tailnet.data?.state === 'running';
  const way = installWay();
  return (
    <div className={styles.body}>
      <h2 className="display-m">Other devices</h2>
      <div className={styles.cards}>
        <article className={styles.card}>
          <h3>
            <Icon name="monitor" /> At home
          </h3>
          {lan.length > 0 ? (
            <>
              <p>Phones, tablets and TVs on this network open flux at:</p>
              <div className={styles.qr}>
                <QrCode value={lan[0]} size={132} label={`QR code to open ${lan[0]}`} />
                <ul className={styles.urls}>
                  {lan.map((u) => (
                    <li key={u}>
                      <code className="mono">{u}</code>
                    </li>
                  ))}
                </ul>
              </div>
              {docker && <p className={styles.hint}>In Docker, use the address of the machine Docker runs on, with the port it publishes.</p>}
            </>
          ) : (
            <p>
              flux answers only this computer now. To watch on the TV or a phone at home, start it with <code className="mono">--lan 0.0.0.0:8097</code> (or <code className="mono">lan = "0.0.0.0:8097"</code> in its config).
            </p>
          )}
        </article>
        <article className={styles.card}>
          <h3>
            <Icon name="tailscale" /> Anywhere
          </h3>
          <p>{remote ? `Remote access is on: ${tailnet.data?.dns_name ?? ''}` : 'With a free Tailscale account, your devices reach flux from anywhere: no ports to open, no passwords.'}</p>
          <Link to="/connect" className="btn btn-sm">
            {remote ? 'Remote access' : 'Set up remote access'}
          </Link>
        </article>
        <article className={styles.card}>
          <h3>
            <Icon name="install" /> As an app
          </h3>
          <p>
            {way === 'ios'
              ? 'On iPhone and iPad: Share, then Add to Home Screen.'
              : way === 'mac-safari'
                ? 'In Safari: File, then Add to Dock.'
                : 'Chrome and Edge install flux from the corner menu (Install flux) on this computer, or over the tailnet with HTTPS on.'}
          </p>
        </article>
      </div>
      <div className={styles.actions}>
        <button type="button" className="btn btn-primary" onClick={() => navigate('/', { replace: true })}>
          Done
        </button>
      </div>
    </div>
  );
}
