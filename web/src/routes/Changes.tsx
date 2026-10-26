/**
 * Agents & changes: every changeset an agent or a person made through flux
 * (docs/AGENT-NATIVE.md), newest first: what changed, why, from which
 * pages; proposals to apply or turn down; undo for the rest. And how to
 * connect an agent in the first place.
 */
import { useEffect, useMemo, useState } from 'react';
import { useTitle } from '../app/useTitle';
import { Link } from '../app/Link';
import {
  fieldName,
  fixText,
  hostOf,
  op,
  OpFailure,
  valueText,
  type Change,
  type Changeset,
  type ChangesetState,
  type ChangesetSummary,
  type ChangesListOutput,
  type Decided,
  type FileFix,
  type Link as SourceLink,
} from '../api/agent';
import { subscribeEvents } from '../api/events';
import { formatAgo, formatCount } from '../api/format';
import { keys, useChanges, useManifest, useServer } from '../api/queries';
import { invalidateQueries, useQuery } from '../api/query';
import type { Id } from '../api/types';
import { Icon } from '../components/Icon';
import styles from './Changes.module.css';

const STATE_LABEL: Record<ChangesetState, string> = {
  proposed: 'Waiting for you',
  applied: 'Applied',
  reverted: 'Undone',
  rejected: 'Turned down',
};

const plural = (n: number, one: string, many = `${one}s`) => `${formatCount(n)} ${n === 1 ? one : many}`;

/** "claude-code for james@…", or the person. */
function who(cs: ChangesetSummary, me: string | null): string {
  if (!cs.agent) return cs.by === me ? 'You' : cs.by;
  return cs.by === me || cs.by === 'local' ? cs.agent : `${cs.agent} for ${cs.by}`;
}

function CopyBox({ text, label }: { text: string; label: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <div className={styles.copyBox}>
      <pre>
        <code>{text}</code>
      </pre>
      <button
        type="button"
        className="btn btn-sm"
        aria-label={`Copy ${label}`}
        onClick={() => {
          void navigator.clipboard?.writeText(text).then(() => {
            setCopied(true);
            setTimeout(() => setCopied(false), 1600);
          });
        }}
      >
        <Icon name={copied ? 'check' : 'copy'} />
        {copied ? 'Copied' : 'Copy'}
      </button>
    </div>
  );
}

function ConnectAgent() {
  const m = useManifest();
  const server = useServer();
  const origin = server.data?.https_url ?? (typeof location === 'undefined' ? 'http://localhost:8096' : location.origin);
  const remote = !/\/\/(localhost|127\.0\.0\.1|\[::1\])(:|$)/.test(origin);
  return (
    <section className={styles.connect} id="connect" aria-labelledby="connect-title">
      <h2 id="connect-title" className="display-m">
        Connect an agent
      </h2>
      <p className={styles.lede}>
        flux has no AI of its own. Connect the agent you already use and it can identify your titles, fill in what’s missing from the open web, find artwork and design new
        lenses. Everything it changes shows up here, with its sources, and you can undo it.
      </p>
      <div className={styles.ways}>
        <div className={styles.way}>
          <h3>Claude Code, Codex or any MCP client</h3>
          <CopyBox label="the MCP command" text={`claude mcp add --transport http flux ${origin}${m.data?.mcp ?? '/mcp'}`} />
          <p className={styles.note}>{remote ? 'On the tailnet your Tailscale identity is the sign-in: no tokens.' : 'On this machine the agent acts as the server’s owner.'}</p>
        </div>
        <div className={styles.way}>
          <h3>Agents that use a terminal</h3>
          <CopyBox label="the terminal commands" text={remote ? `export FLUX_SERVER=${origin}\nflux skills install\nflux status` : 'flux skills install\nflux status'} />
          <p className={styles.note}>The flux command comes with flux. Its skills teach an agent the jobs below.</p>
        </div>
      </div>
      {m.data && m.data.skills.length > 0 && (
        <ul className={styles.skills}>
          {m.data.skills.map((s) => (
            <li key={s.name}>
              <span className="mono">{s.name}</span>
              <span>{s.description.split('. ')[0].replace(/\.$/, '')}.</span>
            </li>
          ))}
        </ul>
      )}
      <p className={styles.policy}>
        <Icon name="lock" /> TMDB’s descriptions and images never reach an agent: TMDB’s terms forbid using them with AI. Agents write their own, and say where they found it.
      </p>
    </section>
  );
}

