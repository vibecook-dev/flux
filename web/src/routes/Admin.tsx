/**
 * Libraries & scanning: what flux watches, live scan progress over SSE, and
 * honest availability — a sleeping NAS makes a library "partly offline", it
 * never deletes anything (ARCHITECTURE.md principle 5).
 */
import { useEffect, useMemo, useRef, useState } from 'react';
import { useTitle } from '../app/useTitle';
import { api } from '../api/client';
import { subscribeEvents, subscribeEventsState, type EventsState } from '../api/events';
import { formatAgo, formatCount, pathLabel } from '../api/format';
import { refreshFolders } from '../api/folder-list';
import { refreshLibrary } from '../api/library-list';
import { keys, useLibraries, useServer, useTailnet } from '../api/queries';
import { invalidateQueries, setQueryData } from '../api/query';
import { LIBRARY_KINDS, type Library, type LibraryKind, type ScanStatus } from '../api/types';
import { Link } from '../app/Link';
import { Icon } from '../components/Icon';
import styles from './Admin.module.css';
import { STATE_LABEL, applyScanProgress, lastScanLine, metadataNote, offlineNote, offlineRoots, scanCounters, stateLabel } from './admin-state';

const KIND_LABEL: Record<LibraryKind, string> = { movies: 'Films', shows: 'Shows', mixed: 'Mixed', videos: 'Videos' };
/** The add-library choices. `videos`: home videos and clips, one per file, by folder, never matched. */
const KIND_CHOICE: Record<LibraryKind, string> = { movies: 'Films', shows: 'TV shows', mixed: 'Both', videos: 'Home videos & clips' };

interface Rate {
  /** performance.now() and wall-clock time when the last progress event arrived. */
  at: number;
  wall: number;
  seen: number;
  perSec: number | null;
}

function ScanReadout({ lib, rate }: { lib: Library; rate: Rate | undefined }) {
  const s = lib.scan;
  if (s.state === 'scanning') {
    const pct = s.files_seen > 0 ? Math.min(100, (s.files_probed / Math.max(1, s.files_seen)) * 100) : 0;
    // Elapsed as of the latest event (render stays pure; events arrive several times a second).
    const elapsed = s.started_at && rate ? Math.max(0, (rate.wall - s.started_at) / 1000) : 0;
    return (
      <div className={styles.scanning}>
        <div className={styles.bar} aria-hidden="true">
          {/* The walk bar is a true fraction once the server knows the total. */}
          <span className={styles.barWalk} style={{ width: s.files_total ? `${Math.min(100, (s.files_seen / Math.max(1, s.files_total)) * 100)}%` : s.files_seen > 0 ? '100%' : '6%' }} />
          <span className={styles.barProbe} style={{ width: `${pct}%` }} />
        </div>
        <dl className={styles.counters}>
          {scanCounters(lib, formatCount).map(([label, value]) => (
            <div key={label}>
              <dt>{label}</dt>
              <dd>{value}</dd>
            </div>
          ))}
          <div>
            <dt>Speed</dt>
            <dd>{rate?.perSec ? `${formatCount(Math.round(rate.perSec))}/s` : '…'}</dd>
          </div>
          <div>
            <dt>Time</dt>
            <dd>{elapsed.toFixed(1)} s</dd>
          </div>
        </dl>
      </div>
    );
  }
  const line = lastScanLine(lib, formatAgo, formatCount);
  return <p className={styles.lastScan}>{line ?? 'Not scanned yet.'}</p>;
}

function LibraryCard({ lib, rate, offlineRoots, onScan, scanError }: { lib: Library; rate: Rate | undefined; offlineRoots: Set<string>; onScan: () => void; scanError: string | null }) {
  const scanning = lib.scan.state === 'scanning';
  return (
    <article className={styles.lib} data-state={lib.scan.state}>
      <header className={styles.libHead}>
        <div>
          <h2 className="display-m">
            <Link to={`/library/${lib.id}`}>{lib.name}</Link>
          </h2>
          <p className="edge">
            {KIND_LABEL[lib.kind]} · {formatCount(lib.item_count)} items · <span className={styles.state}>{stateLabel(lib, offlineRoots)}</span>
          </p>
        </div>
        <button type="button" className="btn btn-sm" onClick={onScan} disabled={scanning}>
          <Icon name="refresh" />
          {scanning ? 'Scanning…' : 'Scan now'}
        </button>
      </header>
      <ul className={styles.roots}>
        {lib.roots.map((root) => {
          const offline = offlineRoots.has(root.path);
          return (
            <li key={root.path} data-offline={offline || undefined}>
              <span className={styles.rootDot} aria-hidden="true" />
              <code>{root.path}</code>
              <span className="edge">
                {offline ? `not responding${root.last_seen ? ` · last seen ${formatAgo(root.last_seen)}` : ''}` : 'online'}
              </span>
            </li>
          );
        })}
      </ul>
      {offlineNote(lib, offlineRoots) && (
        <p className={styles.offlineNote} title={lib.scan.last_error ?? undefined}>
          {offlineNote(lib, offlineRoots)}
        </p>
      )}
      {lib.scan.dirs_unavailable > 0 && (
        <p className={styles.offlineNote}>
          {formatCount(lib.scan.dirs_unavailable)} {lib.scan.dirs_unavailable === 1 ? 'folder' : 'folders'} didn’t respond during the last scan. Nothing in them was removed.
        </p>
      )}
      {lib.scan.state === 'error' && lib.scan.last_error && <p className={styles.errorNote}>{lib.scan.last_error}</p>}
      {scanError && (
        <p className={styles.errorNote} role="alert">
          {scanError}
        </p>
      )}
      <ScanReadout lib={lib} rate={rate} />
    </article>
  );
}

