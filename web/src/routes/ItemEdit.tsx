/**
 * Editing an item's details in the app: the same `items.edit` agents use,
 * as a person (docs/AGENT-NATIVE.md, "Editing in the app"). What you set is
 * yours: flux's matcher and scans leave it alone, an agent changes it only
 * when you ask it to, and every save is a changeset you can undo (Agents &
 * changes). A Viewer's edit is a proposal for an Owner or Admin.
 */
import { useMemo, useState } from 'react';
import { op, OpFailure, type AgentItemDetail, type CastMember, type EditOutput, type Fields, type Link } from '../api/agent';
import { invalidateQueries } from '../api/query';
import { keys } from '../api/queries';
import type { Id, ItemKind } from '../api/types';
import { Icon } from '../components/Icon';
import about from './ItemAbout.module.css';

type Kind = 'text' | 'year' | 'long' | 'list' | 'codes' | 'cast' | 'links' | 'date';

interface Spec {
  field: keyof Fields;
  label: string;
  kind: Kind;
  hint?: string;
  /** Only for these kinds of item. */
  only?: ItemKind[];
  /** Never for these. */
  not?: ItemKind[];
}

const SPECS: Spec[] = [
  { field: 'title', label: 'Title', kind: 'text' },
  { field: 'original_title', label: 'Original title', kind: 'text', hint: 'In its own language and script', not: ['episode', 'season'] },
  { field: 'year', label: 'Year', kind: 'year', not: ['episode', 'season'] },
  { field: 'overview', label: 'Description', kind: 'long' },
  { field: 'tagline', label: 'Tagline', kind: 'text', not: ['episode', 'season'] },
  { field: 'genres', label: 'Genres', kind: 'list', hint: 'Comma-separated', not: ['episode', 'season'] },
  { field: 'directors', label: 'Directed by', kind: 'list', hint: 'Comma-separated' },
  { field: 'writers', label: 'Written by', kind: 'list', hint: 'Comma-separated' },
  { field: 'cast', label: 'Cast', kind: 'cast', hint: 'One a line: Name as Role' },
  { field: 'studios', label: 'Studios', kind: 'list', hint: 'Comma-separated', not: ['episode', 'season'] },
  { field: 'release_date', label: 'Released', kind: 'date', hint: 'YYYY-MM-DD, YYYY-MM or YYYY', not: ['episode', 'season'] },
  { field: 'air_date', label: 'Aired', kind: 'date', hint: 'YYYY-MM-DD', only: ['episode'] },
  { field: 'countries', label: 'Countries', kind: 'codes', hint: 'Two-letter codes: JP, FR', not: ['episode', 'season'] },
  { field: 'languages', label: 'Languages', kind: 'codes', hint: 'Codes: ja, en, zh-Hant', not: ['season'] },
  { field: 'content_rating', label: 'Rating', kind: 'text', hint: 'PG-13, 12, TV-MA', not: ['season'] },
  { field: 'tags', label: 'Tags', kind: 'list', hint: 'Your own labels, comma-separated' },
  { field: 'links', label: 'Links', kind: 'links', hint: 'One address a line' },
];

/** A field's value as the form edits it. */
function toText(spec: Spec, v: unknown): string {
  if (v === null || v === undefined) return '';
  switch (spec.kind) {
    case 'list':
    case 'codes':
      return (v as string[]).join(', ');
    case 'cast':
      return (v as CastMember[]).map((c) => (c.role ? `${c.name} as ${c.role}` : c.name)).join('\n');
    case 'links':
      return (v as Link[]).map((l) => l.url).join('\n');
    default:
      return typeof v === 'string' || typeof v === 'number' ? String(v) : JSON.stringify(v);
  }
}

/** The form's text as the value `items.edit` takes; `null` when empty. */
function fromText(spec: Spec, t: string): unknown {
  const text = t.trim();
  if (!text) return null;
  switch (spec.kind) {
    case 'year':
      return Number(text);
    case 'list':
    case 'codes':
      return text
        .split(',')
        .map((s) => s.trim())
        .filter(Boolean);
    case 'cast':
      return text
        .split('\n')
        .map((l) => l.trim())
        .filter(Boolean)
        .map((l) => {
          const m = / as /i.exec(l);
          return m ? { name: l.slice(0, m.index).trim(), role: l.slice(m.index + m[0].length).trim() || null } : { name: l, role: null };
        });
    case 'links':
      return text
        .split('\n')
        .map((l) => l.trim())
        .filter(Boolean)
        .map((url) => ({ url, title: null }));
    default:
      return text;
  }
}