/** Changes grouped by item, in the changeset's order. */
function byItem(changes: Change[]): { item: Id; title: string | null; changes: Change[]; sources: SourceLink[] }[] {
  const out: { item: Id; title: string | null; changes: Change[]; sources: SourceLink[] }[] = [];
  for (const ch of changes) {
    let g = out.find((x) => x.item === ch.item);
    if (!g) {
      g = { item: ch.item, title: ch.title, changes: [], sources: [] };
      out.push(g);
    }
    g.changes.push(ch);
    for (const s of ch.sources) if (!g.sources.some((x) => x.url === s.url)) g.sources.push(s);
  }
  return out;
}

const CHANGE_NOTE: Partial<Record<Change['state'], string>> = {
  pending: 'proposed',
  skipped: 'not applied: it had changed',
  reverted: 'undone',
  kept: 'kept: it changed again since',
};

/** Fields of a file plan's steps. */
const STEP_FIELDS = new Set(['file.path', 'folder.path', 'folder.new', 'folder.gone']);

/** A step's path (`{root, path, …}`). */
function pathOf(v: unknown): string | null {
  return v && typeof v === 'object' && typeof (v as { path?: unknown }).path === 'string' ? (v as { path: string }).path : null;
}

const STEP_LABEL: Record<string, string> = {
  'folder.new': 'New folder',
  'folder.path': 'Rename folder',
  'folder.gone': 'Remove empty folder',
};

const STEP_NOTE: Partial<Record<Change['state'], string>> = {
  pending: 'to do',
  skipped: 'not done',
  reverted: 'put back',
  kept: 'left: it moved again since',
};

/** A file plan, step by step: what moves where. */
function PlanSteps({ changes }: { changes: Change[] }) {
  return (
    <ol className={styles.steps}>
      {changes
        .filter((ch) => STEP_FIELDS.has(ch.field))
        .map((ch, i) => {
          const companion = (ch.after as { what?: string } | null)?.what === 'companion';
          const label = STEP_LABEL[ch.field] ?? (companion ? 'With it' : 'Move');
          const from = pathOf(ch.before);
          const to = pathOf(ch.after);
          return (
            <li key={i} data-state={ch.state} data-step={ch.field} data-companion={companion ? '' : undefined}>
              <span className={styles.stepKind}>{label}</span>
              <span className={styles.stepPaths}>
                {from && <del className="mono">{from}</del>}
                {to && <ins className="mono">{to}</ins>}
              </span>
              {STEP_NOTE[ch.state] && <span className={styles.changeNote}>{STEP_NOTE[ch.state]}</span>}
            </li>
          );
        })}
    </ol>
  );
}

/** A fix change's fix (`{file, path, fix}`). */
function fixOf(v: unknown): FileFix | null {
  return v && typeof v === 'object' ? ((v as { fix?: FileFix | null }).fix ?? null) : null;
}