function AddLibraryDialog({ open, onClose }: { open: boolean; onClose: () => void }) {
  const ref = useRef<HTMLDialogElement>(null);
  const [name, setName] = useState('');
  const [kind, setKind] = useState<LibraryKind>('movies');
  const [roots, setRoots] = useState<string[]>(['']);
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    const d = ref.current;
    if (!d) return;
    if (open && !d.open) d.showModal();
    else if (!open && d.open) d.close();
  }, [open]);

  const submit = () => {
    const folders = roots.map((r) => r.trim()).filter(Boolean);
    if (!name.trim()) return setErr('Give the library a name.');
    if (folders.length === 0) return setErr('Add at least one folder.');
    setBusy(true);
    setErr(null);
    api
      .createLibrary({ name: name.trim(), kind, roots: folders })
      .then(
        (lib) => {
          setQueryData<Library[]>(keys.libraries, (prev) => [...(prev ?? []), lib]);
          setName('');
          setRoots(['']);
          onClose();
        },
        (e: unknown) => setErr(e instanceof Error ? e.message : 'Couldn’t add the library.'),
      )
      .finally(() => setBusy(false));
  };

  return (
    <dialog ref={ref} className={styles.dialog} onClose={onClose} aria-labelledby="add-lib-title">
      <form
        className={styles.form}
        onSubmit={(e) => {
          e.preventDefault();
          submit();
        }}
      >
        <h2 id="add-lib-title" className="display-m">
          Add a library
        </h2>
        <label className="field">
          <span>Name</span>
          <input className="input" value={name} onChange={(e) => setName(e.target.value)} placeholder="Films" autoFocus />
        </label>
        <fieldset className={styles.kindPick}>
          <legend>What’s in it</legend>
          {LIBRARY_KINDS.map((k) => (
            <label key={k}>
              <input type="radio" name="kind" checked={kind === k} onChange={() => setKind(k)} />
              <span>{KIND_CHOICE[k]}</span>
            </label>
          ))}
        </fieldset>
        <div className="field">
          <span>Folders on this server</span>
          {roots.map((r, i) => (
            <div key={i} className={styles.rootInput}>
              <input
                className="input"
                value={r}
                onChange={(e) => setRoots(roots.map((x, j) => (j === i ? e.target.value : x)))}
                placeholder="/Volumes/Media/Films"
                aria-label={`Folder ${i + 1}`}
              />
              {roots.length > 1 && (
                <button type="button" className="btn btn-ghost btn-sm" onClick={() => setRoots(roots.filter((_, j) => j !== i))} aria-label={`Remove folder ${i + 1}`}>
                  <Icon name="close" />
                </button>
              )}
            </div>
          ))}
          <button type="button" className="btn btn-ghost btn-sm" onClick={() => setRoots([...roots, ''])}>
            <Icon name="plus" /> Add another folder
          </button>
        </div>
        <p className={styles.formHint}>flux reads only file headers while scanning; titles appear as soon as each file is read, before artwork arrives.</p>
        {err && <p className={styles.errorNote}>{err}</p>}
        <div className={styles.formActions}>
          <button type="button" className="btn" onClick={onClose}>
            Cancel
          </button>
          <button type="submit" className="btn btn-primary" disabled={busy}>
            {busy ? 'Adding…' : 'Add and scan'}
          </button>
        </div>
      </form>
    </dialog>
  );
}