const same = (a: unknown, b: unknown) => JSON.stringify(a ?? null) === JSON.stringify(b ?? null);

export function EditDetails({ id, kind, facts, propose, onDone }: { id: Id; kind: ItemKind; facts: AgentItemDetail; propose: boolean; onDone: (note: string) => void }) {
  const specs = useMemo(() => SPECS.filter((s) => (!s.only || s.only.includes(kind)) && !s.not?.includes(kind)), [kind]);
  const start = useMemo(() => Object.fromEntries(specs.map((s) => [s.field, toText(s, facts.fields[s.field])])), [specs, facts]);
  const [form, setForm] = useState<Record<string, string>>(start);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [refused, setRefused] = useState<string[]>([]);
  const withheld = facts.withheld;

  const save = () => {
    const set: Record<string, unknown> = {};
    const clear: string[] = [];
    for (const s of specs) {
      const before = facts.fields[s.field];
      const after = fromText(s, form[s.field] ?? '');
      if (same(fromText(s, start[s.field] ?? ''), after)) continue;
      if (after === null) {
        if (before !== null && before !== undefined) clear.push(s.field);
      } else set[s.field] = after;
    }
    if (Object.keys(set).length === 0 && clear.length === 0) {
      onDone('Nothing changed.');
      return;
    }
    setBusy(true);
    setErr(null);
    op<EditOutput>('items.edit', {
      summary: `Edited ${facts.title} in the app`,
      edits: [{ item: id, set, clear }],
      mode: propose ? 'propose' : 'apply',
    })
      .then((out) => {
        const r = out.items[0];
        const n = r?.changed.length ?? 0;
        setRefused((r?.refused ?? []).map((x) => `${x.field}: ${x.reason}`));
        invalidateQueries(keys.facts(id));
        invalidateQueries(keys.item(id));
        invalidateQueries(keys.changes);
        if (!r?.refused.length) {
          onDone(n === 0 ? 'Nothing changed.' : propose ? `Proposed ${n} change${n === 1 ? '' : 's'} for an Owner or Admin.` : `Saved ${n} change${n === 1 ? '' : 's'}. Undo it in Agents & changes.`);
        }
      })
      .catch((e: unknown) => setErr(e instanceof OpFailure || e instanceof Error ? e.message : 'That didn’t save.'))
      .finally(() => setBusy(false));
  };

  return (
    <form
      className={about.edit}
      onSubmit={(e) => {
        e.preventDefault();
        save();
      }}
    >
      {specs.map((s) => {
        const value = form[s.field] ?? '';
        const set = (v: string) => setForm((f) => ({ ...f, [s.field]: v }));
        const fid = `edit-${s.field}`;
        const long = s.kind === 'long' || s.kind === 'cast' || s.kind === 'links';
        return (
          <label key={s.field} className={about.editRow} htmlFor={fid} data-long={long ? '' : undefined}>
            <span className={about.editLabel}>
              {s.label}
              {withheld[s.field] && <small> · {withheld[s.field].toUpperCase()}’s</small>}
            </span>
            {long ? (
              <textarea id={fid} value={value} rows={s.kind === 'long' ? 5 : 3} onChange={(e) => set(e.target.value)} placeholder={s.hint} />
            ) : (
              <input
                id={fid}
                value={value}
                inputMode={s.kind === 'year' ? 'numeric' : undefined}
                onChange={(e) => set(e.target.value)}
                placeholder={s.hint}
                autoComplete="off"
                spellCheck={s.kind === 'text' || s.kind === 'list'}
              />
            )}
          </label>
        );
      })}
      <p className={about.hint}>
        What you set here is yours: flux’s matching and scans leave it alone, and an agent changes it only if you ask. Leave a field empty to hand it back to flux.
      </p>
      {refused.length > 0 && (
        <ul className={about.refused} role="alert">
          {refused.map((r) => (
            <li key={r}>{r}</li>
          ))}
        </ul>
      )}
      {err && (
        <p className={about.errorNote} role="alert">
          {err}
        </p>
      )}
      <div className={about.editActions}>
        <button type="submit" className="btn btn-sm btn-primary" disabled={busy}>
          <Icon name="check" />
          {propose ? 'Propose' : 'Save'}
        </button>
        <button type="button" className="btn btn-sm btn-ghost" disabled={busy} onClick={() => onDone('')}>
          Cancel
        </button>
      </div>
    </form>
  );
}