function ChangesetBody({ id }: { id: Id }) {
  const q = useQuery(`changeset:${id}`, () => op<Changeset>('changes.get', { changeset: id }), { staleMs: 2_000 });
  if (q.status === 'error') return <p className={styles.errorNote}>{q.error instanceof Error ? q.error.message : 'Couldn’t load this changeset.'}</p>;
  if (!q.data) return <div className={`skeleton ${styles.bodySkeleton}`} />;
  if (q.data.moves_files) {
    return (
      <div className={styles.body}>
        <PlanSteps changes={q.data.changes} />
        {q.data.state === 'proposed' && (
          <p className={styles.planNote}>
            <Icon name="lock" /> Applying it renames and moves these files on disk. flux never replaces a file, never moves one to another drive, and Undo puts everything back.
          </p>
        )}
      </div>
    );
  }
  return (
    <div className={styles.body}>
      {byItem(q.data.changes).map((g) => (
        <div key={g.item} className={styles.group}>
          {g.item > 0 ? (
            <Link to={`/item/${g.item}`} className={styles.itemLink}>
              {g.title ?? `Item ${g.item} (gone)`}
            </Link>
          ) : (
            <span className={styles.itemLink}>Not in the library</span>
          )}
          <ul className={styles.diff}>
            {g.changes.map((ch, i) =>
              ch.field === 'file.fix' ? (
                <li key={`fix-${i}`} data-state={ch.state}>
                  <span className={styles.field}>{fieldName(ch.field)}</span>
                  <span className={styles.values}>
                    <span className={`mono ${styles.filePath}`}>{pathOf(ch.before) ?? pathOf(ch.after)}</span>
                    <del>{fixText(fixOf(ch.before))}</del>
                    <ins>{fixText(fixOf(ch.after))}</ins>
                  </span>
                  {CHANGE_NOTE[ch.state] && <span className={styles.changeNote}>{CHANGE_NOTE[ch.state]}</span>}
                </li>
              ) : (
                <li key={`${ch.field}`} data-state={ch.state}>
                  <span className={styles.field}>{fieldName(ch.field)}</span>
                  <span className={styles.values}>
                    {ch.before !== null && ch.before !== undefined ? <del>{valueText(ch.before, ch.field)}</del> : ch.withheld ? <del className={styles.withheld}>{ch.withheld.toUpperCase()}’s</del> : null}
                    <ins>{ch.after === null || ch.after === undefined ? 'cleared' : valueText(ch.after, ch.field)}</ins>
                  </span>
                  {CHANGE_NOTE[ch.state] && <span className={styles.changeNote}>{CHANGE_NOTE[ch.state]}</span>}
                </li>
              ),
            )}
          </ul>
          {g.sources.length > 0 && (
            <p className={styles.sources}>
              From{' '}
              {g.sources.map((s, i) => (
                <span key={s.url}>
                  {i > 0 && ', '}
                  <a href={s.url} target="_blank" rel="noreferrer noopener">
                    {s.title || hostOf(s.url)}
                  </a>
                </span>
              ))}
            </p>
          )}
        </div>
      ))}
    </div>
  );
}

function ChangesetCard({
  cs,
  me,
  admin,
  open,
  onToggle,
  note,
  setNote,
}: {
  cs: ChangesetSummary;
  me: string | null;
  admin: boolean;
  open: boolean;
  onToggle: () => void;
  /** What the last decision on it did (kept by the page: a decided proposal moves to History). */
  note: string | null;
  setNote: (note: string) => void;
}) {
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const decide = (action: 'undo' | 'apply' | 'reject') => {
    setBusy(true);
    setErr(null);
    op<Decided>(`changes.${action}`, { changeset: cs.id })
      .then((d) => {
        const kept = d.not_done.map((n) => (cs.moves_files ? n.reason : `${fieldName(n.field)} (${n.reason})`));
        const did = action === 'undo' ? 'Undone' : action === 'apply' ? 'Applied' : 'Turned down';
        setNote(action === 'reject' ? did : `${did}: ${plural(d.done, 'change')}${kept.length ? `. Left as it was: ${kept.join(', ')}` : ''}.`);
        invalidateQueries(keys.changes);
        invalidateQueries(`changeset:${cs.id}`);
        invalidateQueries('facts:');
        invalidateQueries('item:');
      })
      .catch((e: unknown) => setErr(e instanceof OpFailure || e instanceof Error ? e.message : 'That didn’t work.'))
      .finally(() => setBusy(false));
  };
  return (
    <article className={styles.cs} data-state={cs.state}>
      <button type="button" className={styles.csHead} onClick={onToggle} aria-expanded={open}>
        <span className={styles.avatar} data-agent={cs.agent ? '' : undefined} aria-hidden="true">
          {cs.agent ? <Icon name="sparkle" /> : <Icon name="user" />}
        </span>
        <span className={styles.csMain}>
          <span className={styles.summary}>{cs.summary}</span>
          <span className={styles.byline}>
            <span className="mono">{who(cs, me)}</span> · {formatAgo(cs.created_at)} ·{' '}
            {cs.moves_files ? plural(cs.change_count, 'step') : plural(cs.change_count, 'change')}
            {cs.item_count > 1 ? ` to ${plural(cs.item_count, 'title')}` : ''}
            {cs.moves_files ? ' · moves files' : ''}
          </span>
        </span>
        <span className={styles.chip} data-state={cs.state}>
          {STATE_LABEL[cs.state]}
        </span>
        <Icon name="forward" />
      </button>
      {open && <ChangesetBody id={cs.id} />}
      {(note || err || (admin && (cs.state === 'applied' || cs.state === 'proposed'))) && (
        <div className={styles.actions}>
          {admin && cs.state === 'applied' && (
            <button type="button" className="btn btn-sm" disabled={busy} onClick={() => decide('undo')}>
              <Icon name="restart" />
              Undo
            </button>
          )}
          {admin && cs.state === 'proposed' && (
            <>
              <button
                type="button"
                className="btn btn-sm btn-primary"
                disabled={busy}
                onClick={() => {
                  if (cs.moves_files && !open) {
                    // A plan moves files: look at it before saying yes.
                    onToggle();
                    return;
                  }
                  decide('apply');
                }}
              >
                <Icon name="check" />
                {cs.moves_files ? (open ? 'Move the files' : 'Review the plan') : 'Apply'}
              </button>
              <button type="button" className="btn btn-sm btn-ghost" disabled={busy} onClick={() => decide('reject')}>
                Turn down
              </button>
            </>
          )}
          {note && (
            <span className={styles.result} role="status">
              {note}
            </span>
          )}
          {err && (
            <span className={styles.errorNote} role="alert">
              {err}
            </span>
          )}
        </div>
      )}
    </article>
  );
}