export default function Admin() {
  useTitle('Libraries & scanning');
  const libs = useLibraries();
  const server = useServer();
  const tailnet = useTailnet();
  const [events, setEvents] = useState<EventsState>('connecting');
  const [adding, setAdding] = useState(false);
  const [rates, setRates] = useState<Record<number, Rate>>({});
  // Root availability: what the libraries report (RootStatus), overridden by live SSE events.
  const [liveRoots, setLiveRoots] = useState<Map<string, boolean>>(new Map());
  const [announce, setAnnounce] = useState('');
  // "Scan now" refused (e.g. a 403 from flux's request guard), per library.
  const [scanErrors, setScanErrors] = useState<Record<number, string>>({});

  const offline = useMemo(() => offlineRoots(libs.data ?? [], liveRoots), [libs.data, liveRoots]);

  useEffect(() => subscribeEventsState(setEvents), []);

  // Subscribed once: re-subscribing on every list change (this effect used to
  // depend on the library list) cancelled the pending refresh below, so a new
  // library kept showing "0 items" after its scan.
  useEffect(() => {
    let refetch: ReturnType<typeof setTimeout> | null = null;
    const refreshSoon = (libraryId: number) => {
      if (refetch) return;
      refetch = setTimeout(() => {
        refetch = null;
        invalidateQueries(keys.libraries);
        invalidateQueries(keys.home);
        refreshLibrary(libraryId);
        refreshFolders(libraryId);
      }, 1000);
    };
    const unsub = subscribeEvents((e) => {
      if (e.type === 'scan_progress') {
        let previous: ScanStatus['state'] | null = null;
        let name: string | null = null;
        setQueryData<Library[]>(keys.libraries, (prev) => {
          const r = applyScanProgress(prev, e.library_id, e.status);
          previous = r.previous;
          name = r.name;
          return r.libs;
        });
        setRates((prev) => {
          const now = performance.now();
          const old = prev[e.library_id];
          const perSec = old && now > old.at ? ((e.status.files_seen - old.seen) * 1000) / (now - old.at) : null;
          return { ...prev, [e.library_id]: { at: now, wall: Date.now(), seen: e.status.files_seen, perSec: perSec !== null && perSec > 0 ? perSec : (old?.perSec ?? null) } };
        });
        if (previous && previous !== e.status.state) {
          setAnnounce(`${name ?? 'Library'}: ${STATE_LABEL[e.status.state].toLowerCase()}`);
          // A finished scan: item counts and availability live on the list.
          if (previous === 'scanning') refreshSoon(e.library_id);
        }
      } else if (e.type === 'source_availability') {
        setLiveRoots((prev) => new Map(prev).set(e.root, e.available));
      } else if (e.type === 'items_changed') {
        // Counts live on the library list; refresh it (and open grids) at most once a second.
        refreshSoon(e.library_id);
      }
    });
    return () => {
      unsub();
      if (refetch) clearTimeout(refetch);
    };
  }, []);

  const scan = (lib: Library) => {
    setScanErrors((prev) => {
      const next = { ...prev };
      delete next[lib.id];
      return next;
    });
    api.scanLibrary(lib.id).then(
      (status) => setQueryData<Library[]>(keys.libraries, (prev) => (prev ?? []).map((l) => (l.id === lib.id ? { ...l, scan: status } : l))),
      (e: unknown) => setScanErrors((prev) => ({ ...prev, [lib.id]: e instanceof Error ? e.message : 'Couldn’t start the scan.' })),
    );
  };

  const s = server.data;
  const noMetadata = s ? metadataNote(s.metadata.source) : null;
  return (
    <div className={styles.page}>
      <header className={styles.header}>
        <div>
          <h1 className="display-l">Libraries &amp; scanning</h1>
          <p className={`edge ${styles.live}`} data-state={events}>
            <span aria-hidden="true" /> {events === 'open' ? 'Live' : events === 'reconnecting' ? 'Reconnecting…' : events === 'connecting' ? 'Connecting…' : 'Offline'}
          </p>
        </div>
        <button type="button" className="btn btn-primary" onClick={() => setAdding(true)}>
          <Icon name="plus" /> Add library
        </button>
      </header>

      <p className="visually-hidden" aria-live="polite">
        {announce}
      </p>

      {s && (
        <dl className={styles.server}>
          <div>
            <dt>Server</dt>
            <dd>{s.name}</dd>
          </div>
          <div>
            <dt>Version</dt>
            <dd>{s.version}</dd>
          </div>
          <div>
            <dt>You’re connected via</dt>
            <dd>{pathLabel(s.path)}</dd>
          </div>
          <div>
            <dt>Signed in as</dt>
            <dd>{s.you ? `${s.you.display_name} (${s.you.role})` : 'Local access, no Tailscale identity'}</dd>
          </div>
          <div>
            <dt>Remote access</dt>
            <dd>
              {tailnet.data?.state === 'running' ? (
                tailnet.data.dns_name
              ) : (
                <Link to="/connect" className={styles.inlineLink}>
                  Set up remote access
                </Link>
              )}
            </dd>
          </div>
        </dl>
      )}

      {noMetadata && (
        <p className={styles.metadataNote}>
          {noMetadata}{' '}
          <a className={styles.inlineLink} href="https://www.themoviedb.org/settings/api" target="_blank" rel="noreferrer">
            Get a TMDB key
          </a>
        </p>
      )}

      <div className={styles.libs}>
        {libs.data?.map((l) => <LibraryCard key={l.id} lib={l} rate={rates[l.id]} offlineRoots={offline} onScan={() => scan(l)} scanError={scanErrors[l.id] ?? null} />)}
        {!libs.data && [0, 1, 2].map((i) => <div key={i} className={`skeleton ${styles.libSkeleton}`} />)}
        {libs.data?.length === 0 && (
          <div className={styles.empty}>
            <p>No libraries yet. Add a folder of films or shows to get started.</p>
          </div>
        )}
      </div>

      <AddLibraryDialog open={adding} onClose={() => setAdding(false)} />
    </div>
  );
}