export default function Changes({ query }: { query: URLSearchParams }) {
  useTitle('Agents & changes');
  const server = useServer();
  const list = useChanges();
  const me = server.data?.you?.login_name ?? null;
  const admin = server.data?.you?.role === 'owner' || server.data?.you?.role === 'admin';
  const [openId, setOpenId] = useState<Id | null>(() => Number(query.get('open')) || null);
  const [older, setOlder] = useState<ChangesetSummary[]>([]);
  const [notes, setNotes] = useState<Record<Id, string>>({});
  const [next, setNext] = useState<Id | null | undefined>(undefined);

  // Live: an agent's new changeset appears as it lands.
  useEffect(
    () =>
      subscribeEvents((e) => {
        if (e.type === 'changes') {
          invalidateQueries(keys.changes);
          invalidateQueries(`changeset:${e.changeset.id}`);
        }
      }),
    [],
  );

  const first = list.data;
  const cursor = next === undefined ? (first?.next ?? null) : next;
  const all = useMemo(() => {
    const seen = new Set<Id>();
    return [...(first?.changesets ?? []), ...older].filter((c) => (seen.has(c.id) ? false : (seen.add(c.id), true)));
  }, [first, older]);
  const proposals = all.filter((c) => c.state === 'proposed');
  const history = all.filter((c) => c.state !== 'proposed');

  const loadOlder = () => {
    if (cursor === null) return;
    void op<ChangesListOutput>('changes.list', { limit: 50, before: cursor }).then((p) => {
      setOlder((o) => [...o, ...p.changesets]);
      setNext(p.next);
    });
  };

  const card = (cs: ChangesetSummary) => (
    <ChangesetCard
      key={cs.id}
      cs={cs}
      me={me}
      admin={admin}
      open={openId === cs.id}
      onToggle={() => setOpenId((o) => (o === cs.id ? null : cs.id))}
      note={notes[cs.id] ?? null}
      setNote={(n) => setNotes((m) => ({ ...m, [cs.id]: n }))}
    />
  );

  return (
    <div className={styles.page}>
      <header className={styles.header}>
        <h1 className="display-l">Agents &amp; changes</h1>
        <p className={styles.lede}>Everything an agent, or you, changed through flux: what, why and from where. Every change can be undone.</p>
      </header>

      {proposals.length > 0 && (
        <section className={styles.section} aria-labelledby="waiting-title">
          <h2 id="waiting-title" className={`edge ${styles.sectionTitle}`}>
            Waiting for you · {formatCount(first?.proposed ?? proposals.length)}
          </h2>
          <div className={styles.list}>{proposals.map(card)}</div>
          {!admin && <p className={styles.note}>An Owner or Admin applies proposals.</p>}
        </section>
      )}

      <section className={styles.section} aria-labelledby="history-title">
        <h2 id="history-title" className={`edge ${styles.sectionTitle}`}>
          History
        </h2>
        {list.status === 'error' && <p className={styles.errorNote}>{list.error instanceof Error ? list.error.message : 'Couldn’t load the changes.'}</p>}
        {!first && list.status !== 'error' && <div className={`skeleton ${styles.listSkeleton}`} />}
        {first && history.length === 0 && (
          <p className={styles.empty}>Nothing yet. When an agent or a person changes a title through flux, it shows up here.</p>
        )}
        <div className={styles.list}>{history.map(card)}</div>
        {cursor !== null && first && (
          <button type="button" className="btn btn-sm" onClick={loadOlder}>
            Older
          </button>
        )}
      </section>

      <ConnectAgent />
    </div>
  );
}
